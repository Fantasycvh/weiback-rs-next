//! 旧 Rust 数据库升级测试（P0-C 交付物 6）。
//!
//! 用真实旧 migration（前 3 个）构建旧库并写入用户/帖子/收藏，
//! 然后通过 `create_db_pool_with_url` 跑完整 migrate!() 升级，
//! 验证帖子/用户/收藏/FTS/媒体引用不丢失，且新列/新表就位。
use std::path::Path;

use sqlx::{Sqlite, SqlitePool, migrate::MigrateDatabase};
use tempfile::tempdir;

use weiback::storage::database::create_db_pool_with_url;

/// 用前 3 个旧 migration 构建旧库。
async fn build_legacy_db(db_url: &str, mig_dir: &Path) {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    let mut files: Vec<_> = std::fs::read_dir(&src)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    files.sort();
    // 前 3 个是旧 schema；第 4 个是 P0-C 扩展。
    assert!(files.len() >= 4, "expected at least 4 migrations");
    for f in files.iter().take(3) {
        let name = f.file_name().unwrap();
        std::fs::copy(f, mig_dir.join(name)).unwrap();
    }
    let legacy_migrator = sqlx::migrate::Migrator::new(mig_dir).await.unwrap();

    Sqlite::create_database(db_url).await.unwrap();
    let pool = SqlitePool::connect(db_url).await.unwrap();
    legacy_migrator.run(&pool).await.unwrap();
    pool.close().await;
}

/// 用截至 P0-C 的真实 migration 构建数据库。
async fn build_p0c_db(db_url: &str, mig_dir: &Path) {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    let mut files: Vec<_> = std::fs::read_dir(&src)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    files.sort();
    assert!(files.len() >= 5, "expected P2 migration after P0-C");
    for file in files.iter().take(4) {
        std::fs::copy(file, mig_dir.join(file.file_name().unwrap())).unwrap();
    }
    let migrator = sqlx::migrate::Migrator::new(mig_dir).await.unwrap();
    Sqlite::create_database(db_url).await.unwrap();
    let pool = SqlitePool::connect(db_url).await.unwrap();
    migrator.run(&pool).await.unwrap();
    pool.close().await;
}

/// 用截至 P2 Phase2 的真实 migration 构建数据库。
async fn build_phase2_db(db_url: &str, mig_dir: &Path) {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    let mut files: Vec<_> = std::fs::read_dir(&src)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .collect();
    files.sort();
    assert!(files.len() >= 6, "expected Phase3 migration after Phase2");
    for file in files.iter().take(5) {
        std::fs::copy(file, mig_dir.join(file.file_name().unwrap())).unwrap();
    }
    let migrator = sqlx::migrate::Migrator::new(mig_dir).await.unwrap();
    Sqlite::create_database(db_url).await.unwrap();
    let pool = SqlitePool::connect(db_url).await.unwrap();
    migrator.run(&pool).await.unwrap();
    pool.close().await;
}

#[tokio::test]
async fn legacy_db_upgrades_without_data_loss() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("legacy.sqlite");
    let db_url = db_path.to_str().unwrap();
    let mig_dir = dir.path().join("legacy_migrations");
    std::fs::create_dir_all(&mig_dir).unwrap();

    build_legacy_db(db_url, &mig_dir).await;

    // 写入旧数据：用户、帖子、收藏。
    {
        let pool = SqlitePool::connect(db_url).await.unwrap();
        sqlx::query(
            "INSERT INTO users (id, screen_name, profile_image_url, avatar_large, avatar_hd, domain, following, follow_me) \
             VALUES (10001, '小明', 'http://a/p.jpg', 'http://a/hd.jpg', 'http://a/hd.jpg', '', 0, 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO posts (id, text, mblogid, uid, created_at, attitudes_status, deleted, favorited, edit_count) \
             VALUES (1, '你好世界', 'abc123', 10001, '2026-01-01T00:00:00+08:00', 0, 0, 1, 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO favorited_posts (id, unfavorited) VALUES (1, 0)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO picture (id, url, definition, path, post_id, user_id) \
             VALUES (1, 'http://a/p.jpg', 'large', 'pictures/1.jpg', 1, NULL)",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;
    }

    // 升级：完整 migrate!()，触发迁移前备份。
    let pool = create_db_pool_with_url(db_url)
        .await
        .expect("upgrade succeeds");

    // 帖子/用户/收藏不丢失。
    let posts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM posts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(posts, 1, "posts preserved");
    let users: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(users, 1, "users preserved");
    let favs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM favorited_posts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(favs, 1, "favorites preserved");
    let pics: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM picture")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(pics, 1, "media references preserved");

    // 帖子正文保留。
    let text: String = sqlx::query_scalar("SELECT text FROM posts WHERE id=1")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(text, "你好世界");

    // 新列默认值就位。
    let content_status: String = sqlx::query_scalar("SELECT content_status FROM posts WHERE id=1")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(content_status, "complete");
    let is_long_text: i64 = sqlx::query_scalar("SELECT is_long_text FROM posts WHERE id=1")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(is_long_text, 0);

    // FTS 表仍存在且可查询。
    let fts: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='posts_fts'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(fts, 1, "posts_fts preserved");

    // 新实体表就位。
    for table in [
        "comments",
        "media",
        "monitored_users",
        "sync_jobs",
        "sync_runs",
        "sync_checkpoints",
        "processed_events",
    ] {
        let exists: i64 = sqlx::query_scalar(&format!(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='{table}'"
        ))
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(exists, 1, "table {table} created");
    }

    // 迁移前备份已生成。
    let backups: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains("-migrate-backup-"))
        .collect();
    assert_eq!(backups.len(), 1, "migration backup created");

    pool.close().await;
}

