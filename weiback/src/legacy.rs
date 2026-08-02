//! Explicit, read-only inspection and transactional import of legacy databases.

use std::{
    fs,
    io::{self, Write},
    path::{Component, Path, PathBuf},
    sync::{Arc, Barrier},
};

use serde::{Deserialize, Serialize};
use sqlx::{Connection, Row, SqliteConnection, SqlitePool, sqlite::SqliteConnectOptions};

use crate::error::{Error, Result};

const LEGACY_NAMESPACE: &str = "weiback";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LegacySourceKind {
    RustV1,
    PythonV2,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LegacyDetection {
    pub kind: LegacySourceKind,
    pub db_path: PathBuf,
    pub schema_version: u32,
    pub media_root: PathBuf,
    pub picture_dir: Option<PathBuf>,
    pub video_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LegacyImportRequest {
    pub source_path: PathBuf,
    pub media_root: PathBuf,
    pub imports_dir: PathBuf,
    publish_failure_after_for_test: Option<usize>,
    commit_failure_for_test: bool,
    #[serde(skip)]
    fingerprint_gap_for_test: Option<(Arc<Barrier>, Arc<Barrier>)>,
}

impl LegacyImportRequest {
    pub fn new(source_path: PathBuf, media_root: PathBuf, imports_dir: PathBuf) -> Self {
        Self {
            source_path,
            media_root,
            imports_dir,
            publish_failure_after_for_test: None,
            commit_failure_for_test: false,
            fingerprint_gap_for_test: None,
        }
    }

    #[doc(hidden)]
    pub fn with_publish_failure_after_for_test(mut self, after: usize) -> Self {
        self.publish_failure_after_for_test = Some(after);
        self
    }

    #[doc(hidden)]
    pub fn with_commit_failure_for_test(mut self) -> Self {
        self.commit_failure_for_test = true;
        self
    }

    #[doc(hidden)]
    pub fn with_fingerprint_gap_for_test(
        mut self,
        reached: Arc<Barrier>,
        resume: Arc<Barrier>,
    ) -> Self {
        self.fingerprint_gap_for_test = Some((reached, resume));
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LegacyImportSummary {
    pub source: LegacyDetection,
    pub status: LegacyImportStatus,
    pub posts: u64,
    pub users: u64,
    pub media_copied: u64,
    pub media_pending: u64,
    pub rollback_backup: PathBuf,
    pub diagnostic_code: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LegacyImportStatus {
    Completed,
    AlreadyCompleted,
    PartialRecoverable,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ImportManifest {
    items: Vec<ManifestItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ManifestItem {
    url: String,
    media_type: String,
    staged_path: String,
    final_path: String,
}

pub fn detect_legacy_sources(data_root: &Path) -> Vec<LegacyDetection> {
    let db_path = data_root.join(LEGACY_NAMESPACE).join("weiback.db");
    let target = data_root.join("weiback-next/weiback.db");
    futures::executor::block_on(inspect_legacy_source(&db_path, &target))
        .ok()
        .into_iter()
        .collect()
}

/// Inspects an explicitly selected source. Classification requires an unambiguous table signature;
/// `user_version` is reported only as diagnostic metadata.
pub async fn inspect_legacy_source(
    source_path: &Path,
    current_db: &Path,
) -> Result<LegacyDetection> {
    let source = source_path
        .canonicalize()
        .map_err(|_| Error::FormatError("legacy source is unavailable".into()))?;
    if source
        == current_db
            .canonicalize()
            .unwrap_or_else(|_| current_db.to_path_buf())
    {
        return Err(Error::FormatError(
            "legacy source cannot be the current database".into(),
        ));
    }
    if !source.is_file() || read_sqlite_user_version(&source).is_none() {
        return Err(Error::FormatError(
            "legacy source is not a supported SQLite database".into(),
        ));
    }
    let options = SqliteConnectOptions::new()
        .filename(&source)
        .read_only(true);
    let mut conn = SqliteConnection::connect_with(&options).await?;
    let tables: Vec<String> =
        sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type='table'")
            .fetch_all(&mut conn)
            .await?;
    let rust = has_signature(
        &mut conn,
        &tables,
        &[
            ("posts", &["id", "text", "uid"]),
            ("users", &["id", "screen_name"]),
            ("picture", &["url", "path", "post_id"]),
            ("video", &["url", "path", "post_id"]),
        ],
    )
    .await?;
    let python = has_signature(
        &mut conn,
        &tables,
        &[
            ("weibo", &["id", "text", "uid"]),
            ("user", &["id", "screen_name"]),
            ("image", &["url", "post_id"]),
        ],
    )
    .await?;
    let kind = match (rust, python) {
        (true, false) => LegacySourceKind::RustV1,
        (false, true) => LegacySourceKind::PythonV2,
        _ => {
            return Err(Error::FormatError(
                "legacy source schema is unknown or ambiguous".into(),
            ));
        }
    };
    conn.close().await?;
    let root = source
        .parent()
        .ok_or_else(|| Error::FormatError("legacy source has no parent".into()))?
        .to_path_buf();
    let (picture, video) = match kind {
        LegacySourceKind::RustV1 => (Some(root.join("pictures")), Some(root.join("videos"))),
        LegacySourceKind::PythonV2 => (Some(root.join("images")), None),
    };
    Ok(LegacyDetection {
        kind,
        db_path: source,
        schema_version: read_sqlite_user_version(source_path).unwrap_or_default(),
        media_root: root,
        picture_dir: picture.filter(|p| p.is_dir()),
        video_dir: video.filter(|p| p.is_dir()),
    })
}

async fn has_signature(
    conn: &mut SqliteConnection,
    tables: &[String],
    required: &[(&str, &[&str])],
) -> Result<bool> {
    for (table, required_columns) in required {
        if !tables.iter().any(|known| known == table) {
            return Ok(false);
        }
        let actual_columns: Vec<String> = sqlx::query(&format!("PRAGMA table_info({table})"))
            .fetch_all(&mut *conn)
            .await?
            .into_iter()
            .map(|row| row.get(1))
            .collect();
        if !required_columns
            .iter()
            .all(|required| actual_columns.iter().any(|actual| actual == required))
        {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Imports into one `BEGIN IMMEDIATE` transaction after a consistent rollback snapshot. Source is
/// opened read-only and never attached to or modified by the target database.
pub async fn import_legacy_source(
    target: &SqlitePool,
    request: LegacyImportRequest,
) -> Result<LegacyImportSummary> {
    let current = target_db_path(target).await?;
    let source = inspect_legacy_source(&request.source_path, &current).await?;
    fs::create_dir_all(&request.imports_dir)?;
    recover_pending_import_batches(target, &request).await?;
    let mut source_conn = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(&source.db_path)
            .read_only(true),
    )
    .await?;
    sqlx::query("BEGIN").execute(&mut source_conn).await?;
    let fingerprint = source_snapshot_fingerprint(&mut source_conn, source.kind).await?;
    if let Some((reached, resume)) = &request.fingerprint_gap_for_test {
        wait_for_test_barrier(reached.clone()).await;
        wait_for_test_barrier(resume.clone()).await;
    }
    if import_is_completed(target, &source, &fingerprint).await? {
        let _ = sqlx::query("ROLLBACK").execute(&mut source_conn).await;
        source_conn.close().await?;
        return Ok(LegacyImportSummary {
            source,
            status: LegacyImportStatus::AlreadyCompleted,
            posts: 0,
            users: 0,
            media_copied: 0,
            media_pending: 0,
            rollback_backup: PathBuf::new(),
            diagnostic_code: None,
        });
    }
    let backup = request
        .imports_dir
        .join(format!("rollback-backup-{}.db", uuid::Uuid::now_v7()));
    let escaped = backup.to_string_lossy().replace('\'', "''");
    sqlx::query(&format!("VACUUM INTO '{escaped}'"))
        .execute(target)
        .await?;
    let staging = request
        .imports_dir
        .join(format!("import-media-{}", uuid::Uuid::now_v7()));
    fs::create_dir_all(&staging)?;
    import_inner(
        target,
        source_conn,
        &source,
        &request,
        &staging,
        &backup,
        &fingerprint,
    )
    .await
}

async fn wait_for_test_barrier(barrier: Arc<Barrier>) {
    tokio::task::spawn_blocking(move || barrier.wait())
        .await
        .expect("test barrier task completes");
}

async fn import_inner(
    target: &SqlitePool,
    mut source_conn: SqliteConnection,
    source: &LegacyDetection,
    request: &LegacyImportRequest,
    staging: &Path,
    backup: &Path,
    fingerprint: &str,
) -> Result<LegacyImportSummary> {
    let mut target_conn = target.acquire().await?;
    sqlx::query("BEGIN IMMEDIATE")
        .execute(&mut *target_conn)
        .await?;
    let import = async {
        let mut manifest = ImportManifest { items: Vec::new() };
        let (posts, users, copied, pending) = match source.kind {
            LegacySourceKind::RustV1 => {
                import_rust(
                    &mut source_conn,
                    &mut target_conn,
                    source,
                    staging,
                    &mut manifest,
                )
                .await?
            }
            LegacySourceKind::PythonV2 => {
                import_python(&mut source_conn, &mut target_conn, staging).await?
            }
        };
        save_manifest(staging, &manifest)?;
        record_pending_import(&mut target_conn, source, fingerprint, staging).await?;
        Ok::<_, Error>((posts, users, copied, pending, manifest))
    }
    .await;
    match import {
        Ok((posts, users, copied, pending, manifest)) => {
            if request.commit_failure_for_test {
                let _ = sqlx::query("ROLLBACK").execute(&mut *target_conn).await;
                let _ = sqlx::query("ROLLBACK").execute(&mut source_conn).await;
                let _ = fs::remove_dir_all(staging);
                return Err(Error::InconsistentTask("test commit failure".into()));
            }
            if let Err(error) = sqlx::query("COMMIT").execute(&mut *target_conn).await {
                let _ = fs::remove_dir_all(staging);
                return Err(error.into());
            }
            source_conn.close().await?;
            match publish_manifest(
                target,
                &request.media_root,
                staging,
                &manifest,
                request.publish_failure_after_for_test,
            )
            .await
            {
                Ok(()) => {
                    mark_manifest_completed(staging)?;
                    mark_import_completed(target, staging).await?;
                    let _ = fs::remove_dir_all(staging.join("files"));
                    Ok(LegacyImportSummary {
                        source: source.clone(),
                        status: LegacyImportStatus::Completed,
                        posts,
                        users,
                        media_copied: copied,
                        media_pending: pending,
                        rollback_backup: backup.to_path_buf(),
                        diagnostic_code: None,
                    })
                }
                Err(_error) => {
                    mark_import_partial(target, staging).await?;
                    Ok(LegacyImportSummary {
                        source: source.clone(),
                        status: LegacyImportStatus::PartialRecoverable,
                        posts,
                        users,
                        media_copied: copied,
                        media_pending: pending,
                        rollback_backup: backup.to_path_buf(),
                        diagnostic_code: Some("LEGACY_IMPORT_POSTCOMMIT_PUBLISH_FAILED".into()),
                    })
                }
            }
        }
        Err(error) => {
            let _ = sqlx::query("ROLLBACK").execute(&mut *target_conn).await;
            let _ = sqlx::query("ROLLBACK").execute(&mut source_conn).await;
            let _ = fs::remove_dir_all(staging);
            Err(error)
        }
    }
}

async fn import_rust(
    source: &mut SqliteConnection,
    target: &mut SqliteConnection,
    detection: &LegacyDetection,
    staging: &Path,
    manifest: &mut ImportManifest,
) -> Result<(u64, u64, u64, u64)> {
    let users = sqlx::query("SELECT id,screen_name FROM users")
        .fetch_all(&mut *source)
        .await?;
    for row in &users {
        sqlx::query("INSERT OR IGNORE INTO users(id,screen_name) VALUES(?,?)")
            .bind(row.get::<i64, _>(0))
            .bind(row.get::<Option<String>, _>(1))
            .execute(&mut *target)
            .await?;
    }
    let posts = sqlx::query("SELECT id,text,mblogid,uid,created_at,deleted,favorited,edit_count,attitudes_status FROM posts").fetch_all(&mut *source).await?;
    for row in &posts {
        sqlx::query("INSERT OR IGNORE INTO posts(id,text,mblogid,uid,created_at,deleted,favorited,edit_count,attitudes_status) VALUES(?,?,?,?,?,?,?,?,?)").bind(row.get::<i64,_>(0)).bind(row.get::<Option<String>,_>(1)).bind(row.get::<Option<String>,_>(2)).bind(row.get::<Option<i64>,_>(3)).bind(row.get::<Option<String>,_>(4)).bind(row.get::<Option<i64>,_>(5)).bind(row.get::<Option<i64>,_>(6)).bind(row.get::<Option<i64>,_>(7)).bind(row.get::<Option<i64>,_>(8)).execute(&mut *target).await?;
    }
    let media =
        sqlx::query("SELECT url,post_id,path,definition FROM picture WHERE url IS NOT NULL")
            .fetch_all(&mut *source)
            .await?;
    let mut copied = 0;
    let mut pending = 0;
    for row in media {
        let url: String = row.get(0);
        if url.is_empty() {
            return Err(Error::FormatError("legacy media URL is invalid".into()));
        };
        let post_id: Option<i64> = row.get(1);
        let path: Option<String> = row.get(2);
        let definition: Option<String> = row.get(3);
        let already_downloaded = media_is_downloaded(target, &url).await?;
        let staged = if already_downloaded {
            None
        } else {
            path.and_then(|path| {
                copy_legacy_media(
                    detection.picture_dir.as_deref(),
                    &path,
                    staging,
                    "pictures",
                    &url,
                    manifest,
                )
                .transpose()
            })
            .transpose()?
        };
        if staged.is_some() {
            copied += 1;
        } else if already_downloaded {
        } else {
            pending += 1;
        }
        upsert_import_media(target, &url, "picture", staging).await?;
        if let Some(post_id) = post_id {
            insert_media_reference(target, post_id, &url, definition.as_deref()).await?;
        }
    }
    let videos = sqlx::query("SELECT url,post_id,path FROM video WHERE url IS NOT NULL")
        .fetch_all(&mut *source)
        .await?;
    for row in videos {
        let url: String = row.get(0);
        if url.is_empty() {
            return Err(Error::FormatError("legacy media URL is invalid".into()));
        }
        let post_id: Option<i64> = row.get(1);
        let path: Option<String> = row.get(2);
        let already_downloaded = media_is_downloaded(target, &url).await?;
        let staged = if already_downloaded {
            None
        } else {
            path.and_then(|path| {
                copy_legacy_media(
                    detection.video_dir.as_deref(),
                    &path,
                    staging,
                    "videos",
                    &url,
                    manifest,
                )
                .transpose()
            })
            .transpose()?
        };
        if staged.is_some() {
            copied += 1;
        } else if already_downloaded {
        } else {
            pending += 1;
        }
        upsert_import_media(target, &url, "video", staging).await?;
        if let Some(post_id) = post_id {
            insert_media_reference(target, post_id, &url, None).await?;
        }
    }
    Ok((posts.len() as u64, users.len() as u64, copied, pending))
}

async fn media_is_downloaded(target: &mut SqliteConnection, url: &str) -> Result<bool> {
    Ok(
        sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM media WHERE url=? AND status='downloaded')",
        )
        .bind(url)
        .fetch_one(&mut *target)
        .await?,
    )
}

async fn insert_media_reference(
    target: &mut SqliteConnection,
    post_id: i64,
    url: &str,
    definition: Option<&str>,
) -> Result<()> {
    sqlx::query(
        "INSERT OR IGNORE INTO media_references(media_id,owner_type,owner_id,definition,created_at) \
         SELECT id,'post',?,?,'1970-01-01T00:00:00Z' FROM media \
         WHERE url=? AND NOT EXISTS( \
             SELECT 1 FROM media_references \
             WHERE media_id=media.id AND owner_type='post' AND owner_id=? AND definition!='' \
         )",
    )
    .bind(post_id)
    .bind(definition.unwrap_or(""))
    .bind(url)
    .bind(post_id)
    .execute(&mut *target)
    .await?;
    Ok(())
}

async fn import_python(
    source: &mut SqliteConnection,
    target: &mut SqliteConnection,
    staging: &Path,
) -> Result<(u64, u64, u64, u64)> {
    let users = sqlx::query("SELECT id,screen_name FROM user")
        .fetch_all(&mut *source)
        .await?;
    for row in &users {
        sqlx::query("INSERT OR IGNORE INTO users(id,screen_name) VALUES(?,?)")
            .bind(row.get::<i64, _>(0))
            .bind(row.get::<Option<String>, _>(1))
            .execute(&mut *target)
            .await?;
    }
    let posts = sqlx::query("SELECT id,text,mblogid,uid,created_at FROM weibo")
        .fetch_all(&mut *source)
        .await?;
    for row in &posts {
        sqlx::query(
            "INSERT OR IGNORE INTO posts(id,text,mblogid,uid,created_at) VALUES(?,?,?,?,?)",
        )
        .bind(row.get::<i64, _>(0))
        .bind(row.get::<Option<String>, _>(1))
        .bind(row.get::<Option<String>, _>(2))
        .bind(row.get::<Option<i64>, _>(3))
        .bind(row.get::<Option<String>, _>(4))
        .execute(&mut *target)
        .await?;
    }
    let media = sqlx::query("SELECT url,post_id FROM image WHERE url IS NOT NULL AND url != ''")
        .fetch_all(&mut *source)
        .await?;
    for row in &media {
        let url: String = row.get(0);
        let post_id: i64 = row.get(1);
        upsert_import_media(target, &url, "picture", staging).await?;
        insert_media_reference(target, post_id, &url, None).await?;
    }
    Ok((
        posts.len() as u64,
        users.len() as u64,
        0,
        media.len() as u64,
    ))
}

fn copy_legacy_media(
    root: Option<&Path>,
    raw: &str,
    staging: &Path,
    kind: &str,
    url: &str,
    manifest: &mut ImportManifest,
) -> Result<Option<String>> {
    let Some(root) = root else { return Ok(None) };
    let relative = Path::new(raw);
    if relative.is_absolute()
        || relative.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(Error::FormatError(
            "legacy media path escapes its root".into(),
        ));
    }
    let root = root.canonicalize()?;
    let source = root
        .join(relative)
        .canonicalize()
        .map_err(|_| Error::FormatError("legacy media path is unavailable".into()))?;
    if !source.is_file() || !source.starts_with(&root) {
        return Err(Error::FormatError(
            "legacy media path escapes its root".into(),
        ));
    }
    let extension = relative
        .extension()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or("bin");
    let final_path = format!("{kind}/{:016x}.{extension}", stable_hash(url));
    let staged_path = format!("files/{final_path}");
    let destination = staging.join(&staged_path);
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?
    };
    copy_new(&source, &destination)?;
    manifest.items.push(ManifestItem {
        url: url.into(),
        media_type: kind.trim_end_matches('s').into(),
        staged_path,
        final_path: final_path.clone(),
    });
    Ok(Some(final_path))
}

fn copy_new(source: &Path, destination: &Path) -> Result<()> {
    let mut input = fs::File::open(source)?;
    let mut output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    io::copy(&mut input, &mut output)?;
    output.flush()?;
    Ok(())
}

fn save_manifest(staging: &Path, manifest: &ImportManifest) -> Result<()> {
    let path = staging.join("manifest.json");
    let bytes = serde_json::to_vec_pretty(manifest)?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    Ok(())
}

fn load_manifest(staging: &Path) -> Result<ImportManifest> {
    Ok(serde_json::from_slice(&fs::read(
        staging.join("manifest.json"),
    )?)?)
}

fn mark_manifest_completed(staging: &Path) -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(staging.join("completed"))?;
    file.write_all(b"published\n")?;
    file.sync_all()?;
    Ok(())
}

async fn recover_pending_import_batches(
    target: &SqlitePool,
    request: &LegacyImportRequest,
) -> Result<()> {
    if !request.imports_dir.is_dir() {
        return Ok(());
    }
    let batches: Vec<String> = sqlx::query_scalar(
        "SELECT batch_dir FROM legacy_imports WHERE status IN ('pending_publish','partial_recoverable')",
    )
    .fetch_all(target)
    .await?;
    for batch in batches {
        let staging = request.imports_dir.join(batch);
        let outcome = (|| -> Result<()> {
            validate_batch_dir(&request.imports_dir, &staging)?;
            let manifest = load_manifest(&staging)?;
            validate_manifest(&request.media_root, &staging, &manifest)?;
            Ok(())
        })();
        if outcome.is_err() {
            continue;
        }
        let manifest = load_manifest(&staging)?;
        match publish_manifest(target, &request.media_root, &staging, &manifest, None).await {
            Ok(()) => {
                mark_manifest_completed(&staging)?;
                mark_import_completed(target, &staging).await?;
                let _ = fs::remove_dir_all(staging.join("files"));
            }
            Err(_) => mark_import_partial(target, &staging).await?,
        }
    }
    Ok(())
}

async fn publish_manifest(
    target: &SqlitePool,
    media_root: &Path,
    staging: &Path,
    manifest: &ImportManifest,
    fail_after: Option<usize>,
) -> Result<()> {
    validate_batch_dir(
        staging
            .parent()
            .ok_or_else(|| Error::FormatError("legacy import batch has no parent".into()))?,
        staging,
    )?;
    validate_manifest(media_root, staging, manifest)?;
    for (index, item) in manifest.items.iter().enumerate() {
        if fail_after == Some(index) {
            return Err(Error::InconsistentTask("test publish failure".into()));
        }
        let is_selected: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM media WHERE url=? AND media_type=? AND import_hold=1 AND import_batch=?)",
        )
        .bind(&item.url)
        .bind(&item.media_type)
        .bind(staging.file_name().and_then(|name| name.to_str()))
        .fetch_one(target)
        .await?;
        if !is_selected {
            continue;
        }
        let staged = safe_staged_file(staging, &item.staged_path)?;
        let destination = safe_media_destination(media_root, Path::new(&item.final_path))?;
        if staged.is_file() {
            match fs::hard_link(&staged, &destination) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::CrossesDevices => {
                    match copy_new(&staged, &destination) {
                        Ok(()) => {}
                        Err(Error::Io(error)) if error.kind() == io::ErrorKind::AlreadyExists => {
                            if fs::read(&staged)? != fs::read(&destination)? {
                                return Err(Error::FormatError(
                                    "legacy media destination already exists".into(),
                                ));
                            }
                        }
                        Err(error) => return Err(error),
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    if fs::read(&staged)? != fs::read(&destination)? {
                        return Err(Error::FormatError(
                            "legacy media destination already exists".into(),
                        ));
                    }
                }
                Err(error) => return Err(error.into()),
            }
        } else if !destination.is_file() {
            return Err(Error::FormatError(
                "legacy media staging file is unavailable".into(),
            ));
        }
        let bytes = i64::try_from(fs::metadata(&destination)?.len()).unwrap_or(i64::MAX);
        sqlx::query("UPDATE media SET status='downloaded',local_path=?,content_length=?,import_hold=0,import_batch=NULL,updated_at=? WHERE url=? AND status='pending' AND import_hold=1 AND import_batch=? AND local_path IS NULL AND media_type=?")
            .bind(&item.final_path)
            .bind(bytes)
            .bind(chrono::Utc::now().to_rfc3339())
            .bind(&item.url)
            .bind(staging.file_name().and_then(|name| name.to_str()))
            .bind(&item.media_type)
            .execute(target)
            .await?;
    }
    sqlx::query("UPDATE media SET status='pending',import_hold=0,import_batch=NULL,updated_at=? WHERE status='pending' AND import_hold=1 AND import_batch=? AND local_path IS NULL")
        .bind(chrono::Utc::now().to_rfc3339())
        .bind(staging.file_name().and_then(|name| name.to_str()))
        .execute(target)
        .await?;
    Ok(())
}

async fn upsert_import_media(
    target: &mut SqliteConnection,
    url: &str,
    media_type: &str,
    staging: &Path,
) -> Result<()> {
    let batch = staging
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| Error::FormatError("legacy import batch name is invalid".into()))?;
    sqlx::query(
        "INSERT INTO media(url,media_type,local_path,status,import_hold,import_batch,content_length,created_at) \
         VALUES(?,?,NULL,'pending',1,?,NULL,?) ON CONFLICT(url) DO UPDATE SET \
         media_type=CASE WHEN media.media_type='video' OR excluded.media_type!='video' THEN media.media_type ELSE 'video' END, \
         local_path=CASE WHEN media.media_type!='video' AND excluded.media_type='video' THEN NULL ELSE media.local_path END, \
         status=CASE WHEN media.media_type!='video' AND excluded.media_type='video' THEN 'pending' ELSE media.status END, \
         content_length=CASE WHEN media.media_type!='video' AND excluded.media_type='video' THEN NULL ELSE media.content_length END, \
         import_hold=CASE WHEN media.media_type='video' AND media.status='downloaded' THEN media.import_hold ELSE 1 END, \
         import_batch=CASE WHEN media.media_type='video' AND media.status='downloaded' THEN media.import_batch ELSE excluded.import_batch END",
    )
    .bind(url)
    .bind(media_type)
    .bind(batch)
    .bind("1970-01-01T00:00:00Z")
    .execute(&mut *target)
    .await?;
    Ok(())
}

fn validate_batch_dir(imports_dir: &Path, staging: &Path) -> Result<()> {
    let name = staging
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| Error::FormatError("legacy import batch name is invalid".into()))?;
    let id = name
        .strip_prefix("import-media-")
        .ok_or_else(|| Error::FormatError("legacy import batch name is invalid".into()))?;
    uuid::Uuid::parse_str(id)
        .map_err(|_| Error::FormatError("legacy import batch name is invalid".into()))?;
    let imports = imports_dir.canonicalize()?;
    let canonical = staging.canonicalize()?;
    if !canonical.is_dir() || !canonical.starts_with(imports) {
        return Err(Error::FormatError(
            "legacy import batch escapes imports root".into(),
        ));
    }
    Ok(())
}

