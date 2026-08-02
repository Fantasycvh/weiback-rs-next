use std::{
    fs,
    path::Path,
    sync::{Arc, Barrier},
};

use sqlx::{SqlitePool, sqlite::SqliteConnectOptions};
use tempfile::tempdir;
use weiback::{
    legacy::{
        LegacyImportRequest, LegacyImportStatus, LegacySourceKind, import_legacy_source,
        inspect_legacy_source,
    },
    storage::{
        database::create_db_pool_with_url,
        internal::entities::{MediaClaimRequest, claim_next_media},
    },
};

async fn open_fixture(path: &Path) -> SqlitePool {
    SqlitePool::connect_with(
        SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true),
    )
    .await
    .unwrap()
}

async fn rust_v1_fixture(root: &Path) -> std::path::PathBuf {
    fs::create_dir_all(root.join("pictures")).unwrap();
    fs::create_dir_all(root.join("videos")).unwrap();
    fs::write(root.join("pictures/p.jpg"), b"legacy-picture").unwrap();
    fs::write(root.join("videos/v.mp4"), b"legacy-video").unwrap();
    let db_path = root.join("weiback.db");
    let pool = open_fixture(&db_path).await;
    sqlx::query("CREATE TABLE users(id INTEGER PRIMARY KEY, screen_name TEXT)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("CREATE TABLE posts(id INTEGER PRIMARY KEY, text TEXT, mblogid TEXT, uid INTEGER, created_at TEXT, deleted INTEGER, favorited INTEGER, edit_count INTEGER, attitudes_status INTEGER)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("CREATE TABLE picture(id TEXT, definition TEXT, path TEXT, post_id INTEGER, url TEXT PRIMARY KEY, user_id INTEGER)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("CREATE TABLE video(url TEXT PRIMARY KEY, path TEXT, post_id INTEGER)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO users(id,screen_name) VALUES(7,'legacy user')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO posts VALUES(42,'legacy post','m42',7,'2026-01-01T00:00:00Z',0,0,0,0)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO picture VALUES('p','large','p.jpg',42,'https://example.invalid/p.jpg',NULL)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO video VALUES('https://example.invalid/v.mp4','v.mp4',42)")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    db_path
}

async fn python_v2_fixture(root: &Path) -> std::path::PathBuf {
    fs::create_dir_all(root).unwrap();
    let db_path = root.join("weiback.db");
    let pool = open_fixture(&db_path).await;
    sqlx::query("CREATE TABLE user(id INTEGER PRIMARY KEY, screen_name TEXT)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("CREATE TABLE weibo(id INTEGER PRIMARY KEY, text TEXT, mblogid TEXT, uid INTEGER, created_at TEXT)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("CREATE TABLE image(url TEXT PRIMARY KEY, post_id INTEGER, path TEXT)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO user VALUES(8,'python user')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO weibo VALUES(43,'python post','m43',8,'2026-01-02T00:00:00Z')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO image VALUES('https://example.invalid/python.jpg',43,'missing.jpg')")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    db_path
}

#[tokio::test]
async fn imports_rust_v1_signature_read_only_with_safe_media_and_p3_query() {
    let temp = tempdir().unwrap();
    let source_root = temp.path().join("legacy");
    let source = rust_v1_fixture(&source_root).await;
    let target_root = temp.path().join("next");
    let target = create_db_pool_with_url(target_root.join("weiback.db").to_str().unwrap())
        .await
        .unwrap();
    let before = fs::read(&source).unwrap();

    let inspected = inspect_legacy_source(&source, &target_root.join("weiback.db"))
        .await
        .unwrap();
    assert_eq!(inspected.kind, LegacySourceKind::RustV1);
    let summary = import_legacy_source(
        &target,
        LegacyImportRequest::new(
            source.clone(),
            target_root.join("media"),
            target_root.join("imports"),
        ),
    )
    .await
    .unwrap();

    assert_eq!(summary.source.kind, LegacySourceKind::RustV1);
    assert_eq!(summary.posts, 1);
    assert_eq!(summary.users, 1);
    assert_eq!(summary.media_copied, 2);
    assert_eq!(
        fs::read(&source).unwrap(),
        before,
        "source remains unchanged"
    );
    let picture_path: String = sqlx::query_scalar(
        "SELECT local_path FROM media WHERE url='https://example.invalid/p.jpg'",
    )
    .fetch_one(&target)
    .await
    .unwrap();
    let video_path: String = sqlx::query_scalar(
        "SELECT local_path FROM media WHERE url='https://example.invalid/v.mp4'",
    )
    .fetch_one(&target)
    .await
    .unwrap();
    assert_eq!(
        fs::read(target_root.join("media").join(picture_path)).unwrap(),
        b"legacy-picture"
    );
    assert_eq!(
        fs::read(target_root.join("media").join(video_path)).unwrap(),
        b"legacy-video"
    );
    let text: String = sqlx::query_scalar("SELECT text FROM posts WHERE id=42")
        .fetch_one(&target)
        .await
        .unwrap();
    assert_eq!(text, "legacy post");
    let status: String =
        sqlx::query_scalar("SELECT status FROM media WHERE url='https://example.invalid/p.jpg'")
            .fetch_one(&target)
            .await
            .unwrap();
    assert_eq!(status, "downloaded");
}

#[tokio::test]
async fn imports_python_v2_signature_and_keeps_missing_media_pending_remote() {
    let temp = tempdir().unwrap();
    let source = python_v2_fixture(&temp.path().join("legacy")).await;
    let target_root = temp.path().join("next");
    let target = create_db_pool_with_url(target_root.join("weiback.db").to_str().unwrap())
        .await
        .unwrap();

    let summary = import_legacy_source(
        &target,
        LegacyImportRequest::new(
            source,
            target_root.join("media"),
            target_root.join("imports"),
        ),
    )
    .await
    .unwrap();

    assert_eq!(summary.source.kind, LegacySourceKind::PythonV2);
    assert_eq!(summary.posts, 1);
    assert_eq!(summary.media_pending, 1);
    let status: String = sqlx::query_scalar(
        "SELECT status FROM media WHERE url='https://example.invalid/python.jpg'",
    )
    .fetch_one(&target)
    .await
    .unwrap();
    assert_eq!(status, "pending");
}

#[tokio::test]
async fn rejects_invalid_current_unknown_and_path_escape_sources() {
    let temp = tempdir().unwrap();
    let target_path = temp.path().join("next/weiback.db");
    let target = create_db_pool_with_url(target_path.to_str().unwrap())
        .await
        .unwrap();
    let invalid = temp.path().join("bad.db");
    fs::write(&invalid, "not sqlite").unwrap();
    assert!(inspect_legacy_source(&invalid, &target_path).await.is_err());
    assert!(
        inspect_legacy_source(&target_path, &target_path)
            .await
            .is_err()
    );
    let unknown = temp.path().join("unknown.db");
    let unknown_pool = open_fixture(&unknown).await;
    sqlx::query("CREATE TABLE unrelated(value TEXT)")
        .execute(&unknown_pool)
        .await
        .unwrap();
    unknown_pool.close().await;
    assert!(inspect_legacy_source(&unknown, &target_path).await.is_err());
    let source = rust_v1_fixture(&temp.path().join("legacy")).await;
    sqlx::query("UPDATE picture SET path='../outside.jpg'")
        .execute(&open_fixture(&source).await)
        .await
        .unwrap();
    let result = import_legacy_source(
        &target,
        LegacyImportRequest::new(
            source,
            temp.path().join("next/media"),
            temp.path().join("next/imports"),
        ),
    )
    .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn partial_failure_rolls_back_target_and_retains_backup() {
    let temp = tempdir().unwrap();
    let source = rust_v1_fixture(&temp.path().join("legacy")).await;
    let target_path = temp.path().join("next/weiback.db");
    let target = create_db_pool_with_url(target_path.to_str().unwrap())
        .await
        .unwrap();
    sqlx::query("INSERT INTO posts(id,text) VALUES(900,'existing')")
        .execute(&target)
        .await
        .unwrap();
    sqlx::query("UPDATE picture SET url='' WHERE id='p'")
        .execute(&open_fixture(&source).await)
        .await
        .unwrap();

    let result = import_legacy_source(
        &target,
        LegacyImportRequest::new(
            source,
            temp.path().join("next/media"),
            temp.path().join("next/imports"),
        ),
    )
    .await;

    assert!(result.is_err());
    let posts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM posts")
        .fetch_one(&target)
        .await
        .unwrap();
    assert_eq!(posts, 1);
    assert!(
        fs::read_dir(temp.path().join("next/imports"))
            .unwrap()
            .any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("rollback-backup"))
    );
}

#[tokio::test]
async fn imports_same_basename_without_overwriting_existing_media() {
    let temp = tempdir().unwrap();
    let source_root = temp.path().join("legacy");
    let source = rust_v1_fixture(&source_root).await;
    fs::create_dir_all(source_root.join("pictures/nested")).unwrap();
    fs::write(source_root.join("pictures/nested/p.jpg"), b"nested-picture").unwrap();
    let source_pool = open_fixture(&source).await;
    sqlx::query("INSERT INTO picture VALUES('p2','large','nested/p.jpg',42,'https://example.invalid/p2.jpg',NULL)")
        .execute(&source_pool)
        .await
        .unwrap();
    source_pool.close().await;

    let target_root = temp.path().join("next");
    fs::create_dir_all(target_root.join("media/pictures")).unwrap();
    fs::write(target_root.join("media/pictures/p.jpg"), b"pre-existing").unwrap();
    let target = create_db_pool_with_url(target_root.join("weiback.db").to_str().unwrap())
        .await
        .unwrap();

    let summary = import_legacy_source(
        &target,
        LegacyImportRequest::new(
            source,
            target_root.join("media"),
            target_root.join("imports"),
        ),
    )
    .await
    .unwrap();
    assert_eq!(summary.media_copied, 3);

    let paths: Vec<String> = sqlx::query_scalar(
        "SELECT local_path FROM media WHERE url IN ('https://example.invalid/p.jpg', 'https://example.invalid/p2.jpg') ORDER BY url",
    )
    .fetch_all(&target)
    .await
    .unwrap();
    assert_eq!(paths.len(), 2);
    assert_ne!(paths[0], paths[1]);
    assert_eq!(
        fs::read(target_root.join("media").join(&paths[0])).unwrap(),
        b"legacy-picture"
    );
    assert_eq!(
        fs::read(target_root.join("media").join(&paths[1])).unwrap(),
        b"nested-picture"
    );
    assert_eq!(
        fs::read(target_root.join("media/pictures/p.jpg")).unwrap(),
        b"pre-existing"
    );
    assert!(
        fs::read_dir(target_root.join("imports"))
            .unwrap()
            .any(|entry| { entry.unwrap().path().join("completed").is_file() })
    );
}

#[tokio::test]
async fn publish_failure_keeps_database_and_media_consistent_for_recovery() {
    let temp = tempdir().unwrap();
    let source = rust_v1_fixture(&temp.path().join("legacy")).await;
    let target_root = temp.path().join("next");
    let target = create_db_pool_with_url(target_root.join("weiback.db").to_str().unwrap())
        .await
        .unwrap();
    let request = LegacyImportRequest::new(
        source.clone(),
        target_root.join("media"),
        target_root.join("imports"),
    )
    .with_publish_failure_after_for_test(1);

    let partial = import_legacy_source(&target, request).await.unwrap();
    assert_eq!(partial.status, LegacyImportStatus::PartialRecoverable);
    assert_eq!(
        partial.diagnostic_code.as_deref(),
        Some("LEGACY_IMPORT_POSTCOMMIT_PUBLISH_FAILED")
    );
    let states: Vec<(String, String, Option<String>, bool)> = sqlx::query_as(
        "SELECT url,status,local_path,import_hold FROM media WHERE url IN ('https://example.invalid/p.jpg', 'https://example.invalid/v.mp4') ORDER BY url",
    )
    .fetch_all(&target)
    .await
    .unwrap();
    assert_eq!(
        states
            .iter()
            .filter(|(_, status, _, _)| status == "downloaded")
            .count(),
        1
    );
    assert_eq!(
        states
            .iter()
            .filter(|(_, status, path, hold)| status == "pending" && path.is_none() && *hold)
            .count(),
        1
    );
    for (_, status, path, _) in &states {
        if status == "downloaded" {
            assert!(
                target_root
                    .join("media")
                    .join(path.as_ref().unwrap())
                    .is_file()
            );
        }
    }
    assert!(
        fs::read_dir(target_root.join("imports"))
            .unwrap()
            .any(|entry| {
                entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("import-media-")
            })
    );

    let retry = import_legacy_source(
        &target,
        LegacyImportRequest::new(
            source,
            target_root.join("media"),
            target_root.join("imports"),
        ),
    )
    .await
    .unwrap();
    assert_eq!(
        retry.media_copied, 0,
        "recovery publishes the retained batch before re-import"
    );
    let pending: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM media WHERE url IN ('https://example.invalid/p.jpg', 'https://example.invalid/v.mp4') AND status != 'downloaded'",
    )
    .fetch_one(&target)
    .await
    .unwrap();
    assert_eq!(pending, 0);
}

#[tokio::test]
async fn imported_media_is_held_from_workers_until_manifest_publish_then_missing_remote_is_pending()
{
    let temp = tempdir().unwrap();
    let source = rust_v1_fixture(&temp.path().join("legacy")).await;
    let target_root = temp.path().join("next");
    let target = create_db_pool_with_url(target_root.join("weiback.db").to_str().unwrap())
        .await
        .unwrap();

    let partial = import_legacy_source(
        &target,
        LegacyImportRequest::new(
            source.clone(),
            target_root.join("media"),
            target_root.join("imports"),
        )
        .with_publish_failure_after_for_test(0),
    )
    .await
    .unwrap();
    assert_eq!(partial.status, LegacyImportStatus::PartialRecoverable);
    let held: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM media WHERE import_hold=1")
        .fetch_one(&target)
        .await
        .unwrap();
    assert_eq!(held, 2);
    assert!(
        claim_next_media(
            &target,
            &MediaClaimRequest {
                token: "worker".into(),
                now_epoch: 1,
                claimed_at: "claimed".into(),
            },
        )
        .await
        .unwrap()
        .is_none()
    );

    import_legacy_source(
        &target,
        LegacyImportRequest::new(
            source,
            target_root.join("media"),
            target_root.join("imports"),
        ),
    )
    .await
    .unwrap();
    let downloaded: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM media WHERE status='downloaded'")
            .fetch_one(&target)
            .await
            .unwrap();
    assert_eq!(downloaded, 2);

    let python_source = python_v2_fixture(&temp.path().join("python-legacy")).await;
    import_legacy_source(
        &target,
        LegacyImportRequest::new(
            python_source,
            target_root.join("media"),
            target_root.join("imports"),
        ),
    )
    .await
    .unwrap();
    let remote_status: String = sqlx::query_scalar(
        "SELECT status FROM media WHERE url='https://example.invalid/python.jpg'",
    )
    .fetch_one(&target)
    .await
    .unwrap();
    assert_eq!(remote_status, "pending");
}

#[tokio::test]
async fn same_url_picture_and_video_prefers_video_asset_without_overwriting_staging_or_references()
{
    let temp = tempdir().unwrap();
    let source_root = temp.path().join("legacy");
    let source = rust_v1_fixture(&source_root).await;
    fs::write(
        source_root.join("videos/shared.mp4"),
        b"legacy-shared-video",
    )
    .unwrap();
    let source_pool = open_fixture(&source).await;
    sqlx::query("INSERT INTO video VALUES('https://example.invalid/p.jpg','shared.mp4',42)")
        .execute(&source_pool)
        .await
        .unwrap();
    source_pool.close().await;
    let target_root = temp.path().join("next");
    let target = create_db_pool_with_url(target_root.join("weiback.db").to_str().unwrap())
        .await
        .unwrap();

    let summary = import_legacy_source(
        &target,
        LegacyImportRequest::new(
            source,
            target_root.join("media"),
            target_root.join("imports"),
        ),
    )
    .await
    .unwrap();
    assert_eq!(summary.status, LegacyImportStatus::Completed);
    let asset: (String, String, String) = sqlx::query_as(
        "SELECT media_type,status,local_path FROM media WHERE url='https://example.invalid/p.jpg'",
    )
    .fetch_one(&target)
    .await
    .unwrap();
    assert_eq!(asset.0, "video");
    assert_eq!(asset.1, "downloaded");
    assert!(asset.2.starts_with("videos/"));
    assert_eq!(
        fs::read(target_root.join("media").join(&asset.2)).unwrap(),
        b"legacy-shared-video"
    );
    let definitions: Vec<String> = sqlx::query_scalar(
        "SELECT definition FROM media_references r JOIN media m ON m.id=r.media_id \
         WHERE m.url='https://example.invalid/p.jpg' ORDER BY definition",
    )
    .fetch_all(&target)
    .await
    .unwrap();
    assert_eq!(definitions, vec!["large"]);
}

#[tokio::test]
async fn commit_failure_publishes_no_media_and_rolls_back_database() {
    let temp = tempdir().unwrap();
    let source = rust_v1_fixture(&temp.path().join("legacy")).await;
    let target_root = temp.path().join("next");
    let target = create_db_pool_with_url(target_root.join("weiback.db").to_str().unwrap())
        .await
        .unwrap();

    let request = LegacyImportRequest::new(
        source,
        target_root.join("media"),
        target_root.join("imports"),
    )
    .with_commit_failure_for_test();
    assert!(import_legacy_source(&target, request).await.is_err());
    let media: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM media")
        .fetch_one(&target)
        .await
        .unwrap();
    assert_eq!(media, 0);
    assert!(!target_root.join("media").exists());
}

#[tokio::test]
async fn rejects_malicious_pending_manifest_without_touching_external_files() {
    let temp = tempdir().unwrap();
    let source = rust_v1_fixture(&temp.path().join("legacy")).await;
    let target_root = temp.path().join("next");
    let target = create_db_pool_with_url(target_root.join("weiback.db").to_str().unwrap())
        .await
        .unwrap();
    let imports = target_root.join("imports");
    let batch_name = format!("import-media-{}", uuid::Uuid::now_v7());
    let batch = imports.join(&batch_name);
    fs::create_dir_all(batch.join("files/pictures")).unwrap();
    fs::write(batch.join("files/pictures/p.jpg"), b"untrusted").unwrap();
    fs::write(
        batch.join("manifest.json"),
        r#"{"items":[{"url":"https://example.invalid/evil","staged_path":"files/pictures/p.jpg","final_path":"../../outside.jpg"}]}"#,
    )
    .unwrap();
    sqlx::query("INSERT INTO legacy_imports(source_path,snapshot_fingerprint,source_kind,status,batch_dir,created_at,updated_at) VALUES('untrusted','untrusted','rust_v1','partial_recoverable',?,'2026-01-01T00:00:00Z','2026-01-01T00:00:00Z')")
        .bind(&batch_name)
        .execute(&target)
        .await
        .unwrap();
    let outside = temp.path().join("outside.jpg");

    let summary = import_legacy_source(
        &target,
        LegacyImportRequest::new(source, target_root.join("media"), imports),
    )
    .await
    .unwrap();

    assert_eq!(summary.status, LegacyImportStatus::Completed);
    assert!(!outside.exists(), "untrusted manifest escaped media root");
    assert!(
        batch.join("manifest.json").is_file(),
        "batch is retained for diagnosis"
    );
    assert!(!batch.join("completed").exists());
    let status: String = sqlx::query_scalar("SELECT status FROM legacy_imports WHERE batch_dir=?")
        .bind(batch_name)
        .fetch_one(&target)
        .await
        .unwrap();
    assert_eq!(status, "partial_recoverable");
}

#[tokio::test]
async fn repeated_snapshot_returns_completed_without_new_backup_or_references() {
    let temp = tempdir().unwrap();
    let source = rust_v1_fixture(&temp.path().join("legacy")).await;
    let target_root = temp.path().join("next");
    let target = create_db_pool_with_url(target_root.join("weiback.db").to_str().unwrap())
        .await
        .unwrap();
    let request = || {
        LegacyImportRequest::new(
            source.clone(),
            target_root.join("media"),
            target_root.join("imports"),
        )
    };

    assert_eq!(
        import_legacy_source(&target, request())
            .await
            .unwrap()
            .status,
        LegacyImportStatus::Completed
    );
    let backups_before = fs::read_dir(target_root.join("imports")).unwrap().count();
    let duplicate = import_legacy_source(&target, request()).await.unwrap();

    assert_eq!(duplicate.status, LegacyImportStatus::AlreadyCompleted);
    assert_eq!(
        fs::read_dir(target_root.join("imports")).unwrap().count(),
        backups_before
    );
    let references: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM media_references")
        .fetch_one(&target)
        .await
        .unwrap();
    assert_eq!(references, 2);
}

#[tokio::test]
async fn import_does_not_add_empty_definition_when_owner_has_large_reference() {
    let temp = tempdir().unwrap();
    let source = rust_v1_fixture(&temp.path().join("legacy")).await;
    let target_root = temp.path().join("next");
    let target = create_db_pool_with_url(target_root.join("weiback.db").to_str().unwrap())
        .await
        .unwrap();
    sqlx::query("INSERT INTO media(url,media_type,status,created_at) VALUES('https://example.invalid/p.jpg','picture','pending','2026-01-01T00:00:00Z')")
        .execute(&target)
        .await
        .unwrap();
    sqlx::query("INSERT INTO media_references(media_id,owner_type,owner_id,definition,created_at) SELECT id,'post',42,'large','2026-01-01T00:00:00Z' FROM media WHERE url='https://example.invalid/p.jpg'")
        .execute(&target)
        .await
        .unwrap();

    import_legacy_source(
        &target,
        LegacyImportRequest::new(
            source,
            target_root.join("media"),
            target_root.join("imports"),
        ),
    )
    .await
    .unwrap();

    let definitions: Vec<String> = sqlx::query_scalar("SELECT definition FROM media_references WHERE owner_type='post' AND owner_id=42 AND media_id=(SELECT id FROM media WHERE url='https://example.invalid/p.jpg')")
        .fetch_all(&target)
        .await
        .unwrap();
    assert_eq!(definitions, vec!["large"]);
}

#[tokio::test]
async fn imports_complete_wal_source_snapshot_from_one_read_transaction() {
    let temp = tempdir().unwrap();
    let source = rust_v1_fixture(&temp.path().join("legacy")).await;
    let source_pool = open_fixture(&source).await;
    sqlx::query("PRAGMA journal_mode=WAL")
        .execute(&source_pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO users(id,screen_name) VALUES(9,'wal user')")
        .execute(&source_pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO posts VALUES(44,'wal post','m44',9,'2026-01-03T00:00:00Z',0,0,0,0)")
        .execute(&source_pool)
        .await
        .unwrap();
    source_pool.close().await;
    let target_root = temp.path().join("next");
    let target = create_db_pool_with_url(target_root.join("weiback.db").to_str().unwrap())
        .await
        .unwrap();

    let summary = import_legacy_source(
        &target,
        LegacyImportRequest::new(
            source,
            target_root.join("media"),
            target_root.join("imports"),
        ),
    )
    .await
    .unwrap();

    assert_eq!(summary.status, LegacyImportStatus::Completed);
    assert_eq!(summary.posts, 2);
    assert_eq!(summary.users, 2);
    let imported: Vec<(i64, i64)> =
        sqlx::query_as("SELECT id,uid FROM posts WHERE id IN (42,44) ORDER BY id")
            .fetch_all(&target)
            .await
            .unwrap();
    assert_eq!(imported, vec![(42, 7), (44, 9)]);
}

#[tokio::test]
async fn wal_source_change_has_a_new_snapshot_fingerprint() {
    let temp = tempdir().unwrap();
    let source = rust_v1_fixture(&temp.path().join("legacy")).await;
    let target_root = temp.path().join("next");
    let target = create_db_pool_with_url(target_root.join("weiback.db").to_str().unwrap())
        .await
        .unwrap();
    let request = || {
        LegacyImportRequest::new(
            source.clone(),
            target_root.join("media"),
            target_root.join("imports"),
        )
    };
    assert_eq!(
        import_legacy_source(&target, request())
            .await
            .unwrap()
            .status,
        LegacyImportStatus::Completed
    );

    let source_pool = open_fixture(&source).await;
    sqlx::query("PRAGMA journal_mode=WAL")
        .execute(&source_pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO users(id,screen_name) VALUES(10,'wal fingerprint')")
        .execute(&source_pool)
        .await
        .unwrap();

    let changed = import_legacy_source(&target, request()).await.unwrap();
    assert_eq!(changed.status, LegacyImportStatus::Completed);
    assert_eq!(changed.users, 2);
    source_pool.close().await;
}

#[tokio::test]
async fn concurrent_write_after_snapshot_fingerprint_is_imported_as_a_new_snapshot() {
    let temp = tempdir().unwrap();
    let source = rust_v1_fixture(&temp.path().join("legacy")).await;
    let source_pool = open_fixture(&source).await;
    sqlx::query("PRAGMA journal_mode=WAL")
        .execute(&source_pool)
        .await
        .unwrap();
    let target_root = temp.path().join("next");
    let target = create_db_pool_with_url(target_root.join("weiback.db").to_str().unwrap())
        .await
        .unwrap();
    let reached = Arc::new(Barrier::new(2));
    let resume = Arc::new(Barrier::new(2));
    let request = LegacyImportRequest::new(
        source.clone(),
        target_root.join("media"),
        target_root.join("imports"),
    )
    .with_fingerprint_gap_for_test(reached.clone(), resume.clone());
    let import_target = target.clone();
    let import = tokio::spawn(async move { import_legacy_source(&import_target, request).await });

    tokio::task::spawn_blocking(move || reached.wait())
        .await
        .unwrap();
    sqlx::query("INSERT INTO users(id,screen_name) VALUES(11,'concurrent wal user')")
        .execute(&source_pool)
        .await
        .unwrap();
    tokio::task::spawn_blocking(move || resume.wait())
        .await
        .unwrap();

    assert_eq!(import.await.unwrap().unwrap().users, 1);
    let retry = import_legacy_source(
        &target,
        LegacyImportRequest::new(
            source,
            target_root.join("media"),
            target_root.join("imports"),
        ),
    )
    .await
    .unwrap();
    assert_eq!(retry.status, LegacyImportStatus::Completed);
    assert_eq!(retry.users, 2);
    source_pool.close().await;
}