/// 升级时重复执行（无新 migration）幂等，数据不重复。
#[tokio::test]
async fn legacy_db_reopen_is_idempotent() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("legacy2.sqlite");
    let db_url = db_path.to_str().unwrap();
    let mig_dir = dir.path().join("legacy_migrations");
    std::fs::create_dir_all(&mig_dir).unwrap();

    build_legacy_db(db_url, &mig_dir).await;

    let pool = create_db_pool_with_url(db_url).await.unwrap();
    sqlx::query("INSERT INTO posts (id, text, mblogid, uid, created_at, attitudes_status, deleted, favorited, edit_count) VALUES (2, 'x', 'x1', NULL, '2026-01-01T00:00:00+08:00', 0, 0, 0, 0)")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;

    // 再次打开：无 pending migration，也不产生重复数据。
    let pool = create_db_pool_with_url(db_url).await.unwrap();
    let posts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM posts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(posts, 1, "no duplicated posts");
    pool.close().await;
}

/// 验证旧库升级后 posts_fts 触发器可正常同步新列写入。
#[tokio::test]
async fn legacy_db_fts_still_tracks_new_rows() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("legacy3.sqlite");
    let db_url = db_path.to_str().unwrap();
    let mig_dir = dir.path().join("legacy_migrations");
    std::fs::create_dir_all(&mig_dir).unwrap();

    build_legacy_db(db_url, &mig_dir).await;

    let pool = create_db_pool_with_url(db_url).await.unwrap();
    sqlx::query(
        "INSERT INTO posts (id, text, mblogid, uid, created_at, attitudes_status, deleted, favorited, edit_count) \
         VALUES (7, '模糊搜索目标', 'm7', NULL, '2026-01-01T00:00:00+08:00', 0, 0, 0, 0)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let fts: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM posts_fts WHERE posts_fts MATCH '模糊搜索目标'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(fts, 1, "fts tracks newly inserted rows after upgrade");
    pool.close().await;
}

