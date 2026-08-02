use std::fs;

use sqlx::SqlitePool;
use tempfile::tempdir;
use weiback::{
    builder::CoreBuilder,
    config::get_config,
    core::{BackupUserPostsOptions, TaskManager, TaskRequest, TaskType, task::BackupType},
    storage::database::create_db_pool_with_url,
    user_backup::{
        UserBackupPaths, create_user_backup, preflight_restore_user_backup, restore_user_backup,
        restore_user_backup_with_fault_for_test, verify_user_backup,
    },
};

async fn fixture(paths: &UserBackupPaths) -> SqlitePool {
    fs::create_dir_all(paths.media_dir.join("pictures")).unwrap();
    fs::create_dir_all(paths.media_dir.join("videos")).unwrap();
    fs::write(paths.media_dir.join("pictures/kept.jpg"), b"kept-media").unwrap();
    fs::create_dir_all(&paths.picture_dir).unwrap();
    fs::create_dir_all(&paths.video_dir).unwrap();
    fs::write(paths.picture_dir.join("legacy.jpg"), b"legacy-picture").unwrap();
    fs::write(paths.video_dir.join("legacy.mp4"), b"legacy-video").unwrap();
    fs::write(paths.picture_dir.join("partial.jpg.part"), b"partial").unwrap();
    fs::write(paths.picture_dir.join("orphan.jpg"), b"orphan").unwrap();
    let pool = create_db_pool_with_url(paths.db_path.to_str().unwrap())
        .await
        .unwrap();
    sqlx::query("INSERT INTO posts(id,text) VALUES(1,'backup source')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO media(url,media_type,local_path,status,content_length,created_at) \
         VALUES('https://example.invalid/kept.jpg','picture','pictures/kept.jpg','downloaded',10,'2026-01-01T00:00:00Z')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO media_references(media_id,owner_type,owner_id,definition,created_at) \
          SELECT id,'post',1,'large','2026-01-01T00:00:00Z' FROM media",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO picture(id,definition,path,post_id,url) \
         VALUES('legacy-picture','large','legacy.jpg',1,'https://example.invalid/legacy.jpg')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO video(url,path,post_id) \
         VALUES('https://example.invalid/legacy.mp4','legacy.mp4',1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool
}

#[tokio::test]
async fn user_backup_roundtrips_new_and_legacy_referenced_media_only() {
    let temp = tempdir().unwrap();
    let paths = UserBackupPaths::new(
        temp.path().join("weiback.db"),
        temp.path().join("media"),
        temp.path().join("imports"),
    );
    let pool = fixture(&paths).await;
    let backup = create_user_backup(&pool, &paths).await.unwrap();
    assert_eq!(backup.file_count, 4);
    assert!(verify_user_backup(&paths, &backup.id).await.unwrap().valid);
    assert!(backup.relative_path.starts_with("backups/"));
    assert!(
        !backup
            .relative_path
            .contains(temp.path().to_string_lossy().as_ref())
    );
    pool.close().await;

    fs::remove_file(&paths.db_path).unwrap();
    fs::remove_dir_all(&paths.media_dir).unwrap();
    let restored = restore_user_backup(&paths, &backup.id).await.unwrap();
    assert!(
        !restored.rollback_created,
        "there was no current database to snapshot"
    );
    assert!(paths.media_dir.join("pictures/kept.jpg").is_file());
    assert!(paths.media_dir.join("pictures/legacy.jpg").is_file());
    assert!(paths.media_dir.join("videos/legacy.mp4").is_file());
    assert!(!paths.media_dir.join("pictures/partial.jpg.part").exists());
    assert!(!paths.media_dir.join("pictures/orphan.jpg").exists());
    let restored_pool = SqlitePool::connect(paths.db_path.to_str().unwrap())
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT text FROM posts WHERE id=1")
            .fetch_one(&restored_pool)
            .await
            .unwrap(),
        "backup source"
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT path FROM picture WHERE url='https://example.invalid/legacy.jpg'"
        )
        .fetch_one(&restored_pool)
        .await
        .unwrap(),
        "legacy.jpg"
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT path FROM video WHERE url='https://example.invalid/legacy.mp4'"
        )
        .fetch_one(&restored_pool)
        .await
        .unwrap(),
        "legacy.mp4"
    );
}

