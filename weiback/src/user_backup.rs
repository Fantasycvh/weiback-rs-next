//! Portable user-data snapshots. Session credentials are deliberately excluded.

use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;

use crate::error::{Error, Result};

const MANIFEST_FILE: &str = "manifest.json";
const COMPLETE_FILE: &str = ".complete";
const SCHEMA_VERSION: u32 = 1;
const MEDIA_NAMESPACE: &str = "media";
const LEGACY_PICTURES_NAMESPACE: &str = "legacy/pictures";
const LEGACY_VIDEOS_NAMESPACE: &str = "legacy/videos";

#[derive(Debug, Clone)]
pub struct UserBackupPaths {
    pub db_path: PathBuf,
    pub media_dir: PathBuf,
    pub imports_dir: PathBuf,
    pub picture_dir: PathBuf,
    pub video_dir: PathBuf,
}

impl UserBackupPaths {
    pub fn new(db_path: PathBuf, media_dir: PathBuf, imports_dir: PathBuf) -> Self {
        Self {
            db_path,
            picture_dir: media_dir.join("pictures"),
            video_dir: media_dir.join("videos"),
            media_dir,
            imports_dir,
        }
    }

    pub fn with_legacy_media_roots(mut self, picture_dir: PathBuf, video_dir: PathBuf) -> Self {
        self.picture_dir = picture_dir;
        self.video_dir = video_dir;
        self
    }

