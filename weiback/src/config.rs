//! This module manages the application's configuration.
//!
//! It handles loading configuration from files (or creating a default one if none exists),
//! saving configurations, and providing a globally accessible instance of the `Config` struct.
//! The configuration includes paths for the database, session, downloaded media,
//! task intervals, and SDK-specific settings.
use std::{
    fs,
    path::PathBuf,
    sync::{Arc, RwLock},
    time::Duration,
};

use once_cell::sync::OnceCell;
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info, warn};
use weibosdk_rs::config::Config as SdkConfig;

use crate::error::Result;
use crate::models::PictureDefinition;
use crate::storage::internal::entities::MEDIA_MAX_BYTES;

/// Global, lazily initialized instance of the application configuration.
///
/// It is wrapped in an `Arc<RwLock<Config>>` to allow safe concurrent
/// read/write access across multiple threads.
static CONFIG: OnceCell<Arc<RwLock<Config>>> = OnceCell::new();

pub const APP_NAMESPACE: &str = "weiback-next";

/// Runtime directory layout for the `weiback-next` namespace.
///
/// This is the single source of truth for the default per-user data directories
/// of the new application. The new app never writes into the legacy `weiback`
/// namespace; everything lives under `data/weiback-next`.
#[derive(Debug, Clone)]
pub struct RuntimeDirs {
    /// Root data directory: `<data_dir>/weiback-next`.
    pub data_dir: PathBuf,
    /// SQLite database file: `<data_dir>/weiback-next/weiback.db`.
    pub db_path: PathBuf,
    /// Unified media directory: `<data_dir>/weiback-next/media`.
    pub media_dir: PathBuf,
    /// Downloaded pictures: `<data_dir>/weiback-next/media/pictures`.
    pub pictures_dir: PathBuf,
    /// Downloaded videos: `<data_dir>/weiback-next/media/videos`.
    pub videos_dir: PathBuf,
    /// Application logs: `<data_dir>/weiback-next/logs`.
    pub logs_dir: PathBuf,
    /// Python collector sidecar: `<data_dir>/weiback-next/sidecar`.
    pub sidecar_dir: PathBuf,
    /// Playwright Chromium cache: `<data_dir>/weiback-next/chromium`.
    pub chromium_dir: PathBuf,
    /// One-time legacy snapshot import staging: `<data_dir>/weiback-next/imports`.
    pub imports_dir: PathBuf,
}

impl RuntimeDirs {
    /// Builds the layout from a data root (usually `dirs::data_dir()`).
    pub fn from_root(data_root: &std::path::Path) -> Self {
        let data_dir = data_root.join(APP_NAMESPACE);
        let media_dir = data_dir.join("media");
        Self {
            db_path: data_dir.join("weiback.db"),
            data_dir: data_dir.clone(),
            media_dir: media_dir.clone(),
            pictures_dir: media_dir.join("pictures"),
            videos_dir: media_dir.join("videos"),
            logs_dir: data_dir.join("logs"),
            sidecar_dir: data_dir.join("sidecar"),
            chromium_dir: data_dir.join("chromium"),
            imports_dir: data_dir.join("imports"),
        }
    }

    /// Creates every runtime directory. Returns an error if any creation fails.
    pub fn ensure_created(&self) -> std::io::Result<()> {
        for dir in [
            &self.data_dir,
            &self.media_dir,
            &self.pictures_dir,
            &self.videos_dir,
            &self.logs_dir,
            &self.sidecar_dir,
            &self.chromium_dir,
            &self.imports_dir,
        ] {
            std::fs::create_dir_all(dir)?;
        }
        Ok(())
    }
}

/// Returns the runtime directory layout based on the user's data directory.
pub fn runtime_dirs() -> RuntimeDirs {
    RuntimeDirs::from_root(&dirs::data_dir().unwrap_or_default())
}

