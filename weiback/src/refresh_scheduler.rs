//! Tiered monitored-user refresh scheduling and persistent queue driving.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use chrono::Utc;
use sqlx::SqlitePool;
use tracing::{info, warn};

use crate::{
    error::Result,
    storage::internal::entities::{RefreshTier, SyncJobSpec},
    sync_executor::{JobExecutionResult, JobExecutor},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefreshScheduleConfig {
    pub hot_interval_secs: i64,
    pub warm_interval_secs: i64,
    pub cold_interval_secs: i64,
    pub hot_jitter_secs: i64,
    pub warm_jitter_secs: i64,
    pub cold_jitter_secs: i64,
}

impl Default for RefreshScheduleConfig {
    fn default() -> Self {
        Self {
            hot_interval_secs: 15 * 60,
            warm_interval_secs: 6 * 60 * 60,
            cold_interval_secs: 24 * 60 * 60,
            hot_jitter_secs: 60,
            warm_jitter_secs: 10 * 60,
            cold_jitter_secs: 30 * 60,
        }
    }
}

impl RefreshScheduleConfig {
    pub fn interval_secs(self, tier: RefreshTier) -> i64 {
        match tier {
            RefreshTier::Hot => self.hot_interval_secs,
            RefreshTier::Warm => self.warm_interval_secs,
            RefreshTier::Cold => self.cold_interval_secs,
        }
    }

    pub fn jitter_secs(self, tier: RefreshTier) -> i64 {
        match tier {
            RefreshTier::Hot => self.hot_jitter_secs,
            RefreshTier::Warm => self.warm_jitter_secs,
            RefreshTier::Cold => self.cold_jitter_secs,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefreshScanSummary {
    pub enqueued: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchedulerTickSummary {
    pub refresh_enqueued: u64,
    pub executed: Option<JobExecutionResult>,
}

/// Drives due monitored users and at most one persistent job per tick.
#[derive(Clone)]
pub struct PersistentScheduler {
    pool: SqlitePool,
    executor: JobExecutor,
    refresh_config: RefreshScheduleConfig,
}

impl PersistentScheduler {
    pub fn new(
        pool: SqlitePool,
        executor: JobExecutor,
        refresh_config: RefreshScheduleConfig,
    ) -> Self {
        Self {
            pool,
            executor,
            refresh_config,
        }
    }

    pub async fn tick(&self) -> Result<SchedulerTickSummary> {
        self.tick_at(Utc::now().timestamp()).await
    }

    pub async fn tick_at(&self, now_epoch: i64) -> Result<SchedulerTickSummary> {
        let refresh = scan_due_monitored_users(&self.pool, now_epoch, &self.refresh_config).await?;
        let executed = self.executor.run_next().await?;
        Ok(SchedulerTickSummary {
            refresh_enqueued: refresh.enqueued,
            executed,
        })
    }

    pub async fn run(self, interval: Duration) {
        self.run_until_cancelled(
            interval,
            Arc::new(AtomicBool::new(false)),
            Arc::new(tokio::sync::Notify::new()),
        )
        .await;
    }

    pub async fn run_until_cancelled(
        self,
        interval: Duration,
        cancelled: Arc<AtomicBool>,
        wake: Arc<tokio::sync::Notify>,
    ) {
        let interval = interval.max(Duration::from_millis(100));
        let mut consecutive_failures = 0_u32;
        loop {
            if cancelled.load(Ordering::Acquire) {
                return;
            }
            match self.tick().await {
                Ok(summary) if summary.refresh_enqueued > 0 || summary.executed.is_some() => {
                    consecutive_failures = 0;
                    info!(
                        refresh_enqueued = summary.refresh_enqueued,
                        executed_job = summary.executed.as_ref().map(|run| run.job_id),
                        "persistent scheduler tick completed"
                    );
                }
                Ok(_) => consecutive_failures = 0,
                Err(error) => {
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    warn!(
                        consecutive_failures,
                        "persistent scheduler tick failed: {error}"
                    );
                }
            }
            let backoff = interval
                .checked_mul(1_u32 << consecutive_failures.min(6))
                .unwrap_or(Duration::from_secs(60))
                .min(Duration::from_secs(60));
            tokio::select! {
                biased;
                _ = wake.notified() => {
                    if cancelled.load(Ordering::Acquire) {
                        return;
                    }
                }
                _ = tokio::time::sleep(backoff) => {}
            }
        }
    }
}

/// Stable FNV-1a based jitter in `[-jitter_secs, +jitter_secs]`.
pub fn deterministic_refresh_jitter(
    account_id: i64,
    uid: i64,
    tier: RefreshTier,
    slot: i64,
    jitter_secs: i64,
) -> i64 {
    if jitter_secs <= 0 {
        return 0;
    }
    let input = format!("{account_id}:{uid}:{}:{slot}", tier.as_str());
    let hash = input.bytes().fold(0xcbf29ce484222325_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
    });
    let width = u64::try_from(jitter_secs.saturating_mul(2).saturating_add(1)).unwrap_or(u64::MAX);
    i64::try_from(hash % width).unwrap_or(0) - jitter_secs
}

/// Transactionally advances due rows before enqueueing their canonical jobs.
pub async fn scan_due_monitored_users(
    pool: &SqlitePool,
    now_epoch: i64,
    config: &RefreshScheduleConfig,
) -> Result<RefreshScanSummary> {
    let config = *config;
    crate::sqlite_write::with_immediate_transaction(pool, |conn| Box::pin(async move {
    let due: Vec<(i64, i64, RefreshTier, i64, i64)> = sqlx::query_as(
        "SELECT m.account_id,m.uid,m.tier,m.interval_secs,m.jitter_secs FROM monitored_users m \
         JOIN accounts a ON a.id=m.account_id AND a.enabled=1 \
         WHERE m.enabled=1 AND m.next_refresh_epoch<=? ORDER BY m.next_refresh_epoch,m.account_id,m.uid",
    )
    .bind(now_epoch)
    .fetch_all(&mut *conn)
    .await?;
    let mut enqueued = 0;
    for (account_id, uid, tier, configured_interval, configured_jitter) in due {
        let interval = if configured_interval > 0 {
            configured_interval
        } else {
            config.interval_secs(tier)
        };
        let jitter = if configured_jitter > 0 {
            configured_jitter
        } else {
            config.jitter_secs(tier)
        };
        let slot = if interval > 0 {
            now_epoch / interval
        } else {
            now_epoch
        };
        let next = now_epoch
            .saturating_add(interval)
            .saturating_add(deterministic_refresh_jitter(
                account_id, uid, tier, slot, jitter,
            ))
            .max(now_epoch.saturating_add(1));
        let resource_key = format!("account:{account_id}:user:{uid}:posts");
        let existing_status: Option<String> = sqlx::query_scalar(
            "SELECT status FROM sync_jobs WHERE resource_key=? \
             AND status IN ('pending','running','paused','interrupted') LIMIT 1",
        )
        .bind(&resource_key)
        .fetch_optional(&mut *conn)
        .await?;
        if matches!(existing_status.as_deref(), Some("paused" | "interrupted")) {
            continue;
        }
        let advanced = sqlx::query(
            "UPDATE monitored_users SET next_refresh_epoch=?,last_refresh_epoch=?,updated_at=? \
             WHERE account_id=? AND uid=? AND enabled=1 AND next_refresh_epoch<=?",
        )
        .bind(next)
        .bind(now_epoch)
        .bind(now_epoch.to_string())
        .bind(account_id)
        .bind(uid)
        .bind(now_epoch)
        .execute(&mut *conn)
        .await?;
        if advanced.rows_affected() != 1 {
            continue;
        }
        crate::storage::internal::entities::enqueue_sync_job_spec_on_conn(
             conn,
            &SyncJobSpec::CollectUserPosts {
                account_id,
                uid,
                max_pages: None,
                priority: match tier {
                    RefreshTier::Hot => 30,
                    RefreshTier::Warm => 20,
                    RefreshTier::Cold => 10,
                },
            },
            now_epoch,
            &now_epoch.to_string(),
        )
        .await?;
        enqueued += 1;
    }
    Ok(RefreshScanSummary { enqueued })
    })).await
}
