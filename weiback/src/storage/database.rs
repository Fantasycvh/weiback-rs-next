//! This module handles the creation and management of the SQLite database connection pool,
//! including database file initialization and running migrations.
//!
//! It provides functions to create a database pool for both default application usage
//! and for custom database URLs, such as in-memory databases for testing.

use std::path::{Path, PathBuf};

use sqlx::{Sqlite, SqlitePool, migrate::MigrateDatabase, sqlite::SqlitePoolOptions};
use tracing::{error, info};

use crate::config::get_config;
use crate::error::{Error, Result};

/// Creates a database connection pool using the default database path specified in the application configuration.
///
/// This function initializes the database file if it doesn't exist and runs all pending migrations.
///
/// # Returns
///
/// A `Result` containing a `SqlitePool` on success, or an `Error` on failure.
pub async fn create_db_pool() -> Result<SqlitePool> {
    let db_path = get_config().read()?.db_path.clone();
    info!("Initializing database pool at path: {db_path:?}");
    create_db_pool_with_url(&db_path.to_string_lossy()).await
}

/// Creates a database connection pool for a given database URL.
///
/// If the `db_url` is not `":memory:"`, it checks for the existence of the database file.
/// If the file does not exist, it creates it and any necessary parent directories.
/// It then connects to the database and runs all pending migrations.
///
/// # Migration safety
///
/// When the database file already exists, a recoverable backup is created via
/// `VACUUM INTO` **before** running migrations. If migration fails, the original
/// database file is left untouched and the backup remains on disk for recovery.
///
/// # Arguments
///
/// * `db_url` - The URL of the SQLite database (e.g., `":memory:"` for an in-memory database, or a file path).
///
/// # Returns
///
/// A `Result` containing a `SqlitePool` on success, or an `Error` on failure.
pub async fn create_db_pool_with_url(db_url: &str) -> Result<SqlitePool> {
    let is_memory = db_url == ":memory:";
    let mut is_new_database = false;

    if !is_memory {
        let db_path = Path::new(db_url);
        if !db_path.exists() {
            info!("Database file not found at {db_path:?}. Creating new database...");
            if let Some(parent) = db_path.parent()
                && !parent.exists()
            {
                info!("Creating parent directory for database: {parent:?}");
                tokio::fs::create_dir_all(parent).await.inspect_err(|e| {
                    error!("create database parent directory {:?} failed: {e}", parent);
                })?;
            }

            Sqlite::create_database(db_url).await?;
            info!("Database file created.");
            is_new_database = true;
        }
    } else {
        info!("Initializing database pool in memory");
    }

    info!("Connecting to database and running migrations...");
    let db_pool = SqlitePoolOptions::new()
        .after_connect(|conn, _| {
            Box::pin(async move {
                sqlx::query("PRAGMA foreign_keys=ON")
                    .execute(&mut *conn)
                    .await?;
                sqlx::query("PRAGMA busy_timeout=5000")
                    .execute(&mut *conn)
                    .await?;
                Ok(())
            })
        })
        .connect(db_url)
        .await?;

    let migrator = sqlx::migrate!();
    // 只有确实会改变 schema 时才创建快照；普通 reopen 不应持续产生备份。
    let backup_path =
        if !is_memory && !is_new_database && has_pending_migrations(&db_pool, &migrator).await? {
            Some(backup_before_migration(&db_pool, Path::new(db_url)).await?)
        } else {
            None
        };

    run_migrations(&db_pool, migrator).await.inspect_err(|e| {
        error!(
            "Database migration failed: {e}. Original database preserved. Backup: {:?}",
            backup_path
        );
    })?;

    let recovered_at = chrono::Utc::now().to_rfc3339();
    let recovery = super::internal::entities::recover_interrupted_sync_jobs(
        &db_pool,
        chrono::Utc::now().timestamp(),
        &recovered_at,
    )
    .await?;
    if recovery.requeued > 0 || recovery.failed > 0 {
        info!(
            "Recovered interrupted sync jobs: requeued={}, failed={}",
            recovery.requeued, recovery.failed
        );
    }

    info!("Database connection and migration successful.");
    Ok(db_pool)
}

