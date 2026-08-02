//! Durable, SSRF-resistant HTTP media downloader backed by the SQLite media queue.

use std::{
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use reqwest::{StatusCode, Url, header::LOCATION, redirect::Policy};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use tokio::{io::AsyncWriteExt, sync::watch, task::JoinHandle};
use tracing::warn;

use crate::{
    error::{Error, Result},
    storage::internal::entities::{
        MediaClaimDto, MediaClaimRequest, MediaDownloadCompletion, claim_next_media,
        complete_media_download, fail_media_download, recover_downloading_media,
    },
};

const MAX_RETRIES: i64 = 5;
const PREVIEW_TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Debug, Clone)]
pub struct MediaPipelineConfig {
    pub media_root: PathBuf,
    pub max_bytes: u64,
    pub poll_interval: Duration,
    /// Test-only escape hatch for local HTTP mock servers. Production keeps this false.
    pub allow_http: bool,
    /// Test-only escape hatch for loopback/private mock servers. Production keeps this false.
    pub allow_private_network: bool,
    pub max_redirects: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaRecoverySummary {
    pub downloading_requeued: u64,
    pub parts_removed: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaWorkerSummary {
    pub stopped: bool,
    pub join_failed: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedMediaSource {
    Local(PathBuf),
    Remote(String),
    Unavailable,
}

/// A bounded in-memory preview fetched through the same SSRF controls as downloads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaPreview {
    pub content_type: String,
    pub bytes: Vec<u8>,
}

#[async_trait]
pub trait MediaHostResolver: Send + Sync {
    async fn resolve(&self, host: &str, port: u16) -> Result<Vec<IpAddr>>;
}

struct SystemMediaHostResolver;

#[async_trait]
impl MediaHostResolver for SystemMediaHostResolver {
    async fn resolve(&self, host: &str, port: u16) -> Result<Vec<IpAddr>> {
        let addresses = tokio::net::lookup_host((host, port))
            .await
            .map_err(|error| Error::FormatError(format!("media DNS lookup failed: {error}")))?
            .map(|address| address.ip())
            .collect::<Vec<_>>();
        if addresses.is_empty() {
            return Err(Error::FormatError(
                "media host resolved no addresses".into(),
            ));
        }
        Ok(addresses)
    }
}

#[derive(Clone)]
pub struct MediaPipeline {
    pool: SqlitePool,
    config: MediaPipelineConfig,
    resolver: Arc<dyn MediaHostResolver>,
}

impl MediaPipeline {
    pub fn new(pool: SqlitePool, _client: reqwest::Client, config: MediaPipelineConfig) -> Self {
        Self::new_with_resolver(pool, _client, config, Arc::new(SystemMediaHostResolver))
    }

    pub fn new_with_resolver(
        pool: SqlitePool,
        _client: reqwest::Client,
        config: MediaPipelineConfig,
        resolver: Arc<dyn MediaHostResolver>,
    ) -> Self {
        Self {
            pool,
            config,
            resolver,
        }
    }

    pub async fn validate_url(&self, raw_url: &str) -> Result<Url> {
        self.validate_and_resolve_url(raw_url)
            .await
            .map(|(url, _)| url)
    }

    /// Fetches a small preview without writing a file or changing media queue state.
    pub async fn fetch_preview(&self, raw_url: &str, media_type: &str) -> Result<MediaPreview> {
        tokio::time::timeout(
            PREVIEW_TIMEOUT,
            self.fetch_preview_inner(raw_url, media_type),
        )
        .await
        .map_err(|_| Error::FormatError("media preview timed out".into()))?
    }

    async fn fetch_preview_inner(&self, raw_url: &str, media_type: &str) -> Result<MediaPreview> {
        let (mut response, _) = self.send_with_validated_redirects(raw_url).await?;
        if response
            .content_length()
            .is_some_and(|length| length == 0 || length > preview_max_bytes(media_type))
        {
            return Err(Error::FormatError("media preview size is invalid".into()));
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(normalize_preview_content_type)
            .filter(|content_type| is_preview_content_type(content_type, media_type))
            .ok_or_else(|| Error::FormatError("media preview has invalid content type".into()))?;
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            if bytes.len().saturating_add(chunk.len()) > preview_max_bytes(media_type) as usize {
                return Err(Error::FormatError(
                    "media preview exceeds size limit".into(),
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        if bytes.is_empty() {
            return Err(Error::FormatError("media preview is empty".into()));
        }
        if media_type != "video" {
            image::load_from_memory(&bytes)
                .map_err(|error| Error::FormatError(format!("invalid preview image: {error}")))?;
        }
        Ok(MediaPreview {
            content_type,
            bytes,
        })
    }

    async fn validate_and_resolve_url(&self, raw_url: &str) -> Result<(Url, Vec<IpAddr>)> {
        let url = Url::parse(raw_url)?;
        if url.scheme() != "https" && !(self.config.allow_http && url.scheme() == "http") {
            return Err(Error::FormatError("media URL must use HTTPS".into()));
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(Error::FormatError(
                "media URL must not contain userinfo".into(),
            ));
        }
        if url.fragment().is_some() || url.query_pairs().any(|(key, _)| is_sensitive_url_key(&key))
        {
            return Err(Error::FormatError(
                "media URL must not contain sensitive credentials".into(),
            ));
        }
        let host = url
            .host_str()
            .ok_or_else(|| Error::FormatError("media URL has no host".into()))?;
        if host.eq_ignore_ascii_case("localhost") || host.ends_with(".localhost") {
            return Err(Error::FormatError(
                "localhost media URL is forbidden".into(),
            ));
        }
        let port = url
            .port_or_known_default()
            .ok_or_else(|| Error::FormatError("media URL has no usable port".into()))?;
        let addresses = match url.host() {
            Some(url::Host::Ipv4(_)) | Some(url::Host::Ipv6(_))
                if !self.config.allow_private_network =>
            {
                return Err(Error::FormatError("literal media host is forbidden".into()));
            }
            Some(url::Host::Ipv4(address)) => vec![IpAddr::V4(address)],
            Some(url::Host::Ipv6(address)) => vec![IpAddr::V6(address)],
            Some(url::Host::Domain(domain)) => self.resolver.resolve(domain, port).await?,
            None => return Err(Error::FormatError("media URL has no host".into())),
        };
        if !self.config.allow_private_network
            && addresses.iter().any(|address| !is_public(*address))
        {
            return Err(Error::FormatError(
                "media host resolved to a non-public address".into(),
            ));
        }
        Ok((url, addresses))
    }

    pub async fn recover_startup(&self) -> Result<MediaRecoverySummary> {
        tokio::fs::create_dir_all(&self.config.media_root).await?;
        let updated_at = chrono::Utc::now().to_rfc3339();
        let downloading_requeued = recover_downloading_media(&self.pool, &updated_at).await?;
        let root = self.config.media_root.clone();
        let parts_removed = tokio::task::spawn_blocking(move || remove_part_files(&root))
            .await
            .map_err(|error| Error::Tokio(error.to_string()))??;
        Ok(MediaRecoverySummary {
            downloading_requeued,
            parts_removed,
        })
    }

    /// Processes at most one item. Download failures are persisted and are not returned.
    pub async fn run_once(&self) -> Result<bool> {
        self.run_once_cancellable(None).await
    }

    async fn run_once_cancellable(
        &self,
        mut cancelled: Option<&mut watch::Receiver<bool>>,
    ) -> Result<bool> {
        let now = chrono::Utc::now();
        let token = uuid::Uuid::now_v7().to_string();
        let Some(claim) = claim_next_media(
            &self.pool,
            &MediaClaimRequest {
                token: token.clone(),
                now_epoch: now.timestamp(),
                claimed_at: now.to_rfc3339(),
            },
        )
        .await?
        else {
            return Ok(false);
        };

        let result = if let Some(cancelled) = cancelled.as_mut() {
            tokio::select! {
                biased;
                changed = cancelled.changed() => {
                    let _ = changed;
                    return Ok(true);
                }
                result = self.download(&claim) => result,
            }
        } else {
            self.download(&claim).await
        };
        if let Err(error) = result {
            let now = chrono::Utc::now();
            fail_media_download(
                &self.pool,
                claim.id,
                &token,
                now.timestamp(),
                &error.to_string(),
                &now.to_rfc3339(),
            )
            .await?;
        }
        Ok(true)
    }

    async fn download(&self, claim: &MediaClaimDto) -> Result<()> {
        if self.repair_from_existing_final(claim).await? {
            return Ok(());
        }
        let (mut response, _final_url) = self.send_with_validated_redirects(&claim.url).await?;
        if response
            .content_length()
            .is_some_and(|length| length > self.config.max_bytes)
        {
            return Err(Error::FormatError(
                "media exceeds configured maximum size".into(),
            ));
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let relative = deterministic_relative_path(
            claim.id,
            &claim.url,
            &claim.media_type,
            content_type.as_deref(),
        );
        let final_path = self.config.media_root.join(&relative);
        let part_path = part_path(&final_path);
        if let Some(parent) = final_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let mut file = tokio::fs::File::create(&part_path).await?;
        let mut bytes = 0_u64;
        let result: Result<()> = async {
            while let Some(chunk) = response.chunk().await? {
                bytes = bytes.saturating_add(chunk.len() as u64);
                if bytes > self.config.max_bytes {
                    return Err(Error::FormatError(
                        "media exceeds configured maximum size".into(),
                    ));
                }
                file.write_all(&chunk).await?;
            }
            file.sync_all().await?;
            drop(file);
            validate_media_file(&part_path, &claim.media_type, self.config.max_bytes).await?;
            if tokio::fs::try_exists(&final_path).await? {
                if validate_media_file(&final_path, &claim.media_type, self.config.max_bytes)
                    .await
                    .is_ok()
                {
                    tokio::fs::remove_file(&part_path).await?;
                    return Ok(());
                }
                tokio::fs::remove_file(&final_path).await?;
            }
            tokio::fs::rename(&part_path, &final_path).await?;
            Ok(())
        }
        .await;
        if let Err(error) = result {
            let _ = tokio::fs::remove_file(&part_path).await;
            return Err(error);
        }

        self.complete_claim_from_file(claim, &relative, content_type)
            .await
    }

    async fn send_with_validated_redirects(
        &self,
        raw_url: &str,
    ) -> Result<(reqwest::Response, Url)> {
        let mut current = raw_url.to_string();
        for redirects in 0..=self.config.max_redirects {
            let (url, addresses) = self.validate_and_resolve_url(&current).await?;
            let host = url
                .host_str()
                .ok_or_else(|| Error::FormatError("media URL has no host".into()))?;
            let port = url
                .port_or_known_default()
                .ok_or_else(|| Error::FormatError("media URL has no usable port".into()))?;
            let sockets = addresses
                .into_iter()
                .map(|address| SocketAddr::new(address, port))
                .collect::<Vec<_>>();
            let client = reqwest::Client::builder()
                .redirect(Policy::none())
                .no_proxy()
                .resolve_to_addrs(host, &sockets)
                .build()?;
            let response = client.get(url.clone()).send().await?;
            if !response.status().is_redirection() {
                return Ok((response.error_for_status()?, url));
            }
            if redirects == self.config.max_redirects {
                return Err(Error::FormatError("media redirect limit exceeded".into()));
            }
            if !matches!(
                response.status(),
                StatusCode::MOVED_PERMANENTLY
                    | StatusCode::FOUND
                    | StatusCode::SEE_OTHER
                    | StatusCode::TEMPORARY_REDIRECT
                    | StatusCode::PERMANENT_REDIRECT
            ) {
                return Err(Error::FormatError("unsupported media redirect".into()));
            }
            let location = response
                .headers()
                .get(LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| Error::FormatError("media redirect has no valid location".into()))?;
            current = url.join(location)?.to_string();
        }
        Err(Error::FormatError("media redirect limit exceeded".into()))
    }

    async fn repair_from_existing_final(&self, claim: &MediaClaimDto) -> Result<bool> {
        let directory = if claim.media_type == "video" {
            "videos"
        } else {
            "pictures"
        };
        let prefix = deterministic_file_prefix(claim.id, &claim.url);
        let parent = self.config.media_root.join(directory);
        let mut entries = match tokio::fs::read_dir(&parent).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with(&prefix)
                && !name.ends_with(".part")
                && path.is_file()
                && validate_media_file(&path, &claim.media_type, self.config.max_bytes)
                    .await
                    .is_ok()
            {
                let relative = path
                    .strip_prefix(&self.config.media_root)
                    .map_err(|_| Error::FormatError("existing media escaped media root".into()))?;
                self.complete_claim_from_file(claim, relative, content_type_for_path(&path))
                    .await?;
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn complete_claim_from_file(
        &self,
        claim: &MediaClaimDto,
        relative: &Path,
        content_type: Option<String>,
    ) -> Result<()> {
        let final_path = self.config.media_root.join(relative);
        let bytes = tokio::fs::metadata(&final_path).await?.len();
        let completed = complete_media_download(
            &self.pool,
            claim.id,
            &claim.claim_token,
            &MediaDownloadCompletion {
                local_path: path_for_database(relative),
                content_type,
                content_length: i64::try_from(bytes).unwrap_or(i64::MAX),
                updated_at: chrono::Utc::now().to_rfc3339(),
            },
        )
        .await?;
        if !completed {
            return Err(Error::InconsistentTask("media claim became stale".into()));
        }
        Ok(())
    }

    async fn run_until_cancelled(&self, mut cancelled: watch::Receiver<bool>) -> Result<()> {
        loop {
            if *cancelled.borrow() {
                return Ok(());
            }
            if !self.run_once_cancellable(Some(&mut cancelled)).await? {
                tokio::select! {
                    changed = cancelled.changed() => {
                        let _ = changed;
                    }
                    _ = tokio::time::sleep(self.config.poll_interval) => {}
                }
            }
        }
    }
}

fn is_sensitive_url_key(key: &str) -> bool {
    let normalized = key
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase();
    matches!(
        normalized.as_str(),
        "accesstoken"
            | "auth"
            | "authorization"
            | "cookie"
            | "credential"
            | "gsid"
            | "passport"
            | "password"
            | "secret"
            | "session"
            | "sub"
            | "token"
            | "xsrf"
    ) || [
        "auth",
        "cookie",
        "credential",
        "gsid",
        "passport",
        "password",
        "secret",
        "session",
        "signature",
        "token",
        "xsrf",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

pub struct MediaWorkerTask {
    cancelled: watch::Sender<bool>,
    handle: JoinHandle<Result<()>>,
}

impl MediaWorkerTask {
    pub fn spawn(pipeline: MediaPipeline) -> Self {
        let (cancelled, receiver) = watch::channel(false);
        let handle = tokio::spawn(async move {
            supervise_pipeline(pipeline, receiver).await;
            Ok(())
        });
        Self { cancelled, handle }
    }

    pub fn is_finished(&self) -> bool {
        self.handle.is_finished()
    }

    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub async fn abort_for_test(self) {
        self.handle.abort();
        let _ = self.handle.await;
    }

    pub async fn shutdown(mut self, timeout: Duration) -> MediaWorkerSummary {
        let _ = self.cancelled.send(true);
        match tokio::time::timeout(timeout, &mut self.handle).await {
            Ok(result) => worker_join_summary(result),
            Err(_) => {
                self.handle.abort();
                let result = self.handle.await;
                match result {
                    Err(error) if error.is_cancelled() => MediaWorkerSummary {
                        stopped: true,
                        join_failed: None,
                    },
                    other => worker_join_summary(other),
                }
            }
        }
    }
}

async fn supervise_pipeline(pipeline: MediaPipeline, mut cancelled: watch::Receiver<bool>) {
    let mut failures = 0_u32;
    loop {
        if *cancelled.borrow() {
            return;
        }
        let result = match pipeline.recover_startup().await {
            Ok(_) => pipeline.run_until_cancelled(cancelled.clone()).await,
            Err(error) => Err(error),
        };
        if *cancelled.borrow() {
            return;
        }
        failures = failures.saturating_add(1);
        let delay = pipeline
            .config
            .poll_interval
            .max(Duration::from_millis(10))
            .saturating_mul(1_u32 << failures.min(6));
        match result {
            Err(error) => warn!(error = %error, ?delay, "media worker failed; retrying"),
            Ok(()) => warn!(?delay, "media worker stopped unexpectedly; retrying"),
        }
        tokio::select! {
            changed = cancelled.changed() => {
                let _ = changed;
            }
            _ = tokio::time::sleep(delay.min(Duration::from_secs(30))) => {}
        }
    }
}

fn worker_join_summary(
    result: std::result::Result<Result<()>, tokio::task::JoinError>,
) -> MediaWorkerSummary {
    match result {
        Ok(Ok(())) => MediaWorkerSummary {
            stopped: true,
            join_failed: None,
        },
        Ok(Err(error)) => MediaWorkerSummary {
            stopped: false,
            join_failed: Some(error.to_string()),
        },
        Err(error) => MediaWorkerSummary {
            stopped: false,
            join_failed: Some(error.to_string()),
        },
    }
}

async fn validate_media_file(path: &Path, media_type: &str, max_bytes: u64) -> Result<()> {
    let metadata = tokio::fs::metadata(path).await?;
    if metadata.len() == 0 || metadata.len() > max_bytes {
        return Err(Error::FormatError("media file size is invalid".into()));
    }
    if media_type != "video" {
        let validation_path = path.to_path_buf();
        tokio::task::spawn_blocking(move || {
            image::io::Reader::open(validation_path)?
                .with_guessed_format()?
                .decode()
                .map(|_| ())
        })
        .await
        .map_err(|error| Error::Tokio(error.to_string()))?
        .map_err(|error| Error::FormatError(format!("invalid image: {error}")))?;
    } else if !matches!(
        path.extension().and_then(|value| value.to_str()),
        Some("mp4" | "mov" | "webm")
    ) {
        return Err(Error::FormatError("invalid video extension".into()));
    }
    Ok(())
}

fn is_public(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => {
            let octets = address.octets();
            !matches!(
                octets,
                [0, ..]
                    | [10, ..]
                    | [100, 64..=127, ..]
                    | [127, ..]
                    | [169, 254, ..]
                    | [172, 16..=31, ..]
                    | [192, 0, 0, ..]
                    | [192, 0, 2, ..]
                    | [192, 168, ..]
                    | [198, 18..=19, ..]
                    | [198, 51, 100, ..]
                    | [203, 0, 113, ..]
                    | [224..=255, ..]
            )
        }
        IpAddr::V6(address) => {
            if let Some(mapped) = address.to_ipv4_mapped() {
                return is_public(IpAddr::V4(mapped));
            }
            let segments = address.segments();
            !(address.is_loopback()
                || address.is_unspecified()
                || address.is_multicast()
                || address.is_unique_local()
                || address.is_unicast_link_local()
                || (segments[0] == 0x2001 && segments[1] == 0x0db8)
                || (segments[0] & 0xffc0) == 0xfec0)
        }
    }
}

fn preview_max_bytes(media_type: &str) -> u64 {
    let _ = media_type;
    8 * 1024 * 1024
}

fn normalize_preview_content_type(value: &str) -> Option<String> {
    let value = value.split(';').next()?.trim().to_ascii_lowercase();
    (!value.is_empty() && value.len() <= 128).then_some(value)
}

fn is_preview_content_type(content_type: &str, media_type: &str) -> bool {
    if media_type == "video" {
        matches!(content_type, "video/mp4" | "video/webm" | "video/quicktime")
    } else {
        matches!(
            content_type,
            "image/jpeg" | "image/png" | "image/gif" | "image/webp"
        )
    }
}

fn deterministic_file_prefix(id: i64, url: &str) -> String {
    format!("{id}-{:016x}.", url_hash(url))
}

fn deterministic_relative_path(
    id: i64,
    url: &str,
    media_type: &str,
    content_type: Option<&str>,
) -> PathBuf {
    let extension = match content_type
        .unwrap_or_default()
        .split(';')
        .next()
        .unwrap_or_default()
    {
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "video/webm" => "webm",
        "video/quicktime" => "mov",
        value if value.starts_with("video/") => "mp4",
        _ if media_type == "video" => "mp4",
        _ => "jpg",
    };
    let directory = if media_type == "video" {
        "videos"
    } else {
        "pictures"
    };
    PathBuf::from(directory).join(format!("{}{extension}", deterministic_file_prefix(id, url)))
}

fn url_hash(url: &str) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in url.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn content_type_for_path(path: &Path) -> Option<String> {
    match path.extension()?.to_str()? {
        "png" => Some("image/png".into()),
        "gif" => Some("image/gif".into()),
        "webp" => Some("image/webp".into()),
        "jpg" | "jpeg" => Some("image/jpeg".into()),
        "webm" => Some("video/webm".into()),
        "mov" => Some("video/quicktime".into()),
        "mp4" => Some("video/mp4".into()),
        _ => None,
    }
}

fn part_path(final_path: &Path) -> PathBuf {
    let mut name = final_path.as_os_str().to_os_string();
    name.push(".part");
    PathBuf::from(name)
}

fn path_for_database(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn remove_part_files(root: &Path) -> Result<u64> {
    fn visit(path: &Path, removed: &mut u64) -> std::io::Result<()> {
        if !path.exists() {
            return Ok(());
        }
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                visit(&path, removed)?;
            } else if path
                .extension()
                .is_some_and(|extension| extension == "part")
            {
                std::fs::remove_file(path)?;
                *removed += 1;
            }
        }
        Ok(())
    }
    let mut removed = 0;
    visit(root, &mut removed)?;
    Ok(removed)
}

pub fn bounded_retry_count(value: i64) -> i64 {
    value.clamp(0, MAX_RETRIES)
}