fn validate_manifest(media_root: &Path, staging: &Path, manifest: &ImportManifest) -> Result<()> {
    for item in &manifest.items {
        let _ = safe_staged_file(staging, &item.staged_path)?;
        let final_path = safe_relative_path(&item.final_path, "final")?;
        let _ = safe_media_destination(media_root, final_path)?;
    }
    Ok(())
}

fn safe_staged_file(staging: &Path, value: &str) -> Result<PathBuf> {
    let staged = safe_relative_path(value, "staged")?;
    if staged.components().next() != Some(Component::Normal("files".as_ref())) {
        return Err(Error::FormatError(
            "legacy manifest staged path is invalid".into(),
        ));
    }
    let staged = staging.join(staged).canonicalize()?;
    let files = staging.join("files").canonicalize()?;
    if !staged.is_file() || !staged.starts_with(files) {
        return Err(Error::FormatError(
            "legacy manifest staged path escapes batch".into(),
        ));
    }
    Ok(staged)
}

fn safe_relative_path<'a>(value: &'a str, field: &str) -> Result<&'a Path> {
    let path = Path::new(value);
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(Error::FormatError(format!(
            "legacy manifest {field} path is invalid"
        )));
    }
    Ok(path)
}

fn safe_media_destination(media_root: &Path, relative: &Path) -> Result<PathBuf> {
    fs::create_dir_all(media_root)?;
    let root = media_root.canonicalize()?;
    let mut current = root.clone();
    let mut components = relative.components().peekable();
    while let Some(Component::Normal(component)) = components.next() {
        if components.peek().is_some() {
            current.push(component);
            fs::create_dir_all(&current)?;
            current = current.canonicalize()?;
            if !current.starts_with(&root) {
                return Err(Error::FormatError(
                    "legacy manifest final path escapes media root".into(),
                ));
            }
        } else {
            current.push(component);
        }
    }
    if current.exists() && !current.canonicalize()?.starts_with(root) {
        return Err(Error::FormatError(
            "legacy manifest final path escapes media root".into(),
        ));
    }
    Ok(current)
}