/// Helper module for serializing/deserializing `std::time::Duration` as seconds.
mod duration_as_secs {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    /// Custom serialization for `Duration` to seconds (u64).
    pub fn serialize<S>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u64(duration.as_secs())
    }

    /// Custom deserialization for `Duration` from seconds (u64).
    pub fn deserialize<'de, D>(deserializer: D) -> Result<Duration, D::Error>
    where
        D: Deserializer<'de>,
    {
        let secs = u64::deserialize(deserializer)?;
        Ok(Duration::from_secs(secs))
    }
}

/// Represents the application's configuration settings.
///
/// This includes paths for data storage, download preferences, task intervals,
/// and SDK-specific configurations.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Path to the SQLite database file.
    pub db_path: PathBuf,
    /// Path to the session file for Weibo API authentication.
    pub session_path: PathBuf,
    /// Whether to download pictures associated with posts.
    pub download_pictures: bool,
    /// The preferred definition/size for downloaded pictures.
    pub picture_definition: PictureDefinition,
    /// Interval for background backup tasks.
    #[serde(with = "duration_as_secs")]
    pub backup_task_interval: Duration,
    /// Interval for other background tasks.
    #[serde(with = "duration_as_secs")]
    pub other_task_interval: Duration,
    /// Number of posts to include in each generated HTML file.
    pub posts_per_html: u32,
    /// Number of posts to fetch per API request (for both favorites and profile).
    pub posts_count: u32,
    /// Whether to generate static HTML pages without JavaScript interactions.
    pub static_html: bool,
    /// Base path for storing downloaded pictures.
    pub picture_path: PathBuf,
    /// Base path for storing downloaded videos.
    pub video_path: PathBuf,
    /// Unified root for the durable media pipeline.
    #[serde(default = "default_media_path")]
    pub media_path: PathBuf,
    /// Maximum accepted size for one durable media asset.
    pub media_max_bytes: u64,
    /// Idle polling interval for the durable media worker.
    #[serde(with = "duration_as_secs")]
    pub media_poll_interval: Duration,
    /// Configuration settings for the Weibo SDK.
    pub sdk_config: SdkConfig,
    /// Output directory for dev mode, if enabled.
    #[cfg(feature = "dev-mode")]
    pub dev_mode_out_dir: Option<PathBuf>,
}

impl Default for Config {
    /// Provides default configuration values.
    ///
    /// These defaults are typically based on platform-specific user directories
    /// (e.g., `dirs::config_dir`, `dirs::data_dir`).
    fn default() -> Self {
        Self::from_roots(
            &dirs::config_dir().unwrap_or_default(),
            &dirs::data_dir().unwrap_or_default(),
        )
    }
}

fn default_media_path() -> PathBuf {
    runtime_dirs().media_dir
}

impl Config {
    fn from_roots(config_root: &std::path::Path, data_root: &std::path::Path) -> Self {
        let config_dir = config_root.join(APP_NAMESPACE);
        let runtime = RuntimeDirs::from_root(data_root);
        Self {
            db_path: runtime.db_path,
            session_path: config_dir.join("session.json"),
            download_pictures: true,
            picture_definition: Default::default(),
            backup_task_interval: Duration::from_secs(3),
            other_task_interval: Duration::from_secs(1),
            posts_per_html: 200,
            posts_count: 20,
            static_html: false,
            picture_path: runtime.pictures_dir,
            video_path: runtime.videos_dir,
            media_path: runtime.media_dir,
            media_max_bytes: MEDIA_MAX_BYTES,
            media_poll_interval: Duration::from_secs(1),
            sdk_config: Default::default(),
            #[cfg(feature = "dev-mode")]
            dev_mode_out_dir: dirs::download_dir().map(|dir| dir.join("weiback-next-records")),
        }
    }
}

