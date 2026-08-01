//! One-time legacy installation detection.
//!
//! The `weiback-next` app must never read or modify the legacy `weiback`
//! namespace automatically. Detection is strictly read-only: it inspects the
//! filesystem and parses the SQLite file header *without opening a connection*,
//! so it never creates WAL/SHM files and never writes anything.

use std::{
    fs,
    path::{Path, PathBuf},
};

use serde::Serialize;

/// The legacy namespace root relative to the user data directory.
const LEGACY_NAMESPACE: &str = "weiback";

/// Which legacy implementation owns a detected database.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LegacySourceKind {
    /// Legacy Rust collector (`user_version` outside 2, media under `pictures/`/`videos/`).
    RustV1,
    /// Python v2 collector (`user_version == 2`, media under `images/`).
    PythonV2,
}

/// A read-only snapshot describing a legacy installation on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LegacyDetection {
    /// Which legacy implementation owns the database.
    pub kind: LegacySourceKind,
    /// Absolute path to the legacy SQLite database file.
    pub db_path: PathBuf,
    /// `PRAGMA user_version` read from the SQLite header (big-endian u32 at offset 60).
    pub schema_version: u32,
    /// The legacy data root (parent directory of the database).
    pub media_root: PathBuf,
    /// Legacy pictures directory, present when it exists on disk.
    pub picture_dir: Option<PathBuf>,
    /// Legacy videos directory, present when it exists on disk.
    pub video_dir: Option<PathBuf>,
}

/// Scans `<data_root>/weiback/weiback.db` and classifies it when present.
///
/// The database is never opened through a SQLite connection; only the file
/// header is inspected, so detection has no side effects on the legacy files.
pub fn detect_legacy_sources(data_root: &Path) -> Vec<LegacyDetection> {
    let legacy_root = data_root.join(LEGACY_NAMESPACE);
    let db_path = legacy_root.join("weiback.db");
    let Some(schema_version) = read_sqlite_user_version(&db_path) else {
        return Vec::new();
    };

    let kind = classify(schema_version);
    let (picture_dir, video_dir) = match kind {
        LegacySourceKind::PythonV2 => (Some(legacy_root.join("images")), None),
        LegacySourceKind::RustV1 => (
            Some(legacy_root.join("pictures")),
            Some(legacy_root.join("videos")),
        ),
    };

    vec![LegacyDetection {
        kind,
        db_path,
        schema_version,
        media_root: legacy_root,
        picture_dir: picture_dir.filter(|path| path.is_dir()),
        video_dir: video_dir.filter(|path| path.is_dir()),
    }]
}

fn classify(schema_version: u32) -> LegacySourceKind {
    if schema_version == 2 {
        LegacySourceKind::PythonV2
    } else {
        LegacySourceKind::RustV1
    }
}

/// Reads `PRAGMA user_version` from a SQLite file header without opening a connection.
///
/// The version is stored big-endian at byte offset 60. Returns `None` when the
/// file is missing, too small, or not a SQLite database.
fn read_sqlite_user_version(path: &Path) -> Option<u32> {
    use std::io::Read;

    let mut file = fs::File::open(path).ok()?;
    let mut header = [0u8; 64];
    file.read_exact(&mut header).ok()?;
    if &header[0..16] != b"SQLite format 3\0" {
        return None;
    }
    Some(u32::from_be_bytes([
        header[60], header[61], header[62], header[63],
    ]))
}

#[cfg(test)]
mod local_tests {
    use super::*;
    use sqlx::Connection;
    use std::collections::BTreeMap;

    /// Creates a legacy Rust v1 installation under `root/weiback`:
    /// a real migrated database (user_version == 3) plus media directories.
    async fn make_rust_v1_fixture(root: &Path) {
        let legacy_root = root.join(LEGACY_NAMESPACE);
        fs::create_dir_all(legacy_root.join("pictures")).unwrap();
        fs::create_dir_all(legacy_root.join("videos")).unwrap();
        fs::write(legacy_root.join("pictures/avatar.jpg"), b"pic").unwrap();
        fs::write(legacy_root.join("videos/clip.mp4"), b"video").unwrap();

        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(legacy_root.join("weiback.db"))
            .create_if_missing(true);
        let mut conn = sqlx::sqlite::SqliteConnection::connect_with(&options)
            .await
            .unwrap();
        // Force file creation and persist the schema version in the header.
        sqlx::query("CREATE TABLE posts (id INTEGER PRIMARY KEY)")
            .execute(&mut conn)
            .await
            .unwrap();
        sqlx::query("PRAGMA user_version = 3")
            .execute(&mut conn)
            .await
            .unwrap();
        conn.close().await.unwrap();
    }