async fn record_pending_import(
    target: &mut SqliteConnection,
    source: &LegacyDetection,
    fingerprint: &str,
    staging: &Path,
) -> Result<()> {
    let batch = staging
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| Error::FormatError("legacy import batch name is invalid".into()))?;
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query("INSERT INTO legacy_imports(source_path,snapshot_fingerprint,source_kind,status,batch_dir,created_at,updated_at) VALUES(?,?,?,'pending_publish',?,?,?)")
        .bind(source.db_path.to_string_lossy().as_ref())
        .bind(fingerprint)
        .bind(source_kind_name(source.kind))
        .bind(batch)
        .bind(&now)
        .bind(&now)
        .execute(&mut *target)
        .await?;
    Ok(())
}

async fn import_is_completed(
    target: &SqlitePool,
    source: &LegacyDetection,
    fingerprint: &str,
) -> Result<bool> {
    Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM legacy_imports WHERE source_path=? AND snapshot_fingerprint=? AND source_kind=? AND status='completed')")
        .bind(source.db_path.to_string_lossy().as_ref())
        .bind(fingerprint)
        .bind(source_kind_name(source.kind))
        .fetch_one(target)
        .await?)
}

async fn mark_import_partial(target: &SqlitePool, staging: &Path) -> Result<()> {
    update_import_status(target, staging, "partial_recoverable", false).await
}