#[tokio::test]
async fn user_backup_restores_referenced_legacy_media_to_external_configured_roots() {
    let temp = tempdir().unwrap();
    let picture_root = temp.path().join("external-pictures");
    let video_root = temp.path().join("external-videos");
    let paths = UserBackupPaths::new(
        temp.path().join("weiback.db"),
        temp.path().join("media"),
        temp.path().join("imports"),
    )
    .with_legacy_media_roots(picture_root.clone(), video_root.clone());
    let pool = fixture(&paths).await;
    let backup = create_user_backup(&pool, &paths).await.unwrap();
    pool.close().await;

    fs::write(paths.media_dir.join("pictures/kept.jpg"), b"current-media").unwrap();
    fs::write(picture_root.join("legacy.jpg"), b"current-picture").unwrap();
    fs::write(video_root.join("legacy.mp4"), b"current-video").unwrap();

    restore_user_backup(&paths, &backup.id).await.unwrap();

    assert_eq!(
        fs::read(picture_root.join("legacy.jpg")).unwrap(),
        b"legacy-picture"
    );
    assert_eq!(
        fs::read(video_root.join("legacy.mp4")).unwrap(),
        b"legacy-video"
    );
    assert_eq!(
        fs::read(paths.media_dir.join("pictures/kept.jpg")).unwrap(),
        b"kept-media"
    );
    assert!(!picture_root.join("partial.jpg.part").exists());
    assert!(!picture_root.join("orphan.jpg").exists());
}

#[tokio::test]
async fn tampered_hash_or_path_is_rejected_without_changing_current_data() {
    let temp = tempdir().unwrap();
    let paths = UserBackupPaths::new(
        temp.path().join("weiback.db"),
        temp.path().join("media"),
        temp.path().join("imports"),
    );
    let pool = fixture(&paths).await;
    let backup = create_user_backup(&pool, &paths).await.unwrap();
    pool.close().await;
    fs::write(
        paths
            .imports_dir
            .join(&backup.relative_path)
            .join("media/pictures/kept.jpg"),
        b"tampered",
    )
    .unwrap();
    assert!(verify_user_backup(&paths, &backup.id).await.is_err());
    assert!(restore_user_backup(&paths, &backup.id).await.is_err());
    let pool = SqlitePool::connect(paths.db_path.to_str().unwrap())
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT text FROM posts WHERE id=1")
            .fetch_one(&pool)
            .await
            .unwrap(),
        "backup source"
    );

    let safe_backup = create_user_backup(&pool, &paths).await.unwrap();
    let manifest = paths
        .imports_dir
        .join(&safe_backup.relative_path)
        .join("manifest.json");
    let tampered = fs::read_to_string(&manifest)
        .unwrap()
        .replace("media/pictures/kept.jpg", "../escape");
    fs::write(manifest, tampered).unwrap();
    assert!(verify_user_backup(&paths, &safe_backup.id).await.is_err());
}

#[tokio::test]
async fn restore_preflight_rejects_tampered_backup_without_touching_live_data() {
    let temp = tempdir().unwrap();
    let paths = UserBackupPaths::new(
        temp.path().join("weiback.db"),
        temp.path().join("media"),
        temp.path().join("imports"),
    );
    let pool = fixture(&paths).await;
    let backup = create_user_backup(&pool, &paths).await.unwrap();
    fs::write(paths.media_dir.join("pictures/kept.jpg"), b"current-media").unwrap();
    fs::write(
        paths
            .imports_dir
            .join(&backup.relative_path)
            .join("database/weiback.db"),
        b"not sqlite",
    )
    .unwrap();

    assert!(
        preflight_restore_user_backup(&paths, &backup.id)
            .await
            .is_err()
    );
    assert_eq!(
        fs::read(paths.media_dir.join("pictures/kept.jpg")).unwrap(),
        b"current-media"
    );
    assert!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM posts")
            .fetch_one(&pool)
            .await
            .is_ok(),
        "preflight must not close the live pool"
    );
}