    fn backups_dir(&self) -> PathBuf {
        self.imports_dir.join("backups")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserBackupSummary {
    pub id: String,
    pub relative_path: String,
    pub created_at: String,
    pub file_count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserBackupVerification {
    pub id: String,
    pub valid: bool,
    pub file_count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserRestoreSummary {
    pub id: String,
    pub rollback_created: bool,
    pub restart_required: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    schema_version: u32,
    created_at: String,
    files: Vec<ManifestFile>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ManifestFile {
    path: String,
    length: u64,
    sha256: String,
}

pub async fn create_user_backup(
    pool: &SqlitePool,
    paths: &UserBackupPaths,
) -> Result<UserBackupSummary> {
    validate_media_roots(paths)?;
    fs::create_dir_all(paths.backups_dir())?;
    let id = uuid::Uuid::now_v7().to_string();
    let staging = paths.backups_dir().join(format!(".creating-{id}"));
    let final_dir = paths.backups_dir().join(&id);
    let result = async {
        fs::create_dir(&staging)?;
        let snapshot = staging.join("database/weiback.db");
        fs::create_dir_all(snapshot.parent().expect("database parent"))?;
        vacuum_into(pool, &snapshot).await?;
        let mut files = vec![manifest_file(&staging, Path::new("database/weiback.db"))?];
        for (relative, source) in downloaded_media_paths(pool, paths).await? {
            if relative
                .extension()
                .is_some_and(|extension| extension == "part")
            {
                continue;
            }
            let target = staging.join(&relative);
            copy_file(&source, &target)?;
            files.push(manifest_file(&staging, &relative)?);
        }
        let manifest = Manifest {
            schema_version: SCHEMA_VERSION,
            created_at: chrono::Utc::now().to_rfc3339(),
            files,
        };
        write_json_sync(&staging.join(MANIFEST_FILE), &manifest)?;
        sync_dir(&staging)?;
        fs::write(staging.join(COMPLETE_FILE), b"complete\n")?;
        sync_dir(&staging)?;
        fs::rename(&staging, &final_dir)?;
        sync_dir(&paths.backups_dir())?;
        Ok(UserBackupSummary {
            id: id.clone(),
            relative_path: format!("backups/{id}"),
            created_at: manifest.created_at,
            file_count: manifest.files.len() as u64,
        })
    }
    .await;
    if result.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    result
}

pub async fn list_user_backups(paths: &UserBackupPaths) -> Result<Vec<UserBackupSummary>> {
    let mut results = Vec::new();
    let Ok(entries) = fs::read_dir(paths.backups_dir()) else {
        return Ok(results);
    };
    for entry in entries.flatten() {
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let id = entry.file_name().to_string_lossy().to_string();
        if !is_backup_id(&id) {
            continue;
        }
        if let Ok(manifest) = read_manifest(&entry.path()) {
            results.push(UserBackupSummary {
                relative_path: format!("backups/{id}"),
                id,
                created_at: manifest.created_at,
                file_count: manifest.files.len() as u64,
            });
        }
    }
    results.sort_by(|left, right| right.created_at.cmp(&left.created_at));
    Ok(results)
}

pub async fn verify_user_backup(
    paths: &UserBackupPaths,
    id: &str,
) -> Result<UserBackupVerification> {
    let root = backup_root(paths, id)?;
    let manifest = verify_manifest(&root).await?;
    Ok(UserBackupVerification {
        id: id.to_string(),
        valid: true,
        file_count: manifest.files.len() as u64,
    })
}

pub async fn restore_user_backup(paths: &UserBackupPaths, id: &str) -> Result<UserRestoreSummary> {
    restore(paths, id, false).await
}

/// Verifies and materializes a restore payload before Core closes its live pool.
/// The staging directory is discarded after the check; the final swap still happens later.
pub async fn preflight_restore_user_backup(paths: &UserBackupPaths, id: &str) -> Result<()> {
    validate_media_roots(paths)?;
    let source = backup_root(paths, id)?;
    let manifest = verify_manifest(&source).await?;
    let operation = uuid::Uuid::now_v7().to_string();
    let stage = paths.backups_dir().join(format!(".preflight-{operation}"));
    fs::create_dir_all(paths.backups_dir())?;
    let result = async {
        fs::create_dir(&stage)?;
        for file in &manifest.files {
            let relative = safe_relative(&file.path)?;
            copy_file(&source.join(&relative), &stage.join(relative))?;
        }
        verify_sqlite(&stage.join("database/weiback.db")).await
    }
    .await;
    let _ = fs::remove_dir_all(&stage);
    result
}

/// Injects a failure after both targets have been moved, solely to prove rollback safety.
pub async fn restore_user_backup_with_fault_for_test(
    paths: &UserBackupPaths,
    id: &str,
) -> Result<UserRestoreSummary> {
    restore(paths, id, true).await
}

async fn restore(
    paths: &UserBackupPaths,
    id: &str,
    inject_failure: bool,
) -> Result<UserRestoreSummary> {
    validate_media_roots(paths)?;
    let source = backup_root(paths, id)?;
    let manifest = verify_manifest(&source).await?;
    let db_entry = manifest
        .files
        .iter()
        .find(|file| file.path == "database/weiback.db")
        .ok_or_else(|| Error::FormatError("backup has no database snapshot".into()))?;
    if db_entry.length == 0 {
        return Err(Error::FormatError("backup database is empty".into()));
    }
    let operation = uuid::Uuid::now_v7().to_string();
    let stage = paths.backups_dir().join(format!(".restore-{operation}"));
    let old_db = paths.backups_dir().join(format!(".old-db-{operation}"));
    let old_media = paths.backups_dir().join(format!(".old-media-{operation}"));
    let old_pictures = paths
        .backups_dir()
        .join(format!(".old-pictures-{operation}"));
    let old_videos = paths.backups_dir().join(format!(".old-videos-{operation}"));
    let rollback_dir = paths.backups_dir().join(format!("rollback-{operation}"));
    fs::create_dir_all(paths.backups_dir())?;
    let result = async {
        fs::create_dir(&stage)?;
        for file in &manifest.files {
            let relative = safe_relative(&file.path)?;
            let target = stage.join(&relative);
            copy_file(&source.join(&relative), &target)?;
        }
        verify_sqlite(&stage.join("database/weiback.db")).await?;
        let rollback_created = if paths.db_path.exists() {
            fs::create_dir(&rollback_dir)?;
            let rollback_db = rollback_dir.join("weiback.db");
            let current = SqlitePool::connect(paths.db_path.to_string_lossy().as_ref()).await?;
            let snapshot = vacuum_into(&current, &rollback_db).await;
            current.close().await;
            snapshot?;
            true
        } else {
            false
        };
        let staged_db = stage.join("database/weiback.db");
        let staged_media = stage.join(MEDIA_NAMESPACE);
        let staged_pictures = stage.join(LEGACY_PICTURES_NAMESPACE);
        let staged_videos = stage.join(LEGACY_VIDEOS_NAMESPACE);
        if paths.db_path.exists() {
            fs::rename(&paths.db_path, &old_db)?;
        }
        if paths.media_dir.exists()
            && let Err(error) = fs::rename(&paths.media_dir, &old_media)
        {
            if old_db.exists() {
                let _ = fs::rename(&old_db, &paths.db_path);
            }
            return Err(error.into());
        }
        if uses_external_picture_root(paths)
            && let Err(error) = move_existing_root(&paths.picture_dir, &old_pictures)
        {
            rollback_roots(&paths.db_path, &old_db, &paths.media_dir, &old_media);
            return Err(error);
        }
        if uses_external_video_root(paths)
            && let Err(error) = move_existing_root(&paths.video_dir, &old_videos)
        {
            rollback_roots(&paths.db_path, &old_db, &paths.media_dir, &old_media);
            let _ = move_existing_root(&old_pictures, &paths.picture_dir);
            return Err(error);
        }
        let swapped = (|| -> Result<()> {
            if let Some(parent) = paths.db_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::rename(&staged_db, &paths.db_path)?;
            if staged_media.exists() {
                fs::rename(&staged_media, &paths.media_dir)?;
            } else {
                fs::create_dir_all(&paths.media_dir)?;
            }
            restore_root(&staged_pictures, &paths.picture_dir)?;
            restore_root(&staged_videos, &paths.video_dir)?;
            if inject_failure {
                return Err(Error::Io(std::io::Error::other("injected restore failure")));
            }
            Ok(())
        })();
        if let Err(error) = swapped {
            let _ = fs::remove_file(&paths.db_path);
            let _ = fs::remove_dir_all(&paths.media_dir);
            if uses_external_picture_root(paths) {
                let _ = fs::remove_dir_all(&paths.picture_dir);
            }
            if uses_external_video_root(paths) {
                let _ = fs::remove_dir_all(&paths.video_dir);
            }
            if old_db.exists() {
                let _ = fs::rename(&old_db, &paths.db_path);
            }
            if old_media.exists() {
                let _ = fs::rename(&old_media, &paths.media_dir);
            }
            let _ = move_existing_root(&old_pictures, &paths.picture_dir);
            let _ = move_existing_root(&old_videos, &paths.video_dir);
            return Err(error);
        }
        let _ = fs::remove_file(&old_db);
        let _ = fs::remove_dir_all(&old_media);
        let _ = fs::remove_dir_all(&old_pictures);
        let _ = fs::remove_dir_all(&old_videos);
        let _ = fs::remove_dir_all(&stage);
        Ok(UserRestoreSummary {
            id: id.to_string(),
            rollback_created,
            restart_required: true,
        })
    }
    .await;
    if result.is_err() {
        let _ = fs::remove_dir_all(&stage);
    }
    result
}

fn backup_root(paths: &UserBackupPaths, id: &str) -> Result<PathBuf> {
    if !is_backup_id(id) {
        return Err(Error::FormatError("invalid backup identifier".into()));
    }
    Ok(paths.backups_dir().join(id))
}

fn is_backup_id(id: &str) -> bool {
    id.len() == 36
        && id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
}

async fn verify_manifest(root: &Path) -> Result<Manifest> {
    if !root.is_dir()
        || fs::symlink_metadata(root)?.file_type().is_symlink()
        || !root.join(COMPLETE_FILE).is_file()
    {
        return Err(Error::FormatError("backup is incomplete".into()));
    }
    let manifest = read_manifest(root)?;
    if manifest.schema_version != SCHEMA_VERSION || manifest.files.is_empty() {
        return Err(Error::FormatError(
            "unsupported or empty backup manifest".into(),
        ));
    }
    let mut database_found = false;
    let mut seen_paths = std::collections::HashSet::new();
    for entry in &manifest.files {
        if !seen_paths.insert(&entry.path) {
            return Err(Error::FormatError(
                "backup manifest has duplicate paths".into(),
            ));
        }
        let relative = safe_relative(&entry.path)?;
        if entry.path != "database/weiback.db"
            && !entry.path.starts_with("media/")
            && !entry.path.starts_with("legacy/pictures/")
            && !entry.path.starts_with("legacy/videos/")
        {
            return Err(Error::FormatError(
                "backup manifest has an unexpected path".into(),
            ));
        }
        if entry.path.ends_with(".part") {
            return Err(Error::FormatError(
                "backup manifest contains partial media".into(),
            ));
        }
        if entry.path == "database/weiback.db" {
            database_found = true;
        }
        let source = root.join(relative);
        let metadata = fs::symlink_metadata(&source)?;
        if !metadata.is_file()
            || metadata.file_type().is_symlink()
            || metadata.len() != entry.length
            || !contained_regular_file(root, &source)?
        {
            return Err(Error::FormatError(
                "backup file metadata does not match manifest".into(),
            ));
        }
        if sha256_file(&source)? != entry.sha256 {
            return Err(Error::FormatError(
                "backup file hash does not match manifest".into(),
            ));
        }
    }
    if !database_found {
        return Err(Error::FormatError("backup has no database snapshot".into()));
    }
    verify_sqlite(&root.join("database/weiback.db")).await?;
    Ok(manifest)
}

async fn downloaded_media_paths(
    pool: &SqlitePool,
    paths: &UserBackupPaths,
) -> Result<BTreeMap<PathBuf, PathBuf>> {
    let mut files = BTreeMap::new();
    for local_path in sqlx::query_scalar::<_, String>(
        "SELECT DISTINCT media.local_path FROM media \
          WHERE media.status='downloaded' AND media.local_path IS NOT NULL \
          AND EXISTS(SELECT 1 FROM media_references WHERE media_references.media_id=media.id)",
    )
    .fetch_all(pool)
    .await?
    {
        insert_media_file(
            &mut files,
            &paths.media_dir,
            Path::new(MEDIA_NAMESPACE),
            &local_path,
        )?;
    }
    for path in sqlx::query_scalar::<_, String>(
        "SELECT DISTINCT path FROM picture WHERE post_id IS NOT NULL AND path IS NOT NULL AND path != ''",
    )
    .fetch_all(pool)
    .await?
    {
        insert_media_file(
            &mut files,
            &paths.picture_dir,
            legacy_picture_namespace(paths),
            &path,
        )?;
    }
    for path in sqlx::query_scalar::<_, String>(
        "SELECT DISTINCT path FROM video WHERE post_id IS NOT NULL AND path IS NOT NULL AND path != ''",
    )
    .fetch_all(pool)
    .await?
    {
        insert_media_file(
            &mut files,
            &paths.video_dir,
            legacy_video_namespace(paths),
            &path,
        )?;
    }
    Ok(files)
}

fn validate_media_roots(paths: &UserBackupPaths) -> Result<()> {
    let default_pictures = paths.media_dir.join("pictures");
    let default_videos = paths.media_dir.join("videos");
    let picture_external = paths.picture_dir != default_pictures;
    let video_external = paths.video_dir != default_videos;
    if (picture_external && paths.picture_dir.starts_with(&paths.media_dir))
        || (video_external && paths.video_dir.starts_with(&paths.media_dir))
        || (picture_external && paths.media_dir.starts_with(&paths.picture_dir))
        || (video_external && paths.media_dir.starts_with(&paths.video_dir))
        || (picture_external
            && video_external
            && (paths.picture_dir.starts_with(&paths.video_dir)
                || paths.video_dir.starts_with(&paths.picture_dir)))
    {
        return Err(Error::FormatError(
            "legacy media roots must be distinct from the unified media root".into(),
        ));
    }
    Ok(())
}

fn uses_external_picture_root(paths: &UserBackupPaths) -> bool {
    paths.picture_dir != paths.media_dir.join("pictures")
}

fn uses_external_video_root(paths: &UserBackupPaths) -> bool {
    paths.video_dir != paths.media_dir.join("videos")
}

fn legacy_picture_namespace(paths: &UserBackupPaths) -> &Path {
    if uses_external_picture_root(paths) {
        Path::new(LEGACY_PICTURES_NAMESPACE)
    } else {
        Path::new("media/pictures")
    }
}

fn legacy_video_namespace(paths: &UserBackupPaths) -> &Path {
    if uses_external_video_root(paths) {
        Path::new(LEGACY_VIDEOS_NAMESPACE)
    } else {
        Path::new("media/videos")
    }
}

fn move_existing_root(source: &Path, destination: &Path) -> Result<()> {
    if source.exists() {
        fs::rename(source, destination)?;
    }
    Ok(())
}

fn restore_root(staged: &Path, destination: &Path) -> Result<()> {
    if staged.exists() {
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::rename(staged, destination)?;
    } else {
        fs::create_dir_all(destination)?;
    }
    Ok(())
}

fn rollback_roots(db_path: &Path, old_db: &Path, media_dir: &Path, old_media: &Path) {
    if old_db.exists() {
        let _ = fs::rename(old_db, db_path);
    }
    if old_media.exists() {
        let _ = fs::rename(old_media, media_dir);
    }
}

fn insert_media_file(
    files: &mut BTreeMap<PathBuf, PathBuf>,
    root: &Path,
    destination_root: &Path,
    raw: &str,
) -> Result<()> {
    let relative = safe_relative(raw)?;
    if relative
        .extension()
        .is_some_and(|extension| extension == "part")
    {
        return Ok(());
    }
    let source = checked_media_path(root, raw)?;
    if source.is_file() && contained_regular_file(root, &source)? {
        files
            .entry(destination_root.join(relative))
            .or_insert(source);
    }
    Ok(())
}

async fn vacuum_into(pool: &SqlitePool, target: &Path) -> Result<()> {
    let escaped = target.to_string_lossy().replace('\'', "''");
    sqlx::query(&format!("VACUUM INTO '{escaped}'"))
        .execute(pool)
        .await
        .map_err(|error| Error::DbError(format!("SQLite snapshot failed: {error}")))?;
    Ok(())
}

async fn verify_sqlite(path: &Path) -> Result<()> {
    let pool = SqlitePool::connect(path.to_string_lossy().as_ref()).await?;
    let result: String = sqlx::query_scalar("PRAGMA integrity_check")
        .fetch_one(&pool)
        .await?;
    pool.close().await;
    if result != "ok" {
        return Err(Error::FormatError("SQLite integrity check failed".into()));
    }
    Ok(())
}

fn checked_media_path(root: &Path, raw: &str) -> Result<PathBuf> {
    Ok(root.join(safe_relative(raw)?))
}

fn contained_regular_file(root: &Path, file: &Path) -> Result<bool> {
    let root = root.canonicalize()?;
    let file = file.canonicalize()?;
    Ok(file.starts_with(root) && !fs::symlink_metadata(&file)?.file_type().is_symlink())
}

fn safe_relative(raw: &str) -> Result<PathBuf> {
    let path = Path::new(raw);
    if raw.is_empty()
        || path.is_absolute()
        || raw.contains('\0')
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(Error::FormatError("unsafe backup path".into()));
    }
    Ok(path.to_path_buf())
}

fn copy_file(source: &Path, target: &Path) -> Result<()> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut input = fs::File::open(source)?;
    let mut output = fs::File::create(target)?;
    std::io::copy(&mut input, &mut output)?;
    output.sync_all()?;
    Ok(())
}

fn manifest_file(root: &Path, relative: &Path) -> Result<ManifestFile> {
    let path = root.join(relative);
    Ok(ManifestFile {
        path: relative.to_string_lossy().replace('\\', "/"),
        length: fs::metadata(&path)?.len(),
        sha256: sha256_file(&path)?,
    })
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; 32 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hash.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn write_json_sync(path: &Path, value: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value)?;
    let mut file = fs::File::create(path)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    Ok(())
}

fn read_manifest(root: &Path) -> Result<Manifest> {
    Ok(serde_json::from_slice(&fs::read(
        root.join(MANIFEST_FILE),
    )?)?)
}

fn sync_dir(path: &Path) -> Result<()> {
    #[cfg(not(windows))]
    fs::File::open(path)?.sync_all()?;
    #[cfg(windows)]
    let _ = path;
    Ok(())
}