async fn has_pending_migrations(
    pool: &SqlitePool,
    migrator: &sqlx::migrate::Migrator,
) -> Result<bool> {
    let table_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='_sqlx_migrations')",
    )
    .fetch_one(pool)
    .await?;
    if !table_exists {
        return Ok(migrator.iter().next().is_some());
    }
    let applied: std::collections::HashSet<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations WHERE success=1")
            .fetch_all(pool)
            .await?
            .into_iter()
            .collect();
    Ok(migrator
        .iter()
        .any(|migration| !applied.contains(&migration.version)))
}

/// 在迁移前用 `VACUUM INTO` 创建一致性备份。
///
/// 备份文件名为 `<db-stem>-migrate-backup-<timestamp>.db`（同秒冲突时追加序号），
/// 与数据库同目录。`VACUUM INTO` 由 SQLite 保证一致性快照，不依赖未合并的 WAL，
/// 且不覆盖已存在的备份文件。
async fn backup_before_migration(db_pool: &SqlitePool, db_path: &Path) -> Result<PathBuf> {
    let stem = db_path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "weiback".to_string());
    let ts = chrono::Utc::now().format("%Y%m%d%H%M%S");
    let mut backup_path = db_path.with_file_name(format!("{stem}-migrate-backup-{ts}.db"));
    let mut n = 0;
    while backup_path.exists() {
        n += 1;
        backup_path = db_path.with_file_name(format!("{stem}-migrate-backup-{ts}-{n}.db"));
    }
    let target = backup_path.to_string_lossy().replace('\'', "''");
    sqlx::query(&format!("VACUUM INTO '{target}'"))
        .execute(db_pool)
        .await
        .map_err(|e| {
            error!("create migration backup {backup_path:?} failed: {e}");
            Error::DbError(format!("migration backup failed: {e}"))
        })?;
    info!("Created migration backup at {backup_path:?}");
    Ok(backup_path)
}

/// 运行给定的 migrator。
///
/// 拆分为独立函数以便测试注入自定义 migrator（如含坏 SQL 的目录）
/// 来验证迁移失败时原库保持可打开。
async fn run_migrations(db_pool: &SqlitePool, migrator: sqlx::migrate::Migrator) -> Result<()> {
    migrator.run(db_pool).await.map_err(|e| {
        error!("Database migration failed: {e}");
        Error::DbError(e.to_string())
    })?;
    Ok(())
}