#[tokio::test]
async fn p0c_running_history_is_normalized_before_unique_index() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("p0c-dirty.sqlite");
    let db_url = db_path.to_str().unwrap();
    let mig_dir = dir.path().join("p0c_migrations");
    std::fs::create_dir_all(&mig_dir).unwrap();
    build_p0c_db(db_url, &mig_dir).await;

    {
        let pool = SqlitePool::connect(db_url).await.unwrap();
        sqlx::query(
            "INSERT INTO sync_jobs(name,kind,priority,enabled,created_at) VALUES('legacy','collect_user_posts',0,1,'2026-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();
        for started in ["2026-01-01T00:00:01Z", "2026-01-01T00:00:02Z"] {
            sqlx::query("INSERT INTO sync_runs(job_id,status,started_at) VALUES(1,'running',?)")
                .bind(started)
                .execute(&pool)
                .await
                .unwrap();
        }
        pool.close().await;
    }

    let pool = create_db_pool_with_url(db_url)
        .await
        .expect("dirty P0-C database must upgrade");
    let running: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sync_runs WHERE status='running'")
        .fetch_one(&pool)
        .await
        .unwrap();
    let interrupted: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM sync_runs WHERE status='interrupted'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(running, 0);
    assert_eq!(interrupted, 2);
    let ownership: (Option<String>, Option<i64>, Option<i64>) = sqlx::query_as(
        "SELECT owner_token, lease_until_epoch, current_run_id FROM sync_jobs WHERE id=1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(ownership, (None, None, None));
}

#[tokio::test]
async fn phase2_database_upgrades_to_legacy_account_without_secrets() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("phase2.sqlite");
    let db_url = db_path.to_str().unwrap();
    let mig_dir = dir.path().join("phase2_migrations");
    std::fs::create_dir_all(&mig_dir).unwrap();
    build_phase2_db(db_url, &mig_dir).await;
    {
        let pool = SqlitePool::connect(db_url).await.unwrap();
        sqlx::query(
            "INSERT INTO monitored_users(uid,screen_name,refresh_strategy,enabled,created_at) \
             VALUES(123,'legacy-user','daily',1,'2026-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO sync_jobs(resource_key,name,kind,status,enabled,created_at) \
             VALUES('legacy:job','legacy','collect_user_posts','pending',1,'2026-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;
    }

    let pool = create_db_pool_with_url(db_url).await.unwrap();
    let legacy: (i64, String, String, bool) =
        sqlx::query_as("SELECT id,provider,session_ref,enabled FROM accounts WHERE uid='legacy'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(legacy.1, "legacy");
    assert!(!legacy.2.is_empty());
    assert!(!legacy.3);
    let monitored_account: i64 =
        sqlx::query_scalar("SELECT account_id FROM monitored_users WHERE uid=123")
            .fetch_one(&pool)
            .await
            .unwrap();
    let job_account: i64 =
        sqlx::query_scalar("SELECT account_id FROM sync_jobs WHERE resource_key='legacy:job'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((monitored_account, job_account), (legacy.0, legacy.0));
    let monitored_enabled: bool =
        sqlx::query_scalar("SELECT enabled FROM monitored_users WHERE uid=123")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!monitored_enabled);
    let archived_job: (String, bool, Option<String>) = sqlx::query_as(
        "SELECT status,enabled,last_error FROM sync_jobs WHERE resource_key='legacy:job'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(archived_job.0, "failed");
    assert!(!archived_job.1);
    assert!(
        archived_job
            .2
            .is_some_and(|error| error.contains("archived"))
    );
    let columns: Vec<String> = sqlx::query_scalar("SELECT name FROM pragma_table_info('accounts')")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert!(
        !columns.iter().any(|column| {
            matches!(column.as_str(), "cookie" | "token" | "password" | "secret")
        })
    );
    let unique_indexes: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pragma_index_list('monitored_users') WHERE [unique]=1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(unique_indexes >= 1);

    let account_column: (i64, Option<String>) = sqlx::query_as(
        "SELECT [notnull], dflt_value FROM pragma_table_info('sync_jobs') WHERE name='account_id'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(account_column, (1, None));
    let account_fk: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pragma_foreign_key_list('sync_jobs') \
         WHERE [table]='accounts' AND [from]='account_id'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(account_fk, 1);
    let invalid_account = sqlx::query(
        "INSERT INTO sync_jobs(resource_key,name,kind,status,enabled,created_at,account_id) \
         VALUES('invalid:account','invalid','collect_user_posts','failed',0,'x',999999)",
    )
    .execute(&pool)
    .await;
    assert!(invalid_account.is_err());
}

#[tokio::test]
async fn phase3_archives_only_active_legacy_jobs_and_preserves_terminal_history() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("phase2-terminal.sqlite");
    let db_url = db_path.to_str().unwrap();
    let mig_dir = dir.path().join("phase2-terminal-migrations");
    std::fs::create_dir_all(&mig_dir).unwrap();
    build_phase2_db(db_url, &mig_dir).await;
    {
        let pool = SqlitePool::connect(db_url).await.unwrap();
        for (resource, status, error) in [
            ("active", "pending", Some("old active error")),
            ("done", "completed", Some("completed detail")),
            ("cancel", "cancelled", None),
            ("failed", "failed", Some("original failure")),
        ] {
            sqlx::query(
                "INSERT INTO sync_jobs(resource_key,name,kind,status,enabled,created_at,last_error) \
                 VALUES(?,?,?, ?,1,?,?)",
            )
            .bind(resource)
            .bind(resource)
            .bind("collect_user_posts")
            .bind(status)
            .bind("2026-01-01T00:00:00Z")
            .bind(error)
            .execute(&pool)
            .await
            .unwrap();
        }
        pool.close().await;
    }
    let pool = create_db_pool_with_url(db_url).await.unwrap();
    let rows: Vec<(String, bool, Option<String>)> =
        sqlx::query_as("SELECT status,enabled,last_error FROM sync_jobs ORDER BY resource_key")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        rows[0],
        ("failed".into(), false, Some("old active error".into()))
    );
    assert_eq!(rows[1], ("cancelled".into(), true, None));
    assert_eq!(
        rows[2],
        ("completed".into(), true, Some("completed detail".into()))
    );
    assert_eq!(
        rows[3],
        ("failed".into(), true, Some("original failure".into()))
    );
}
