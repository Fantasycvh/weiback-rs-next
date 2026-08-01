//! Durable account/endpoint rate-limit calculations and queue application.

use std::sync::{Arc, Mutex};

use tokio::sync::Notify;

use sqlx::{SqliteConnection, SqlitePool};

use crate::{
    error::{Error, Result},
    sidecar::{RateLimitInfo, RateLimitScope},
    storage::internal::entities::{FinishRunRequest, get_sync_job},
};

pub fn retry_after_epoch(now_epoch: i64, retry_after_ms: u64) -> i64 {
    let seconds = retry_after_ms.saturating_add(999) / 1000;
    now_epoch.saturating_add(i64::try_from(seconds).unwrap_or(i64::MAX))
}

pub fn rate_limit_delay_secs(
    account_id: i64,
    endpoint_key: &str,
    level: i64,
    retry_after_ms: Option<u64>,
    cap: i64,
) -> i64 {
    let local = backoff_delay_secs(account_id, endpoint_key, level, cap);
    let upstream = retry_after_ms
        .map(|milliseconds| {
            i64::try_from(milliseconds.saturating_add(999) / 1000).unwrap_or(i64::MAX)
        })
        .unwrap_or(0);
    local.max(upstream)
}

#[derive(Debug, Default)]
struct BarrierState {
    entered: bool,
    released: bool,
}

#[derive(Debug, Clone, Default)]
pub struct PendingRateLimitBarrier {
    state: Arc<Mutex<BarrierState>>,
    entered: Arc<Notify>,
    released: Arc<Notify>,
}

impl PendingRateLimitBarrier {
    pub fn new() -> Self {
        Self::default()
    }

    async fn enter_and_wait(&self) -> Result<()> {
        {
            let mut state = self.state.lock()?;
            state.entered = true;
        }
        self.entered.notify_waiters();
        loop {
            if self.state.lock()?.released {
                break;
            }
            self.released.notified().await;
        }
        Ok(())
    }

    pub async fn wait_until_entered(&self) {
        loop {
            if self.state.lock().is_ok_and(|state| state.entered) {
                return;
            }
            self.entered.notified().await;
        }
    }

    pub fn release(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.released = true;
        }
        self.released.notify_waiters();
    }
}

/// Atomically persists the selected gate and ends the owned run while requeueing its job.
pub async fn finish_rate_limited_run(
    pool: &SqlitePool,
    request: &FinishRunRequest,
    info: &RateLimitInfo,
    now_epoch: i64,
) -> Result<i64> {
    let request = request.clone();
    let info = info.clone();
    crate::sqlite_write::with_immediate_transaction(pool, |conn| Box::pin(async move {
        let job = get_sync_job(&mut *conn, request.job_id)
            .await?
            .ok_or_else(|| Error::InconsistentTask(format!("sync job {} not found", request.job_id)))?;
        let gate_next = apply_limit_on_conn(conn, &job, &info, now_epoch, &request.finished_at).await?;
        let run = sqlx::query(
        "UPDATE sync_runs SET status='interrupted',finished_at=?,stats_json=?,error=?,updated_at=? \
         WHERE id=? AND job_id=? AND status='running' AND owner_token=? AND generation=?",
    )
    .bind(&request.finished_at)
    .bind(&request.stats_json)
    .bind(&request.error)
    .bind(&request.finished_at)
    .bind(request.run_id)
    .bind(request.job_id)
    .bind(&request.owner_token)
    .bind(request.generation)
        .execute(&mut *conn)
        .await?;
        let job_update: Option<String> = sqlx::query_scalar(
        "UPDATE sync_jobs SET recovery_count=recovery_count+1, \
         status=CASE WHEN recovery_count+1>=max_recovery_attempts THEN 'failed' ELSE 'pending' END, \
         available_at_epoch=MAX(available_at_epoch,?), \
         owner_token=NULL,current_run_id=NULL,lease_until_epoch=NULL,claimed_at=NULL,last_error=?,updated_at=? \
           WHERE id=? AND status='running' AND enabled=1 AND current_run_id=? AND owner_token=? AND generation=? \
           AND lease_until_epoch>? \
           AND EXISTS(SELECT 1 FROM accounts a WHERE a.id=sync_jobs.account_id AND a.enabled=1) \
          RETURNING status",
    )
    .bind(gate_next)
    .bind(&request.error)
    .bind(&request.finished_at)
    .bind(request.job_id)
    .bind(request.run_id)
    .bind(&request.owner_token)
    .bind(request.generation)
    .bind(now_epoch)
        .fetch_optional(&mut *conn)
        .await?;
        let Some(status) = job_update else {
            return Err(Error::InconsistentTask(
                "rate-limit finish lost ownership".into(),
            ));
        };
        if run.rows_affected() != 1 {
            return Err(Error::InconsistentTask(
                "rate-limit finish lost run ownership".into(),
            ));
        }
        Ok(if status == "failed" { -1 } else { gate_next })
    })).await
}