    /// Creates a legacy Python v2 installation under `root/weiback`:
    /// an empty database whose header `user_version` is set to 2,
    /// plus an `images/` media directory.
    async fn make_python_v2_fixture(root: &Path) {
        let legacy_root = root.join(LEGACY_NAMESPACE);
        fs::create_dir_all(legacy_root.join("images")).unwrap();
        fs::write(legacy_root.join("images/photo.jpg"), b"image").unwrap();

        let db_path = legacy_root.join("weiback.db");
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(&db_path)
            .create_if_missing(true);
        let mut conn = sqlx::sqlite::SqliteConnection::connect_with(&options)
            .await
            .unwrap();
        // Force file creation, then persist user_version = 2 in the header.
        sqlx::query("CREATE TABLE monitored_users (id INTEGER PRIMARY KEY)")
            .execute(&mut conn)
            .await
            .unwrap();
        sqlx::query("PRAGMA user_version = 2")
            .execute(&mut conn)
            .await
            .unwrap();
        conn.close().await.unwrap();
    }

    fn snapshot_tree(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        let mut snapshot = BTreeMap::new();
        for entry in walkdir::WalkDir::new(root) {
            let entry = entry.unwrap();
            if entry.file_type().is_file() {
                let content = fs::read(entry.path()).unwrap();
                snapshot.insert(entry.path().to_path_buf(), content);
            }
        }
        snapshot
    }

    #[tokio::test]
    async fn detects_rust_v1_legacy_installation() {
        let tmp = tempfile::tempdir().unwrap();
        make_rust_v1_fixture(tmp.path()).await;

        let detections = detect_legacy_sources(tmp.path());

        assert_eq!(detections.len(), 1);
        let detection = &detections[0];
        assert_eq!(detection.kind, LegacySourceKind::RustV1);
        assert_eq!(detection.schema_version, 3);
        assert_eq!(detection.db_path, tmp.path().join("weiback/weiback.db"));
        assert_eq!(detection.media_root, tmp.path().join("weiback"));
        assert_eq!(
            detection.picture_dir,
            Some(tmp.path().join("weiback/pictures"))
        );
        assert_eq!(detection.video_dir, Some(tmp.path().join("weiback/videos")));
    }

    #[tokio::test]
    async fn detects_python_v2_legacy_installation() {
        let tmp = tempfile::tempdir().unwrap();
        make_python_v2_fixture(tmp.path()).await;

        let detections = detect_legacy_sources(tmp.path());

        assert_eq!(detections.len(), 1);
        let detection = &detections[0];
        assert_eq!(detection.kind, LegacySourceKind::PythonV2);
        assert_eq!(detection.schema_version, 2);
        assert_eq!(detection.db_path, tmp.path().join("weiback/weiback.db"));
        assert_eq!(detection.media_root, tmp.path().join("weiback"));
        assert_eq!(
            detection.picture_dir,
            Some(tmp.path().join("weiback/images"))
        );
        assert_eq!(detection.video_dir, None);
    }

    #[tokio::test]
    async fn detection_never_modifies_legacy_files() {
        let tmp = tempfile::tempdir().unwrap();
        make_rust_v1_fixture(tmp.path()).await;
        let before = snapshot_tree(tmp.path());

        let detections = detect_legacy_sources(tmp.path());

        assert_eq!(detections.len(), 1);
        let after = snapshot_tree(tmp.path());
        assert_eq!(before, after, "detection must not change any legacy file");
        assert!(
            snapshot_tree(&tmp.path().join(LEGACY_NAMESPACE))
                .keys()
                .all(|path| !path.to_string_lossy().ends_with("-wal")
                    && !path.to_string_lossy().ends_with("-shm")),
            "detection must not create WAL/SHM files"
        );
    }

    #[tokio::test]
    async fn ignores_non_sqlite_legacy_file() {
        let tmp = tempfile::tempdir().unwrap();
        let legacy_root = tmp.path().join(LEGACY_NAMESPACE);
        fs::create_dir_all(&legacy_root).unwrap();
        fs::write(legacy_root.join("weiback.db"), b"this is not a sqlite db").unwrap();

        let detections = detect_legacy_sources(tmp.path());

        assert!(detections.is_empty());
    }

    #[test]
    fn returns_empty_without_legacy_root() {
        let tmp = tempfile::tempdir().unwrap();

        assert!(detect_legacy_sources(tmp.path()).is_empty());
    }
}