async fn mark_import_completed(target: &SqlitePool, staging: &Path) -> Result<()> {
    update_import_status(target, staging, "completed", true).await
}

async fn update_import_status(
    target: &SqlitePool,
    staging: &Path,
    status: &str,
    completed: bool,
) -> Result<()> {
    let batch = staging
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| Error::FormatError("legacy import batch name is invalid".into()))?;
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query("UPDATE legacy_imports SET status=?,updated_at=?,completed_at=CASE WHEN ? THEN ? ELSE completed_at END WHERE batch_dir=?")
        .bind(status)
        .bind(&now)
        .bind(completed)
        .bind(&now)
        .bind(batch)
        .execute(target)
        .await?;
    Ok(())
}

fn source_kind_name(kind: LegacySourceKind) -> &'static str {
    match kind {
        LegacySourceKind::RustV1 => "rust_v1",
        LegacySourceKind::PythonV2 => "python_v2",
    }
}

async fn source_snapshot_fingerprint(
    source: &mut SqliteConnection,
    kind: LegacySourceKind,
) -> Result<String> {
    let mut bytes =
        format!("weiback-logical-snapshot-v1\0{}\0", source_kind_name(kind)).into_bytes();
    let tables: &[(&str, &str)] = match kind {
        LegacySourceKind::RustV1 => &[
            (
                "users",
                "SELECT quote(id),quote(screen_name) FROM users ORDER BY quote(id),quote(screen_name)",
            ),
            (
                "posts",
                "SELECT quote(id),quote(text),quote(mblogid),quote(uid),quote(created_at),quote(deleted),quote(favorited),quote(edit_count),quote(attitudes_status) FROM posts ORDER BY quote(id),quote(text),quote(mblogid),quote(uid),quote(created_at),quote(deleted),quote(favorited),quote(edit_count),quote(attitudes_status)",
            ),
            (
                "picture",
                "SELECT quote(id),quote(definition),quote(path),quote(post_id),quote(url),quote(user_id) FROM picture ORDER BY quote(id),quote(definition),quote(path),quote(post_id),quote(url),quote(user_id)",
            ),
            (
                "video",
                "SELECT quote(url),quote(path),quote(post_id) FROM video ORDER BY quote(url),quote(path),quote(post_id)",
            ),
        ],
        LegacySourceKind::PythonV2 => &[
            (
                "user",
                "SELECT quote(id),quote(screen_name) FROM user ORDER BY quote(id),quote(screen_name)",
            ),
            (
                "weibo",
                "SELECT quote(id),quote(text),quote(mblogid),quote(uid),quote(created_at) FROM weibo ORDER BY quote(id),quote(text),quote(mblogid),quote(uid),quote(created_at)",
            ),
            (
                "image",
                "SELECT quote(url),quote(post_id),quote(path) FROM image ORDER BY quote(url),quote(post_id),quote(path)",
            ),
        ],
    };
    for (table, query) in tables {
        bytes.extend_from_slice(b"table:");
        bytes.extend_from_slice(table.as_bytes());
        bytes.push(b'\n');
        let schema: Option<String> =
            sqlx::query_scalar("SELECT sql FROM sqlite_master WHERE type='table' AND name=?")
                .bind(table)
                .fetch_optional(&mut *source)
                .await?;
        append_fingerprint_value(&mut bytes, schema.as_deref().unwrap_or_default());
        bytes.push(b'\n');
        for row in sqlx::query(query).fetch_all(&mut *source).await? {
            bytes.extend_from_slice(b"row:");
            for index in 0..row.columns().len() {
                let value: String = row.get(index);
                append_fingerprint_value(&mut bytes, &value);
            }
            bytes.push(b'\n');
        }
    }
    Ok(format!(
        "sha256:{}",
        sha256(&bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    ))
}

fn append_fingerprint_value(bytes: &mut Vec<u8>, value: &str) {
    bytes.extend_from_slice(value.len().to_string().as_bytes());
    bytes.push(b':');
    bytes.extend_from_slice(value.as_bytes());
    bytes.push(b',');
}

fn sha256(input: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut message = input.to_vec();
    let bit_len = (message.len() as u64).wrapping_mul(8);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_be_bytes());
    let mut state = [
        0x6a09e667_u32,
        0xbb67ae85,
        0x3c6ef372,
        0xa54ff53a,
        0x510e527f,
        0x9b05688c,
        0x1f83d9ab,
        0x5be0cd19,
    ];
    for chunk in message.chunks_exact(64) {
        let mut words = [0_u32; 64];
        for (index, word) in words[..16].iter_mut().enumerate() {
            *word = u32::from_be_bytes(
                chunk[index * 4..index * 4 + 4]
                    .try_into()
                    .expect("chunk is 64 bytes"),
            );
        }
        for index in 16..64 {
            let s0 = words[index - 15].rotate_right(7)
                ^ words[index - 15].rotate_right(18)
                ^ (words[index - 15] >> 3);
            let s1 = words[index - 2].rotate_right(17)
                ^ words[index - 2].rotate_right(19)
                ^ (words[index - 2] >> 10);
            words[index] = words[index - 16]
                .wrapping_add(s0)
                .wrapping_add(words[index - 7])
                .wrapping_add(s1);
        }
        let mut working = state;
        for index in 0..64 {
            let s1 = working[4].rotate_right(6)
                ^ working[4].rotate_right(11)
                ^ working[4].rotate_right(25);
            let choose = (working[4] & working[5]) ^ ((!working[4]) & working[6]);
            let temp1 = working[7]
                .wrapping_add(s1)
                .wrapping_add(choose)
                .wrapping_add(K[index])
                .wrapping_add(words[index]);
            let s0 = working[0].rotate_right(2)
                ^ working[0].rotate_right(13)
                ^ working[0].rotate_right(22);
            let majority =
                (working[0] & working[1]) ^ (working[0] & working[2]) ^ (working[1] & working[2]);
            let temp2 = s0.wrapping_add(majority);
            working = [
                temp1.wrapping_add(temp2),
                working[0],
                working[1],
                working[2],
                working[3].wrapping_add(temp1),
                working[4],
                working[5],
                working[6],
            ];
        }
        for (value, update) in state.iter_mut().zip(working) {
            *value = value.wrapping_add(update);
        }
    }
    let mut result = [0_u8; 32];
    for (index, value) in state.iter().enumerate() {
        result[index * 4..index * 4 + 4].copy_from_slice(&value.to_be_bytes());
    }
    result
}

fn stable_hash(value: &str) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}
async fn target_db_path(pool: &SqlitePool) -> Result<PathBuf> {
    let row = sqlx::query("PRAGMA database_list").fetch_one(pool).await?;
    Ok(PathBuf::from(row.get::<String, _>(2)))
}
fn read_sqlite_user_version(path: &Path) -> Option<u32> {
    use std::io::Read;
    let mut file = fs::File::open(path).ok()?;
    let mut header = [0; 64];
    file.read_exact(&mut header).ok()?;
    (&header[..16] == b"SQLite format 3\0")
        .then(|| u32::from_be_bytes([header[60], header[61], header[62], header[63]]))
}