/// Explicit initialization function, which should be called at the start of the `main` function.
///
/// It attempts to load the configuration file. If the configuration file is not found
/// in any predefined path, it creates a default configuration and attempts to write it
/// to the user's local configuration directory.
///
/// # Errors
/// This function will return an error if:
/// - The configuration file exists but cannot be read or parsed.
/// - An I/O error occurs while trying to write a new default configuration file.
pub fn init() -> Result<()> {
    info!("Initializing config...");
    let config = load_or_create()?;
    let _ = CONFIG.set(Arc::new(RwLock::new(config)));
    info!("Config initialized successfully.");
    Ok(())
}

/// Forcefully initializes the global configuration with default values.
///
/// This is typically used as a fallback when `init()` fails, ensuring the
/// application can still function with in-memory defaults.
pub fn init_default() {
    let _ = CONFIG.set(Arc::new(RwLock::new(Config::default())));
}

/// Retrieves the global configuration instance.
///
/// This is a robust function that guarantees to always return a configuration instance.
/// - If `init()` has been successfully called, it will return the configuration set by `init()`.
/// - If `init()` has never been called, it will first attempt to load the configuration
///   from files (but will not create a new file). If loading fails for any reason,
///   it will fall back to an in-memory default configuration, ensuring the program
///   does not panic.
///
/// # Returns
/// An `Arc<RwLock<Config>>` providing shared, thread-safe access to the application's configuration.
pub fn get_config() -> Arc<RwLock<Config>> {
    CONFIG
        .get_or_init(|| {
            // "Implicit" initialization path: attempts to load, if fails (e.g., not found, permissions),
            // it uses the default value. This ensures get_config always returns successfully, without panicking.
            // It does not write to a file here to avoid uncontrollable I/O errors at runtime.
            warn!("Config not explicitly initialized, trying to load from files or use default.");
            let config = load_from_files()
                .unwrap_or_else(|e| {
                    warn!("Failed to load config from files, using default: {e}");
                    None
                })
                .unwrap_or_default();
            Arc::new(RwLock::new(config))
        })
        .clone()
}

/// Saves the current configuration to the appropriate configuration file.
///
/// If a configuration file already exists in one of the predefined paths, it will be
/// updated. Otherwise, a new default configuration file will be created in the
/// user's local configuration directory.
///
/// # Arguments
/// * `config` - A reference to the `Config` instance to save.
///
/// # Returns
/// A `Result` indicating success or an `Error` if saving fails (e.g., I/O errors, serialization errors).
pub fn save_config(config: &Config) -> Result<()> {
    let config_path = if let Some(path) = find_config_file()? {
        path
    } else {
        // from load_or_create
        let path = dirs::config_local_dir()
            .unwrap_or_default()
            .join(APP_NAMESPACE)
            .join("config.toml");
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).inspect_err(|e| {
                error!("create config parent directory {:?} failed: {e}", parent);
            })?;
        }
        path
    };

    fs::write(
        &config_path,
        toml::to_string_pretty(config).inspect_err(|e| {
            error!("serialize config to toml failed: {e}");
        })?,
    )
    .inspect_err(|e| {
        error!("write config file {:?} failed: {e}", config_path);
    })?;
    debug!("Configuration file saved at: {config_path:?}");

    if let Some(g_config) = CONFIG.get()
        && let Ok(mut g_config) = g_config.write()
    {
        *g_config = config.clone();
    }

    Ok(())
}

/// Attempts to load the configuration from all known predefined paths.
///
/// # Returns
/// A `Result` containing `Some(Config)` if a configuration file is found and successfully parsed,
/// `None` if no configuration file is found, or an `Error` if a file is found but cannot be read or parsed.
fn load_from_files() -> Result<Option<Config>> {
    let Some(config_path) = find_config_file()? else {
        return Ok(None);
    };
    let content = fs::read_to_string(config_path).inspect_err(|e| {
        error!("read config file failed: {e}");
    })?;
    let cfg = toml::from_str::<Config>(&content).inspect_err(|e| {
        error!("parse config file failed: {e}");
    })?;
    Ok(Some(cfg))
}

