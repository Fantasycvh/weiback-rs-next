//! SQLite immediate-write transactions with bounded busy retry.

use std::time::Duration;

use futures::future::BoxFuture;
use sqlx::{Sqlite, SqliteConnection, pool::PoolConnection};

use crate::error::{Error, Result};

const BUSY_RETRIES: usize = 8;

pub(crate) async fn with_immediate_transaction<T, F>(
    pool: &sqlx::SqlitePool,
    operation: F,
) -> Result<T>
where
    F: for<'c> FnOnce(&'c mut SqliteConnection) -> BoxFuture<'c, Result<T>>,
{
    let mut transaction = ImmediateTransaction::new(begin_immediate(pool).await?);
    match operation(transaction.connection()).await {
        Ok(value) => match sqlx::query("COMMIT")
            .execute(transaction.connection())
            .await
        {
            Ok(_) => {
                transaction.disarm();
                Ok(value)
            }
            Err(error) => {
                transaction.rollback().await;
                Err(error.into())
            }
        },
        Err(error) => {
            transaction.rollback().await;
            Err(error)
        }
    }
}

struct ImmediateTransaction {
    conn: Option<PoolConnection<Sqlite>>,
}

impl ImmediateTransaction {
    fn new(conn: PoolConnection<Sqlite>) -> Self {
        Self { conn: Some(conn) }
    }

    fn connection(&mut self) -> &mut SqliteConnection {
        self.conn.as_deref_mut().expect("transaction connection")
    }

    fn disarm(&mut self) {
        self.conn.take();
    }

    async fn rollback(&mut self) {
        if self.conn.is_none() {
            return;
        }
        if sqlx::query("ROLLBACK")
            .execute(self.connection())
            .await
            .is_ok()
        {
            self.disarm();
        }
    }
}

impl Drop for ImmediateTransaction {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.as_mut() {
            conn.close_on_drop();
        }
    }
}

async fn begin_immediate(pool: &sqlx::SqlitePool) -> Result<PoolConnection<Sqlite>> {
    for attempt in 0..=BUSY_RETRIES {
        let mut conn = pool.acquire().await?;
        match sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await {
            Ok(_) => return Ok(conn),
            Err(error) if is_busy(&error) && attempt < BUSY_RETRIES => {
                drop(conn);
                tokio::time::sleep(Duration::from_millis(5 * (attempt as u64 + 1))).await;
            }
            Err(error) => {
                let result = Err(error.into());
                let _ = conn.close().await;
                return result;
            }
        }
    }
    Err(Error::DbError(
        "failed to acquire SQLite immediate transaction".to_string(),
    ))
}

fn is_busy(error: &sqlx::Error) -> bool {
    match error {
        sqlx::Error::Database(database) => {
            matches!(database.code().as_deref(), Some("5") | Some("6"))
                || database.message().contains("database is locked")
                || database.message().contains("database table is locked")
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use futures::poll;
    use sqlx::sqlite::SqlitePoolOptions;

    use super::*;

    #[tokio::test]
    async fn failed_operation_rolls_back_before_single_connection_returns_to_pool() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect(":memory:")
            .await
            .unwrap();
        sqlx::query("CREATE TABLE guarded(value INTEGER CHECK(value > 0))")
            .execute(&pool)
            .await
            .unwrap();

        let failed = with_immediate_transaction(&pool, |conn| {
            Box::pin(async move {
                sqlx::query("INSERT INTO guarded(value) VALUES(1)")
                    .execute(&mut *conn)
                    .await?;
                sqlx::query("INSERT INTO guarded(value) VALUES(0)")
                    .execute(&mut *conn)
                    .await?;
                Ok(())
            })
        })
        .await;
        assert!(failed.is_err());

        with_immediate_transaction(&pool, |conn| {
            Box::pin(async move {
                sqlx::query("INSERT INTO guarded(value) VALUES(2)")
                    .execute(&mut *conn)
                    .await?;
                Ok(())
            })
        })
        .await
        .unwrap();
        let values: Vec<i64> = sqlx::query_scalar("SELECT value FROM guarded ORDER BY value")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(values, vec![2]);
    }

    #[tokio::test]
    async fn failed_commit_rolls_back_before_single_connection_returns_to_pool() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect(":memory:")
            .await
            .unwrap();
        sqlx::query("PRAGMA foreign_keys=ON")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("CREATE TABLE parent(id INTEGER PRIMARY KEY)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE child(parent_id INTEGER, FOREIGN KEY(parent_id) REFERENCES parent(id) \
             DEFERRABLE INITIALLY DEFERRED)",
        )
        .execute(&pool)
        .await
        .unwrap();

        let failed = with_immediate_transaction(&pool, |conn| {
            Box::pin(async move {
                sqlx::query("INSERT INTO child(parent_id) VALUES(1)")
                    .execute(&mut *conn)
                    .await?;
                Ok(())
            })
        })
        .await;
        assert!(failed.is_err());

        with_immediate_transaction(&pool, |conn| {
            Box::pin(async move {
                sqlx::query("INSERT INTO parent(id) VALUES(1)")
                    .execute(&mut *conn)
                    .await?;
                sqlx::query("INSERT INTO child(parent_id) VALUES(1)")
                    .execute(&mut *conn)
                    .await?;
                Ok(())
            })
        })
        .await
        .unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM child")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn cancelled_rollback_does_not_return_an_open_transaction_to_pool() {
        let directory = tempfile::tempdir().unwrap();
        let url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("cancel.db").display()
        );
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .unwrap();
        sqlx::query("CREATE TABLE guarded(value INTEGER)")
            .execute(&pool)
            .await
            .unwrap();

        let mut transaction = ImmediateTransaction::new(begin_immediate(&pool).await.unwrap());
        sqlx::query("INSERT INTO guarded(value) VALUES(1)")
            .execute(transaction.connection())
            .await
            .unwrap();
        let mut rollback = Box::pin(transaction.rollback());
        let rollback_pending = poll!(&mut rollback).is_pending();
        drop(rollback);
        assert_eq!(transaction.conn.is_some(), rollback_pending);
        drop(transaction);

        with_immediate_transaction(&pool, |conn| {
            Box::pin(async move {
                sqlx::query("INSERT INTO guarded(value) VALUES(2)")
                    .execute(&mut *conn)
                    .await?;
                Ok(())
            })
        })
        .await
        .unwrap();
        let values: Vec<i64> = sqlx::query_scalar("SELECT value FROM guarded ORDER BY value")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(values, vec![2]);
    }
}