pub fn backoff_delay_secs(account_id: i64, endpoint_key: &str, level: i64, cap: i64) -> i64 {
    let exponent = u32::try_from(level.clamp(0, 20)).unwrap_or(20);
    let base = 1_i64
        .checked_shl(exponent)
        .unwrap_or(i64::MAX)
        .min(cap.max(1));
    let seed = format!("{account_id}:{endpoint_key}:{level}");
    let hash = seed.bytes().fold(0xcbf29ce484222325_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
    });
    let jitter_width = (base / 4).max(1);
    base.saturating_add(i64::try_from(hash % u64::try_from(jitter_width).unwrap_or(1)).unwrap_or(0))
        .min(cap.max(1))
}

pub async fn apply_rate_limit_to_pending_job(
    pool: &SqlitePool,
    job_id: i64,
    info: &RateLimitInfo,
    now_epoch: i64,
    updated_at: &str,
) -> Result<i64> {
    apply_rate_limit_to_pending_job_inner(pool, job_id, info, now_epoch, updated_at, None).await
}

pub async fn apply_rate_limit_to_pending_job_with_barrier(
    pool: &SqlitePool,
    job_id: i64,
    info: &RateLimitInfo,
    now_epoch: i64,
    updated_at: &str,
    barrier: &PendingRateLimitBarrier,
) -> Result<i64> {
    apply_rate_limit_to_pending_job_inner(pool, job_id, info, now_epoch, updated_at, Some(barrier))
        .await
}

async fn apply_rate_limit_to_pending_job_inner(
    pool: &SqlitePool,
    job_id: i64,
    info: &RateLimitInfo,
    now_epoch: i64,
    updated_at: &str,
    barrier: Option<&PendingRateLimitBarrier>,
) -> Result<i64> {
    if let Some(barrier) = barrier {
        barrier.enter_and_wait().await?;
    }
    let info = info.clone();
    let updated_at = updated_at.to_string();
    crate::sqlite_write::with_immediate_transaction(pool, |conn| {
        Box::pin(async move {
            let job = get_sync_job(&mut *conn, job_id)
                .await?
                .ok_or_else(|| Error::InconsistentTask(format!("sync job {job_id} not found")))?;
            if job.status != "pending" {
                return Err(Error::InconsistentTask(
                    "rate-limit target is not pending".into(),
                ));
            }
            let gate_next = apply_limit_on_conn(conn, &job, &info, now_epoch, &updated_at).await?;
            let updated = sqlx::query(
        "UPDATE sync_jobs SET status='pending',available_at_epoch=MAX(available_at_epoch,?), \
         updated_at=? WHERE id=? AND status='pending'",
    )
    .bind(gate_next)
        .bind(&updated_at)
    .bind(job_id)
        .execute(&mut *conn)
        .await?;
            if updated.rows_affected() != 1 {
                return Err(Error::InconsistentTask(
                    "rate-limit pending CAS lost".into(),
                ));
            }
            Ok(gate_next)
        })
    })
    .await
}