/// Attempts to load the configuration from a file. If no configuration file is found,
/// it creates a new default configuration and saves it to the user's local
/// configuration directory.
///
/// # Returns
/// A `Result` containing the loaded or newly created `Config` instance.
/// Returns an `Error` if loading fails or if writing the default config fails.
fn load_or_create() -> Result<Config> {
    if let Some(path) = find_config_file()? {
        let content = fs::read_to_string(path).inspect_err(|e| {
            error!("read config file failed: {e}");
        })?;
        return Ok(toml::from_str(&content).inspect_err(|e| {
            error!("parse config file failed: {e}");
        })?);
    }

    // No config file found, create and write default config
    let config = Config::default();
    let config_local_path = dirs::config_local_dir()
        .unwrap_or_default()
        .join(APP_NAMESPACE)
        .join("config.toml");

    if let Some(parent) = config_local_path.parent() {
        fs::create_dir_all(parent).inspect_err(|e| {
            error!("create config parent directory {:?} failed: {e}", parent);
        })?;
    }
    fs::write(
        &config_local_path,
        toml::to_string_pretty(&config).inspect_err(|e| {
            error!("serialize config to toml failed: {e}");
        })?,
    )
    .inspect_err(|e| error!("config file write failed: {e}"))?;
    debug!("Default configuration file created at: {config_local_path:?}",);

    Ok(config)
}

/// Searches for an existing configuration file in a set of predefined paths.
///
/// The search order is typically: user's local config directory, user's shared config directory,
/// and then an application-specific config path relative to the executable's directory.
///
/// # Returns
/// A `Result` containing `Some(PathBuf)` if a config file is found, or `None` otherwise.
/// Returns an `Error` if the current executable path cannot be determined.
fn find_config_file() -> Result<Option<PathBuf>> {
    let exe_path = std::env::current_exe().inspect_err(|e| {
        error!("get current executable path failed: {e}");
    })?;
    let exe_dir = exe_path.parent().unwrap_or(&exe_path);

    let paths = config_file_candidates(
        &dirs::config_local_dir().unwrap_or_default(),
        &dirs::config_dir().unwrap_or_default(),
        exe_dir,
    );

    Ok(paths.into_iter().find(|p| p.exists()))
}

fn config_file_candidates(
    config_local_root: &std::path::Path,
    config_root: &std::path::Path,
    exe_dir: &std::path::Path,
) -> [PathBuf; 3] {
    let relative_path = PathBuf::from(APP_NAMESPACE).join("config.toml");
    [
        config_local_root.join(&relative_path),
        config_root.join(&relative_path),
        exe_dir.join(relative_path),
    ]
}

#[cfg(test)]
mod local_tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn default_paths_use_the_next_namespace() {
        let config = Config::from_roots(Path::new("config-root"), Path::new("data-root"));