#[cfg(test)]
mod local_tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_create_db_pool_with_url_memory() {
        let pool = create_db_pool_with_url(":memory:").await;
        assert!(pool.is_ok());
        let pool = pool.unwrap();
        // Verify that a table from migrations exists
        let res = sqlx::query_scalar::<Sqlite, i64>("SELECT COUNT(*) FROM posts")
            .fetch_one(&pool)
            .await;
        assert!(res.is_ok());
    }

    #[tokio::test]
    async fn test_create_db_pool_with_url_file() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("test_db.sqlite");
        let db_url = db_path.to_str().unwrap();

        assert!(!db_path.exists());

        let pool = create_db_pool_with_url(db_url).await;
        assert!(pool.is_ok());
        assert!(db_path.exists());

        let pool = pool.unwrap();
        // Verify that a table from migrations exists
        let res = sqlx::query_scalar::<Sqlite, i64>("SELECT COUNT(*) FROM posts")
            .fetch_one(&pool)
            .await;
        assert!(res.is_ok());

        // Clean up
        pool.close().await;
    }

    #[tokio::test]
    async fn test_create_db_pool_with_url_existing_file() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("existing_db.sqlite");
        let db_url = db_path.to_str().unwrap();

        // Create the database file manually before calling the function
        Sqlite::create_database(db_url).await.unwrap();
        assert!(db_path.exists());

        let pool = create_db_pool_with_url(db_url).await;
        assert!(pool.is_ok());
        assert!(db_path.exists()); // Should still exist

        let pool = pool.unwrap();
        // Verify that a table from migrations exists
        let res = sqlx::query_scalar::<Sqlite, i64>("SELECT COUNT(*) FROM posts")
            .fetch_one(&pool)
            .await;
        assert!(res.is_ok());

        // Clean up
        pool.close().await;
    }

    #[tokio::test]
    async fn test_create_db_pool_with_url_non_existent_parent_dir() {
        let dir = tempdir().unwrap();
        let non_existent_parent = dir.path().join("non_existent_parent");
        let db_path = non_existent_parent.join("test_db.sqlite");
        let db_url = db_path.to_str().unwrap();

        assert!(!non_existent_parent.exists());
        assert!(!db_path.exists());

        let pool = create_db_pool_with_url(db_url).await;
        assert!(pool.is_ok());
        assert!(non_existent_parent.exists()); // Parent should have been created
        assert!(db_path.exists());

        let pool = pool.unwrap();
        // Verify that a table from migrations exists
        let res = sqlx::query_scalar::<Sqlite, i64>("SELECT COUNT(*) FROM posts")
            .fetch_one(&pool)
            .await;
        assert!(res.is_ok());

        // Clean up
        pool.close().await;
    }

    /// 已有数据库文件但没有待执行迁移时不应创建备份。
    #[tokio::test]
    async fn test_existing_db_gets_migration_backup() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("backup_test.sqlite");
        let db_url = db_path.to_str().unwrap();

        // 第一次创建：全新库，无备份。
        let pool = create_db_pool_with_url(db_url)
            .await
            .expect("create new db");
        sqlx::query("INSERT INTO posts (id, text) VALUES (1, 'legacy data');")
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;

        // 第二次打开：没有 pending migration，不应重复快照。
        let pool = create_db_pool_with_url(db_url)
            .await
            .expect("reopen existing db");

        // 没有 pending migration，不应产生备份文件。
        let backups: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains("-migrate-backup-"))
            .collect();
        assert_eq!(backups.len(), 0, "no pending migration means no backup");

        // 原库数据应保留。
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM posts")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1);

        pool.close().await;
    }

    /// 迁移失败时原库保持可打开，备份保留。
    #[tokio::test]
    async fn test_migration_failure_preserves_original_db() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("fail_test.sqlite");
        let db_url = db_path.to_str().unwrap();

        // 建一个含数据的旧库。
        {
            Sqlite::create_database(db_url).await.unwrap();
            let pool = SqlitePool::connect(db_url).await.unwrap();
            sqlx::query("CREATE TABLE legacy (id INTEGER PRIMARY KEY, name TEXT);")
                .execute(&pool)
                .await
                .unwrap();
            pool.close().await;
        }

        // 构造一个会失败的 migrator：引用不存在的表。
        let mig_dir = dir.path().join("bad_migrations");
        std::fs::create_dir_all(&mig_dir).unwrap();
        std::fs::write(
            mig_dir.join("20260101000001_bad.sql"),
            "ALTER TABLE does_not_exist ADD COLUMN x TEXT;",
        )
        .unwrap();
        let bad_migrator = sqlx::migrate::Migrator::new(mig_dir).await.unwrap();

        let pool = SqlitePool::connect(db_url).await.unwrap();
        let backup = backup_before_migration(&pool, &db_path).await.unwrap();
        let err = run_migrations(&pool, bad_migrator).await;
        assert!(err.is_err(), "bad migration should fail");
        pool.close().await;

        // 原库仍可打开且数据完整。
        let reopen = SqlitePool::connect(db_url).await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM legacy")
            .fetch_one(&reopen)
            .await
            .unwrap();
        assert_eq!(count, 0);
        reopen.close().await;

        // 备份存在且可打开。
        assert!(backup.exists());
        let backup_pool = SqlitePool::connect(backup.to_str().unwrap()).await.unwrap();
        let backup_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM legacy")
            .fetch_one(&backup_pool)
            .await
            .unwrap();
        assert_eq!(backup_count, 0);
        backup_pool.close().await;
    }

    /// P4 迁移中 DDL 成功、后续 SQL 失败时，原库、迁移记录和备份均须保持一致。
    #[tokio::test]
    async fn test_failed_migration_rolls_back_ddl_and_allows_retry() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("p4_failure.sqlite");
        let db_url = db_path.to_str().unwrap();
        const VERSION: i64 = 20_260_802_150_001;

        Sqlite::create_database(db_url).await.unwrap();
        let pool = SqlitePool::connect(db_url).await.unwrap();
        sqlx::query("CREATE TABLE legacy_business (id INTEGER PRIMARY KEY, name TEXT NOT NULL);")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO legacy_business (id, name) VALUES (1, 'must survive');")
            .execute(&pool)
            .await
            .unwrap();

        let bad_dir = dir.path().join("bad_p4_migrations");
        std::fs::create_dir_all(&bad_dir).unwrap();
        std::fs::write(
            bad_dir.join("20260802150001_p4_probe.sql"),
            "ALTER TABLE legacy_business ADD COLUMN p4_probe TEXT;\nINSERT INTO missing_table VALUES (1);",
        )
        .unwrap();
        let bad_migrator = sqlx::migrate::Migrator::new(bad_dir).await.unwrap();

        let backup = backup_before_migration(&pool, &db_path).await.unwrap();
        assert!(run_migrations(&pool, bad_migrator).await.is_err());
        pool.close().await;

        let reopened = SqlitePool::connect(db_url).await.unwrap();
        let business_data: String =
            sqlx::query_scalar("SELECT name FROM legacy_business WHERE id = 1")
                .fetch_one(&reopened)
                .await
                .unwrap();
        assert_eq!(business_data, "must survive");
        let probe_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('legacy_business') WHERE name = 'p4_probe')",
        )
        .fetch_one(&reopened)
        .await
        .unwrap();
        assert!(!probe_exists, "failed migration must roll back its DDL");
        let failed_migration_record_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM _sqlx_migrations WHERE version = ?")
                .bind(VERSION)
                .fetch_one(&reopened)
                .await
                .unwrap();
        assert_eq!(failed_migration_record_count, 0);

        let backup_pool = SqlitePool::connect(backup.to_str().unwrap()).await.unwrap();
        let backup_data: String =
            sqlx::query_scalar("SELECT name FROM legacy_business WHERE id = 1")
                .fetch_one(&backup_pool)
                .await
                .unwrap();
        assert_eq!(backup_data, "must survive");
        let backup_probe_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('legacy_business') WHERE name = 'p4_probe')",
        )
        .fetch_one(&backup_pool)
        .await
        .unwrap();
        assert!(
            !backup_probe_exists,
            "backup must represent the pre-migration database"
        );
        let backup_migration_table_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = '_sqlx_migrations')",
        )
        .fetch_one(&backup_pool)
        .await
        .unwrap();
        assert!(
            !backup_migration_table_exists,
            "backup must not contain migration metadata created after the snapshot"
        );
        backup_pool.close().await;

        let retry_dir = dir.path().join("retry_p4_migrations");
        std::fs::create_dir_all(&retry_dir).unwrap();
        std::fs::write(
            retry_dir.join("20260802150001_p4_probe.sql"),
            "ALTER TABLE legacy_business ADD COLUMN p4_probe TEXT;",
        )
        .unwrap();
        let retry_migrator = sqlx::migrate::Migrator::new(retry_dir).await.unwrap();
        run_migrations(&reopened, retry_migrator).await.unwrap();

        let retried_probe_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('legacy_business') WHERE name = 'p4_probe')",
        )
        .fetch_one(&reopened)
        .await
        .unwrap();
        assert!(retried_probe_exists);
        let retried_data: String =
            sqlx::query_scalar("SELECT name FROM legacy_business WHERE id = 1")
                .fetch_one(&reopened)
                .await
                .unwrap();
        assert_eq!(retried_data, "must survive");
        let successful_migration_record_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM _sqlx_migrations WHERE version = ? AND success = 1",
        )
        .bind(VERSION)
        .fetch_one(&reopened)
        .await
        .unwrap();
        assert_eq!(successful_migration_record_count, 1);
        reopened.close().await;
    }
}