async fn apply_limit_on_conn(
    conn: &mut SqliteConnection,
    job: &crate::storage::internal::entities::SyncJobDto,
    info: &RateLimitInfo,
    now_epoch: i64,
    updated_at: &str,
) -> Result<i64> {
    if info.scope == RateLimitScope::Request {
        let level: i64 = sqlx::query_scalar(
            "UPDATE sync_jobs SET rate_limit_backoff_level=rate_limit_backoff_level+1 \
             WHERE id=? RETURNING rate_limit_backoff_level",
        )
        .bind(job.id)
        .fetch_one(&mut *conn)
        .await?;
        return Ok(now_epoch.saturating_add(rate_limit_delay_secs(
            job.account_id,
            &job.endpoint_key,
            level,
            info.retry_after_ms,
            3600,
        )));
    }
    let gate_key = if info.scope == RateLimitScope::Account {
        "__account__"
    } else {
        job.endpoint_key.as_str()
    };
    let level: i64 = sqlx::query_scalar(
        "INSERT INTO rate_limit_gates(account_id,endpoint_key,next_allowed_epoch,backoff_level,retry_after_epoch,updated_at,updated_at_epoch,revision) \
         VALUES(?,?,0,1,NULL,?,?,1) ON CONFLICT(account_id,endpoint_key) DO UPDATE SET \
         backoff_level=rate_limit_gates.backoff_level+1,updated_at=excluded.updated_at, \
         updated_at_epoch=MAX(rate_limit_gates.updated_at_epoch,excluded.updated_at_epoch), \
         revision=rate_limit_gates.revision+1 RETURNING backoff_level",
    )
    .bind(job.account_id)
    .bind(gate_key)
    .bind(updated_at)
    .bind(now_epoch)
    .fetch_one(&mut *conn)
    .await?;
    let proposed = now_epoch.saturating_add(rate_limit_delay_secs(
        job.account_id,
        gate_key,
        level,
        info.retry_after_ms,
        3600,
    ));
    sqlx::query(
        "UPDATE rate_limit_gates SET next_allowed_epoch=MAX(next_allowed_epoch,?), \
         retry_after_epoch=?,updated_at=?,updated_at_epoch=MAX(updated_at_epoch,?) \
         WHERE account_id=? AND endpoint_key=?",
    )
    .bind(proposed)
    .bind(info.retry_after_ms.map(|_| proposed))
    .bind(updated_at)
    .bind(now_epoch)
    .bind(job.account_id)
    .bind(gate_key)
    .execute(&mut *conn)
    .await?;
    combined_gate_epoch(conn, job.account_id, &job.endpoint_key).await
}

async fn combined_gate_epoch(
    conn: &mut SqliteConnection,
    account_id: i64,
    endpoint_key: &str,
) -> Result<i64> {
    Ok(sqlx::query_scalar(
        "SELECT COALESCE(MAX(next_allowed_epoch),0) FROM rate_limit_gates \
         WHERE account_id=? AND endpoint_key IN ('__account__',?)",
    )
    .bind(account_id)
    .bind(endpoint_key)
    .fetch_one(&mut *conn)
    .await?)
}

pub(crate) struct GateUpdate<'a> {
    pub account_id: i64,
    pub endpoint_key: &'a str,
    pub next_allowed_epoch: i64,
    pub backoff_level: i64,
    pub retry_after_epoch: Option<i64>,
    pub updated_at: &'a str,
    pub updated_at_epoch: i64,
}

pub(crate) async fn apply_gate_on_conn(
    conn: &mut SqliteConnection,
    update: &GateUpdate<'_>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO rate_limit_gates(account_id,endpoint_key,next_allowed_epoch,backoff_level,retry_after_epoch,updated_at,updated_at_epoch) \
         VALUES(?,?,?,?,?,?,?) ON CONFLICT(account_id,endpoint_key) DO UPDATE SET \
         next_allowed_epoch=MAX(rate_limit_gates.next_allowed_epoch,excluded.next_allowed_epoch), \
         backoff_level=MAX(rate_limit_gates.backoff_level,excluded.backoff_level), \
         retry_after_epoch=CASE WHEN excluded.next_allowed_epoch>=rate_limit_gates.next_allowed_epoch THEN excluded.retry_after_epoch ELSE rate_limit_gates.retry_after_epoch END, \
         updated_at=excluded.updated_at,updated_at_epoch=MAX(rate_limit_gates.updated_at_epoch,excluded.updated_at_epoch)",
    )
    .bind(update.account_id)
    .bind(update.endpoint_key)
    .bind(update.next_allowed_epoch)
    .bind(update.backoff_level)
    .bind(update.retry_after_epoch)
    .bind(update.updated_at)
    .bind(update.updated_at_epoch)
    .execute(&mut *conn)
    .await?;
    Ok(())
}