        assert_eq!(
            config.db_path,
            Path::new("data-root/weiback-next/weiback.db")
        );
        assert_eq!(
            config.session_path,
            Path::new("config-root/weiback-next/session.json")
        );
        assert_eq!(
            config.picture_path,
            Path::new("data-root/weiback-next/media/pictures")
        );
        assert_eq!(
            config.video_path,
            Path::new("data-root/weiback-next/media/videos")
        );
        assert_eq!(config.media_path, Path::new("data-root/weiback-next/media"));
    }

    #[test]
    fn config_roundtrip_preserves_unified_and_legacy_media_paths() {
        let mut config = Config::from_roots(Path::new("config-root"), Path::new("data-root"));
        config.media_path = PathBuf::from("custom/unified-media");
        config.picture_path = PathBuf::from("custom/legacy-pictures");
        config.video_path = PathBuf::from("custom/legacy-videos");

        let encoded = toml::to_string(&config).unwrap();
        let restored: Config = toml::from_str(&encoded).unwrap();

        assert_eq!(restored.media_path, Path::new("custom/unified-media"));
        assert_eq!(restored.picture_path, Path::new("custom/legacy-pictures"));
        assert_eq!(restored.video_path, Path::new("custom/legacy-videos"));
    }

    #[test]
    fn old_config_without_media_path_uses_current_namespace_media_root() {
        let mut config = Config::from_roots(Path::new("config-root"), Path::new("data-root"));
        config.picture_path = PathBuf::from("custom/pictures-only");
        config.video_path = PathBuf::from("custom/videos-only");
        let mut value = toml::Value::try_from(&config).unwrap();
        value.as_table_mut().unwrap().remove("media_path");

        let restored: Config = value.try_into().unwrap();

        assert_eq!(restored.media_path, runtime_dirs().media_dir);
        assert_eq!(restored.picture_path, Path::new("custom/pictures-only"));
        assert_eq!(restored.video_path, Path::new("custom/videos-only"));
    }

    #[test]
    fn config_candidates_never_include_the_legacy_namespace() {
        let candidates = config_file_candidates(
            Path::new("config-local"),
            Path::new("config"),
            Path::new("bin"),
        );

        assert_eq!(
            candidates,
            [
                PathBuf::from("config-local/weiback-next/config.toml"),
                PathBuf::from("config/weiback-next/config.toml"),
                PathBuf::from("bin/weiback-next/config.toml"),
            ]
        );
        assert!(
            candidates
                .iter()
                .all(|path| !path.to_string_lossy().contains("weiback/config.toml"))
        );
    }

    #[test]
    fn runtime_dirs_stay_inside_the_next_namespace() {
        let dirs = RuntimeDirs::from_root(Path::new("data-root"));

        assert_eq!(dirs.data_dir, PathBuf::from("data-root/weiback-next"));
        assert_eq!(
            dirs.db_path,
            PathBuf::from("data-root/weiback-next/weiback.db")
        );
        assert_eq!(
            dirs.media_dir,
            PathBuf::from("data-root/weiback-next/media")
        );
        assert_eq!(
            dirs.pictures_dir,
            PathBuf::from("data-root/weiback-next/media/pictures")
        );
        assert_eq!(
            dirs.videos_dir,
            PathBuf::from("data-root/weiback-next/media/videos")
        );
        assert_eq!(dirs.logs_dir, PathBuf::from("data-root/weiback-next/logs"));
        assert_eq!(
            dirs.sidecar_dir,
            PathBuf::from("data-root/weiback-next/sidecar")
        );
        assert_eq!(
            dirs.chromium_dir,
            PathBuf::from("data-root/weiback-next/chromium")
        );
        assert_eq!(
            dirs.imports_dir,
            PathBuf::from("data-root/weiback-next/imports")
        );

        // None of the runtime paths may touch the legacy namespace.
        for path in [
            &dirs.data_dir,
            &dirs.db_path,
            &dirs.media_dir,
            &dirs.pictures_dir,
            &dirs.videos_dir,
            &dirs.logs_dir,
            &dirs.sidecar_dir,
            &dirs.chromium_dir,
            &dirs.imports_dir,
        ] {
            let s = path.to_string_lossy();
            assert!(!s.contains("weiback/"), "path must not use legacy ns: {s}");
            assert!(s.contains("weiback-next"), "path must use new ns: {s}");
        }
    }

    #[test]
    fn ensure_created_builds_all_runtime_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = RuntimeDirs::from_root(tmp.path());

        dirs.ensure_created().unwrap();

        for path in [
            &dirs.data_dir,
            &dirs.media_dir,
            &dirs.pictures_dir,
            &dirs.videos_dir,
            &dirs.logs_dir,
            &dirs.sidecar_dir,
            &dirs.chromium_dir,
            &dirs.imports_dir,
        ] {
            assert!(path.is_dir(), "expected directory: {path:?}");
        }
    }
}