#[tokio::test]
async fn restore_failure_rolls_back_current_db_media_and_keeps_rollback_snapshot() {
    let temp = tempdir().unwrap();
    let paths = UserBackupPaths::new(
        temp.path().join("weiback.db"),
        temp.path().join("media"),
        temp.path().join("imports"),
    );
    let pool = fixture(&paths).await;
    let backup = create_user_backup(&pool, &paths).await.unwrap();
    pool.close().await;

    let current = create_db_pool_with_url(paths.db_path.to_str().unwrap())
        .await
        .unwrap();
    sqlx::query("UPDATE posts SET text='current data' WHERE id=1")
        .execute(&current)
        .await
        .unwrap();
    fs::write(paths.media_dir.join("pictures/kept.jpg"), b"current-media").unwrap();

    assert!(
        restore_user_backup_with_fault_for_test(&paths, &backup.id)
            .await
            .is_err()
    );
    assert_eq!(
        fs::read(paths.media_dir.join("pictures/kept.jpg")).unwrap(),
        b"current-media"
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT text FROM posts WHERE id=1")
            .fetch_one(&current)
            .await
            .unwrap(),
        "current data"
    );
    let tasks = TaskManager::new();
    tasks
        .start_task(1, TaskType::BackupUser, "still usable".into(), 1)
        .unwrap();
    assert!(
        fs::read_dir(paths.imports_dir.join("backups"))
            .unwrap()
            .any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("rollback-"))
    );
}

#[tokio::test]
async fn restore_failure_rolls_back_external_legacy_media_roots() {
    let temp = tempdir().unwrap();
    let picture_root = temp.path().join("external-pictures");
    let video_root = temp.path().join("external-videos");
    let paths = UserBackupPaths::new(
        temp.path().join("weiback.db"),
        temp.path().join("media"),
        temp.path().join("imports"),
    )
    .with_legacy_media_roots(picture_root.clone(), video_root.clone());
    let pool = fixture(&paths).await;
    let backup = create_user_backup(&pool, &paths).await.unwrap();
    pool.close().await;

    fs::write(picture_root.join("legacy.jpg"), b"current-picture").unwrap();
    fs::write(video_root.join("legacy.mp4"), b"current-video").unwrap();
    assert!(
        restore_user_backup_with_fault_for_test(&paths, &backup.id)
            .await
            .is_err()
    );

    assert_eq!(
        fs::read(picture_root.join("legacy.jpg")).unwrap(),
        b"current-picture"
    );
    assert_eq!(
        fs::read(video_root.join("legacy.mp4")).unwrap(),
        b"current-video"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn active_real_core_task_blocks_restore_without_closing_pool_or_swapping_data() {
    let temp = tempdir().unwrap();
    let config_handle = get_config();
    let original_config = config_handle.read().unwrap().clone();
    {
        let mut config = config_handle.write().unwrap();
        config.db_path = temp.path().join("weiback.db");
        config.media_path = temp.path().join("media");
        config.picture_path = config.media_path.join("pictures");
        config.video_path = config.media_path.join("videos");
        config.session_path = temp.path().join("session.json");
    }
    let core = CoreBuilder::new().build().unwrap();
    let paths = UserBackupPaths::new(
        temp.path().join("weiback.db"),
        temp.path().join("media"),
        temp.path().join("imports"),
    );
    fs::create_dir_all(paths.media_dir.join("pictures")).unwrap();
    fs::write(paths.media_dir.join("pictures/kept.jpg"), b"current-media").unwrap();
    let backup = core.create_user_backup().await.unwrap();

    core.backup_user(TaskRequest::BackupUser(BackupUserPostsOptions {
        num_pages: 1,
        uid: 1,
        backup_type: BackupType::Normal,
    }))
    .await
    .unwrap();
    assert!(core.get_current_task().await.unwrap().is_some());

    let error = core.restore_user_backup(&backup.id).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("ordinary task or writer is active")
    );
    assert_eq!(
        fs::read(paths.media_dir.join("pictures/kept.jpg")).unwrap(),
        b"current-media"
    );
    assert!(
        core.get_accounts().await.is_ok(),
        "restore must not close the pool"
    );

    let _ = core
        .shutdown_persistent_tasks(std::time::Duration::from_secs(1))
        .await;
    *config_handle.write().unwrap() = original_config;
}
