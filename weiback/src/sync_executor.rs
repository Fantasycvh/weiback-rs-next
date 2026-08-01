//! 持久同步任务执行器与进程内 worker registry。

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex, mpsc},
    time::Duration,
};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::SqlitePool;

use crate::{
    error::{Error, Result},
    sidecar::{
        CollectionRequest, CollectionStatus, CommandType, ExecutionControl,
        ExecutionControlRequest, PersistentExecution, Sidecar, SidecarError, SpawnOptions,
        run_collection_persistent,
    },
    storage::internal::entities::{
        AccountDto, ClaimRequest, FinishRunRequest, SyncJobStatus, cancel_sync_job,
        claim_next_sync_job_with_gates, create_sync_run_at, finish_sync_run_at, get_account,
        get_sync_job, interrupt_sync_run_for_shutdown_at, pause_sync_job,
        recover_interrupted_sync_run_at,
    },
};

const INITIAL_LEASE_DURATION: Duration = Duration::from_secs(30);

pub type AccountSpawnResolver = Arc<dyn Fn(&AccountDto) -> Result<SpawnOptions> + Send + Sync>;

/// Builds a resolver that injects the persisted session reference into a spawn template.
/// The session path is kept in the child environment and is never logged by the executor.
pub fn account_session_resolver(template: SpawnOptions) -> AccountSpawnResolver {
    Arc::new(move |account| {
        let global = crate::config::get_config();
        let config = global.read()?;
        let root = config
            .session_path
            .parent()
            .ok_or_else(|| Error::FormatError("configured session root is invalid".into()))?;
        resolve_account_session(account, root, &template)
    })
}

pub fn secure_account_session_resolver(
    session_root: PathBuf,
    template: SpawnOptions,
) -> AccountSpawnResolver {
    Arc::new(move |account| resolve_account_session(account, &session_root, &template))
}

fn resolve_account_session(
    account: &AccountDto,
    session_root: &Path,
    template: &SpawnOptions,
) -> Result<SpawnOptions> {
    if !account.enabled {
        return Err(Error::InconsistentTask("account is disabled".into()));
    }
    let bytes = account.session_ref.as_bytes();
    let has_windows_prefix = bytes.get(1) == Some(&b':')
        || account.session_ref.starts_with(r"\\")
        || account.session_ref.starts_with("//");
    if account.session_ref.is_empty()
        || account.session_ref.starts_with(['/', '\\'])
        || has_windows_prefix
    {
        return Err(Error::FormatError(
            "session reference must be relative".into(),
        ));
    }
    let mut normalized = PathBuf::new();
    for component in account.session_ref.split(['/', '\\']) {
        match component {
            "" | "." => {}
            ".." => return Err(Error::FormatError("session reference escapes root".into())),
            part => normalized.push(part),
        }
    }
    if normalized.as_os_str().is_empty() {
        return Err(Error::FormatError("session reference is empty".into()));
    }
    let root = std::fs::canonicalize(session_root)
        .map_err(|error| Error::FormatError(format!("session root is unavailable: {error}")))?;
    let path = std::fs::canonicalize(root.join(normalized))
        .map_err(|error| Error::FormatError(format!("session file is unavailable: {error}")))?;
    if !path.starts_with(&root) {
        return Err(Error::FormatError("session reference escapes root".into()));
    }
    let mut options = template.clone();
    options
        .env
        .retain(|(key, _)| key != "WEIBACK_COLLECTOR_SESSION_PATH");
    options.env.push((
        "WEIBACK_COLLECTOR_SESSION_PATH".to_string(),
        path.to_string_lossy().into_owned(),
    ));
    Ok(options)
}

/// Validates that an account's persisted session reference resolves inside the configured root.
pub fn validate_account_session(account: &AccountDto, session_root: &Path) -> Result<()> {
    let mut enabled = account.clone();
    enabled.enabled = true;
    resolve_account_session(&enabled, session_root, &SpawnOptions::default()).map(|_| ())
}

#[derive(Debug, Default)]
struct FinalizationBarrierState {
    entered: bool,
    released: bool,
}

/// 测试用 finalization 门闩，确定性覆盖 collector return 到数据库 finish 的窗口。
#[derive(Debug, Clone, Default)]
pub struct FinalizationBarrier {
    state: Arc<(Mutex<FinalizationBarrierState>, Condvar)>,
}

/// Test-only activation latch for the reservation-to-running transition.
#[doc(hidden)]
pub type ActivationBarrier = FinalizationBarrier;

impl FinalizationBarrier {
    pub fn new() -> Self {
        Self::default()
    }

    fn enter_and_wait(&self) -> Result<()> {
        let (lock, wake) = &*self.state;
        let mut state = lock.lock()?;
        state.entered = true;
        wake.notify_all();
        while !state.released {
            state = wake.wait(state)?;
        }
        Ok(())
    }

    pub fn wait_until_entered(&self) {
        let (lock, wake) = &*self.state;
        let Ok(mut state) = lock.lock() else {
            return;
        };
        while !state.entered {
            let Ok(next) = wake.wait(state) else {
                return;
            };
            state = next;
        }
    }

    pub fn release(&self) {
        let (lock, wake) = &*self.state;
        if let Ok(mut state) = lock.lock() {
            state.released = true;
            wake.notify_all();
        }
    }
}

#[derive(Debug)]
enum WorkerSlot {
    Starting {
        owner_token: String,
        generation: i64,
    },
    Running {
        pid: u32,
        control_tx: mpsc::Sender<ExecutionControlRequest>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActivationOutcome {
    Active,
    Shutdown,
}

/// 进程内每 job 独占 Sidecar 的注册表。
#[derive(Debug, Default)]
struct WorkerRegistryState {
    workers: HashMap<i64, WorkerSlot>,
    shutting_down: bool,
}

#[derive(Debug, Default)]
pub struct WorkerRegistry {
    state: Mutex<WorkerRegistryState>,
    changed: Condvar,
}

impl WorkerRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Prevents new reservations and returns a snapshot for database fencing.
    pub fn begin_shutdown(&self) -> Vec<i64> {
        match self.state.lock() {
            Ok(mut state) => {
                state.shutting_down = true;
                state.workers.keys().copied().collect()
            }
            Err(_) => Vec::new(),
        }
    }

    /// Requests that an already fenced worker stop at its next control poll.
    pub fn stop_fenced_job(&self, job_id: i64, timeout: Duration) -> ControlStopResult {
        self.stop(job_id, ExecutionControl::Pause, timeout)
    }

    fn reserve(&self, job_id: i64, owner_token: &str, generation: i64) -> Result<()> {
        let mut state = self.state.lock()?;
        if state.shutting_down {
            return Err(Error::InconsistentTask(
                "worker registry is shutting down".to_string(),
            ));
        }
        if state.workers.contains_key(&job_id) {
            return Err(Error::InconsistentTask(format!(
                "worker already registered for sync job {job_id}"
            )));
        }
        state.workers.insert(
            job_id,
            WorkerSlot::Starting {
                owner_token: owner_token.to_string(),
                generation,
            },
        );
        Ok(())
    }

    fn activate(
        &self,
        job_id: i64,
        owner_token: &str,
        generation: i64,
        pid: u32,
        control_tx: mpsc::Sender<ExecutionControlRequest>,
    ) -> Result<ActivationOutcome> {
        let mut state = self.state.lock()?;
        if state.shutting_down {
            return Ok(ActivationOutcome::Shutdown);
        }
        match state.workers.get(&job_id) {
            Some(WorkerSlot::Starting {
                owner_token: reserved_owner,
                generation: reserved_generation,
            }) if reserved_owner == owner_token && *reserved_generation == generation => {
                state
                    .workers
                    .insert(job_id, WorkerSlot::Running { pid, control_tx });
                self.changed.notify_all();
                Ok(ActivationOutcome::Active)
            }
            _ => Err(Error::InconsistentTask(format!(
                "worker reservation lost for sync job {job_id}"
            ))),
        }
    }

    fn unregister(&self, job_id: i64) {
        if let Ok(mut state) = self.state.lock() {
            state.workers.remove(&job_id);
            self.changed.notify_all();
        }
    }

    /// 返回已激活 worker 的 PID。
    pub fn pid(&self, job_id: i64) -> Option<u32> {
        let state = self.state.lock().ok()?;
        match state.workers.get(&job_id) {
            Some(WorkerSlot::Running { pid, .. }) => Some(*pid),
            _ => None,
        }
    }

    #[doc(hidden)]
    pub fn contains(&self, job_id: i64) -> bool {
        self.state
            .lock()
            .is_ok_and(|state| state.workers.contains_key(&job_id))
    }

    fn is_shutting_down(&self) -> bool {
        self.state
            .lock()
            .map(|state| state.shutting_down)
            .unwrap_or(true)
    }

    fn stop(&self, job_id: i64, action: ExecutionControl, timeout: Duration) -> ControlStopResult {
        let deadline = std::time::Instant::now() + timeout;
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(_) => return ControlStopResult::StopFailed("worker registry poisoned".into()),
        };
        let worker = loop {
            match state.workers.get(&job_id) {
                Some(WorkerSlot::Running { pid, control_tx }) => {
                    break Some((*pid, control_tx.clone()));
                }
                Some(WorkerSlot::Starting { .. }) => {
                    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                    if remaining.is_zero() {
                        return ControlStopResult::WorkerStarting;
                    }
                    match self.changed.wait_timeout(state, remaining) {
                        Ok((next, wait)) => {
                            state = next;
                            if wait.timed_out()
                                && matches!(
                                    state.workers.get(&job_id),
                                    Some(WorkerSlot::Starting { .. })
                                )
                            {
                                return ControlStopResult::WorkerStarting;
                            }
                        }
                        Err(_) => {
                            return ControlStopResult::StopFailed(
                                "worker registry poisoned".into(),
                            );
                        }
                    }
                }
                None => break None,
            }
        };
        drop(state);
        if let Some((pid, control_tx)) = worker {
            let (ack_tx, ack_rx) = mpsc::sync_channel(1);
            if control_tx
                .send(ExecutionControlRequest {
                    action,
                    ack: ack_tx,
                })
                .is_err()
            {
                return ControlStopResult::WorkerNotFound;
            }
            return match ack_rx
                .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
            {
                Ok(Ok(())) => ControlStopResult::Stopped { pid },
                Ok(Err(error)) => ControlStopResult::StopFailed(error),
                Err(_) => ControlStopResult::StopTimedOut { pid },
            };
        }
        ControlStopResult::WorkerNotFound
    }

    /// Closes registration and interrupts every current slot for restart recovery.
    pub fn shutdown_all(&self, timeout: Duration) -> WorkerShutdownSummary {
        let job_ids = match self.state.lock() {
            Ok(mut state) => {
                state.shutting_down = true;
                state.workers.keys().copied().collect::<Vec<_>>()
            }
            Err(_) => {
                return WorkerShutdownSummary {
                    workers: vec![WorkerShutdownOutcome {
                        job_id: 0,
                        worker_stop: ControlStopResult::StopFailed(
                            "worker registry poisoned".into(),
                        ),
                    }],
                };
            }
        };
        let deadline = std::time::Instant::now() + timeout;
        let workers = job_ids
            .into_iter()
            .map(|job_id| WorkerShutdownOutcome {
                job_id,
                worker_stop: self.stop(
                    job_id,
                    ExecutionControl::Shutdown,
                    deadline.saturating_duration_since(std::time::Instant::now()),
                ),
            })
            .collect();
        WorkerShutdownSummary { workers }
    }
}

struct WorkerRegistrationGuard {
    registry: Arc<WorkerRegistry>,
    job_id: i64,
}

impl WorkerRegistrationGuard {
    fn new(registry: Arc<WorkerRegistry>, job_id: i64) -> Self {
        Self { registry, job_id }
    }
}

impl Drop for WorkerRegistrationGuard {
    fn drop(&mut self) {
        self.registry.unregister(self.job_id);
    }
}

/// 数据库 fencing 后的进程停止事实。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", content = "detail", rename_all = "snake_case")]
pub enum ControlStopResult {
    Stopped { pid: u32 },
    WorkerNotFound,
    WorkerStarting,
    StopTimedOut { pid: u32 },
    StopFailed(String),
}

impl ControlStopResult {
    pub fn is_degraded(&self) -> bool {
        matches!(
            self,
            Self::WorkerStarting | Self::StopTimedOut { .. } | Self::StopFailed(_)
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerShutdownOutcome {
    pub job_id: i64,
    pub worker_stop: ControlStopResult,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerShutdownSummary {
    pub workers: Vec<WorkerShutdownOutcome>,
}

impl WorkerShutdownSummary {
    pub fn degraded(&self) -> bool {
        self.workers
            .iter()
            .any(|outcome| outcome.worker_stop.is_degraded())
    }
}

/// 一次持久任务执行结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobExecutionResult {
    pub job_id: i64,
    pub run_id: i64,
    pub status: String,
}

/// 最小持久 scheduler/executor：claim 一条、独占 Sidecar、原子 finish。
#[derive(Clone)]
pub struct JobExecutor {
    pool: SqlitePool,
    registry: Arc<WorkerRegistry>,
    spawn_options: Option<SpawnOptions>,
    event_timeout: Duration,
    finalization_barrier: Option<FinalizationBarrier>,
    activation_barrier: Option<ActivationBarrier>,
    account_resolver: Option<AccountSpawnResolver>,
    minimum_interval_secs: i64,
}

impl JobExecutor {
    pub fn new(pool: SqlitePool, registry: Arc<WorkerRegistry>) -> Self {
        Self {
            pool,
            registry,
            spawn_options: None,
            event_timeout: Duration::from_secs(60),
            finalization_barrier: None,
            activation_barrier: None,
            account_resolver: None,
            minimum_interval_secs: 0,
        }
    }

    pub fn with_spawn_options(
        pool: SqlitePool,
        registry: Arc<WorkerRegistry>,
        spawn_options: SpawnOptions,
    ) -> Self {
        Self {
            pool,
            registry,
            spawn_options: Some(spawn_options),
            event_timeout: Duration::from_secs(60),
            finalization_barrier: None,
            activation_barrier: None,
            account_resolver: None,
            minimum_interval_secs: 0,
        }
    }

    pub fn with_account_resolver(mut self, resolver: AccountSpawnResolver) -> Self {
        self.account_resolver = Some(resolver);
        self
    }

    pub fn with_minimum_interval_secs(mut self, seconds: i64) -> Self {
        self.minimum_interval_secs = seconds.max(0);
        self
    }

    pub async fn resolve_spawn_options(&self, job_id: i64) -> Result<SpawnOptions> {
        let job = get_sync_job(&self.pool, job_id)
            .await?
            .ok_or_else(|| Error::InconsistentTask(format!("sync job {job_id} not found")))?;
        if let Some(resolver) = &self.account_resolver {
            let account = get_account(&self.pool, job.account_id)
                .await?
                .ok_or_else(|| {
                    Error::InconsistentTask(format!("account {} not found", job.account_id))
                })?;
            if !account.enabled {
                return Err(Error::InconsistentTask("account is disabled".into()));
            }
            return resolver(&account);
        }
        self.spawn_options.clone().ok_or_else(|| {
            Error::InconsistentTask("sync executor has no sidecar spawn options".to_string())
        })
    }

    pub fn with_finalization_barrier(mut self, barrier: FinalizationBarrier) -> Self {
        self.finalization_barrier = Some(barrier);
        self
    }

    #[doc(hidden)]
    pub fn with_activation_barrier(mut self, barrier: ActivationBarrier) -> Self {
        self.activation_barrier = Some(barrier);
        self
    }

    pub async fn pause(&self, job_id: i64, timeout: Duration) -> Result<ControlStopResult> {
        pause_sync_job(&self.pool, job_id, &Utc::now().to_rfc3339()).await?;
        let registry = self.registry.clone();
        tokio::task::spawn_blocking(move || registry.stop(job_id, ExecutionControl::Pause, timeout))
            .await
            .map_err(|error| Error::Tokio(error.to_string()))
    }

    pub async fn cancel(&self, job_id: i64, timeout: Duration) -> Result<ControlStopResult> {
        cancel_sync_job(&self.pool, job_id, &Utc::now().to_rfc3339()).await?;
        let registry = self.registry.clone();
        tokio::task::spawn_blocking(move || {
            registry.stop(job_id, ExecutionControl::Cancel, timeout)
        })
        .await
        .map_err(|error| Error::Tokio(error.to_string()))
    }

    pub async fn run_next(&self) -> Result<Option<JobExecutionResult>> {
        let owner_token = crate::sidecar::protocol::new_uuid_v7();
        let now = Utc::now();
        let claimed = claim_next_sync_job_with_gates(
            &self.pool,
            &ClaimRequest {
                owner_token: owner_token.clone(),
                now_epoch: now.timestamp(),
                lease_until_epoch: now.timestamp()
                    + i64::try_from(INITIAL_LEASE_DURATION.as_secs()).unwrap_or(i64::MAX),
                claimed_at: now.to_rfc3339(),
            },
            self.minimum_interval_secs,
        )
        .await?;
        let Some(job) = claimed else {
            return Ok(None);
        };
        let run_id = create_sync_run_at(
            &self.pool,
            job.id,
            &owner_token,
            job.generation,
            now.timestamp(),
            &now.to_rfc3339(),
        )
        .await?
        .ok_or_else(|| Error::InconsistentTask("failed to create owned sync run".to_string()))?;
        ensure_owned(&self.pool, job.id, run_id, &owner_token, job.generation).await?;
        let options = match self.resolve_spawn_options(job.id).await {
            Ok(options) => options,
            Err(error) => {
                fail_owned_run(
                    &self.pool,
                    job.id,
                    run_id,
                    &owner_token,
                    job.generation,
                    &error.to_string(),
                )
                .await?;
                return Err(error);
            }
        };
        let request = match request_from_job(&job) {
            Ok(request) => request,
            Err(error) => {
                fail_owned_run(
                    &self.pool,
                    job.id,
                    run_id,
                    &owner_token,
                    job.generation,
                    &error.to_string(),
                )
                .await?;
                return Err(error);
            }
        };
        if let Err(error) = self.registry.reserve(job.id, &owner_token, job.generation) {
            if self.registry.is_shutting_down() {
                let summary = crate::sidecar::CollectionSummary {
                    status: CollectionStatus::Shutdown,
                    error: Some("application shutdown before sidecar startup".to_string()),
                    ..crate::sidecar::CollectionSummary::default()
                };
                let status =
                    interrupt_shutdown_run(&self.pool, &job, run_id, &owner_token, &summary)
                        .await?;
                return Ok(Some(JobExecutionResult {
                    job_id: job.id,
                    run_id,
                    status,
                }));
            }
            fail_owned_run(
                &self.pool,
                job.id,
                run_id,
                &owner_token,
                job.generation,
                &error.to_string(),
            )
            .await?;
            return Err(error);
        }
        let pool = self.pool.clone();
        let registry = self.registry.clone();
        let event_timeout = self.event_timeout;
        let finalization_barrier = self.finalization_barrier.clone();
        let activation_barrier = self.activation_barrier.clone();
        let registration = WorkerRegistrationGuard::new(registry.clone(), job.id);
        let worker = tokio::task::spawn_blocking(move || -> Result<JobExecutionResult> {
            let _registration = registration;
            let runtime = tokio::runtime::Handle::current();
            let handshake_registry = registry.clone();
            let (mut sidecar, _, _) =
                match Sidecar::spawn_with_handshake_cancellable(&options, || {
                    handshake_registry.is_shutting_down()
                }) {
                    Ok(sidecar) => sidecar,
                    Err(SidecarError::HandshakeCancelled) => {
                        let summary = crate::sidecar::CollectionSummary {
                            status: CollectionStatus::Shutdown,
                            error: Some("application shutdown during sidecar handshake".into()),
                            ..crate::sidecar::CollectionSummary::default()
                        };
                        let status = runtime.block_on(interrupt_shutdown_run(
                            &pool,
                            &job,
                            run_id,
                            &owner_token,
                            &summary,
                        ))?;
                        return Ok(JobExecutionResult {
                            job_id: job.id,
                            run_id,
                            status,
                        });
                    }
                    Err(error) => {
                        runtime.block_on(fail_owned_run(
                            &pool,
                            job.id,
                            run_id,
                            &owner_token,
                            job.generation,
                            &error.to_string(),
                        ))?;
                        return Err(Error::FormatError(error.to_string()));
                    }
                };
            if let Err(error) = runtime.block_on(ensure_owned(
                &pool,
                job.id,
                run_id,
                &owner_token,
                job.generation,
            )) {
                let _ = sidecar.kill_and_wait();
                return Err(error);
            }
            let (control_tx, control_rx) = mpsc::channel();
            if let Some(barrier) = activation_barrier {
                barrier.enter_and_wait()?;
            }
            match registry.activate(
                job.id,
                &owner_token,
                job.generation,
                sidecar.pid(),
                control_tx,
            ) {
                Ok(ActivationOutcome::Active) => {}
                Ok(ActivationOutcome::Shutdown) => {
                    sidecar
                        .kill_and_wait()
                        .map_err(|error| Error::FormatError(error.to_string()))?;
                    let summary = crate::sidecar::CollectionSummary {
                        status: CollectionStatus::Shutdown,
                        error: Some("application shutdown during sidecar startup".to_string()),
                        ..crate::sidecar::CollectionSummary::default()
                    };
                    let status = runtime.block_on(interrupt_shutdown_run(
                        &pool,
                        &job,
                        run_id,
                        &owner_token,
                        &summary,
                    ))?;
                    return Ok(JobExecutionResult {
                        job_id: job.id,
                        run_id,
                        status,
                    });
                }
                Err(error) => {
                    let _ = sidecar.kill_and_wait();
                    return Err(error);
                }
            }
            let mut execution = PersistentExecution {
                job_id: job.id,
                run_id,
                generation: job.generation,
                owner_token: &owner_token,
                checkpoint_stream: &job.resource_key,
                control_rx: &control_rx,
                poll_interval: Duration::from_millis(100),
                heartbeat_interval: Duration::from_millis(100),
                lease_duration: Duration::from_secs(10),
            };
            let summary = runtime.block_on(run_collection_persistent(
                &mut sidecar,
                &pool,
                &request,
                &mut execution,
                |_, _| {},
                event_timeout,
            ));
            let stopped = sidecar.kill_and_wait();
            stopped.map_err(|error| Error::FormatError(error.to_string()))?;
            let summary = match summary {
                Ok(summary) => summary,
                Err(error) => {
                    runtime.block_on(fail_owned_run(
                        &pool,
                        job.id,
                        run_id,
                        &owner_token,
                        job.generation,
                        &error.to_string(),
                    ))?;
                    return Err(error);
                }
            };
            if let Some(barrier) = finalization_barrier {
                barrier.enter_and_wait()?;
            }
            let status = runtime.block_on(finalize_execution(
                &pool,
                &control_rx,
                &job,
                run_id,
                &owner_token,
                summary,
            ))?;
            Ok(JobExecutionResult {
                job_id: job.id,
                run_id,
                status,
            })
        })
        .await
        .map_err(|error| Error::Tokio(error.to_string()))?;
        worker.map(Some)
    }
}

async fn finalize_execution(
    pool: &SqlitePool,
    control_rx: &mpsc::Receiver<ExecutionControlRequest>,
    job: &crate::storage::internal::entities::SyncJobDto,
    run_id: i64,
    owner_token: &str,
    summary: crate::sidecar::CollectionSummary,
) -> Result<String> {
    if let Some(status) = consume_final_control(control_rx) {
        return match status {
            ExecutionControl::Shutdown => {
                interrupt_shutdown_run(pool, job, run_id, owner_token, &summary).await
            }
            ExecutionControl::Pause | ExecutionControl::Cancel => {
                controlled_final_status(pool, job.id, status).await
            }
        };
    }
    if summary.status == CollectionStatus::RateLimited {
        let info = summary.rate_limit.as_ref().ok_or_else(|| {
            Error::InconsistentTask("rate-limited summary missing structured info".to_string())
        })?;
        let next = crate::rate_limit::finish_rate_limited_run(
            pool,
            &FinishRunRequest {
                job_id: job.id,
                run_id,
                owner_token: owner_token.to_string(),
                generation: job.generation,
                next_status: SyncJobStatus::Interrupted,
                finished_at: Utc::now().to_rfc3339(),
                stats_json: Some(
                    serde_json::json!({"fetched_count":summary.fetched_count,"pages":summary.pages})
                        .to_string(),
                ),
                error: Some("rate limited".to_string()),
            },
            info,
            Utc::now().timestamp(),
        )
        .await?;
        return Ok(if next >= 0 {
            "pending".to_string()
        } else {
            "failed".to_string()
        });
    }
    let request = FinishRunRequest {
        job_id: job.id,
        run_id,
        owner_token: owner_token.to_string(),
        generation: job.generation,
        next_status: match summary.status {
            CollectionStatus::Completed => SyncJobStatus::Completed,
            CollectionStatus::Failed => SyncJobStatus::Failed,
            CollectionStatus::Stopped => SyncJobStatus::Cancelled,
            CollectionStatus::Interrupted => SyncJobStatus::Interrupted,
            CollectionStatus::Shutdown => SyncJobStatus::Interrupted,
            CollectionStatus::Paused | CollectionStatus::Cancelled => {
                return current_job_status(pool, job.id).await;
            }
            CollectionStatus::RateLimited => unreachable!("handled above"),
        },
        finished_at: Utc::now().to_rfc3339(),
        stats_json: Some(
            serde_json::json!({"fetched_count":summary.fetched_count,"pages":summary.pages})
                .to_string(),
        ),
        error: summary.error.clone(),
    };
    if summary.status == CollectionStatus::Shutdown {
        return interrupt_shutdown_run(pool, job, run_id, owner_token, &summary).await;
    }
    if summary.status == CollectionStatus::Interrupted {
        if let Some(recovered) =
            recover_interrupted_sync_run_at(pool, &request, Utc::now().timestamp()).await?
        {
            return Ok(recovered.status.as_str().to_string());
        }
    } else if finish_sync_run_at(pool, &request, Utc::now().timestamp()).await? {
        return Ok(request.next_status.as_str().to_string());
    }

    let status = current_job_status(pool, job.id).await?;
    if matches!(status.as_str(), "paused" | "cancelled") {
        let _ = consume_final_control_wait(control_rx, Duration::from_secs(1));
        return Ok(status);
    }
    Err(Error::InconsistentTask(
        "sync finish lost ownership".to_string(),
    ))
}

fn consume_final_control(
    control_rx: &mpsc::Receiver<ExecutionControlRequest>,
) -> Option<ExecutionControl> {
    let request = control_rx.try_recv().ok()?;
    let action = request.action;
    let _ = request.ack.send(Ok(()));
    Some(action)
}

fn consume_final_control_wait(
    control_rx: &mpsc::Receiver<ExecutionControlRequest>,
    timeout: Duration,
) -> Option<ExecutionControl> {
    let request = control_rx.recv_timeout(timeout).ok()?;
    let action = request.action;
    let _ = request.ack.send(Ok(()));
    Some(action)
}

async fn controlled_final_status(
    pool: &SqlitePool,
    job_id: i64,
    action: ExecutionControl,
) -> Result<String> {
    let status = current_job_status(pool, job_id).await?;
    let expected = match action {
        ExecutionControl::Pause => "paused",
        ExecutionControl::Cancel => "cancelled",
        ExecutionControl::Shutdown => {
            return Err(Error::InconsistentTask(
                "shutdown control requires owned-run recovery".to_string(),
            ));
        }
    };
    if status != expected {
        return Err(Error::InconsistentTask(format!(
            "control {expected} acknowledged with job status {status}"
        )));
    }
    Ok(status)
}

async fn interrupt_shutdown_run(
    pool: &SqlitePool,
    job: &crate::storage::internal::entities::SyncJobDto,
    run_id: i64,
    owner_token: &str,
    summary: &crate::sidecar::CollectionSummary,
) -> Result<String> {
    let request = FinishRunRequest {
        job_id: job.id,
        run_id,
        owner_token: owner_token.to_string(),
        generation: job.generation,
        next_status: SyncJobStatus::Interrupted,
        finished_at: Utc::now().to_rfc3339(),
        stats_json: Some(
            serde_json::json!({"fetched_count":summary.fetched_count,"pages":summary.pages})
                .to_string(),
        ),
        error: summary
            .error
            .clone()
            .or_else(|| Some("application shutdown".to_string())),
    };
    if interrupt_sync_run_for_shutdown_at(pool, &request, Utc::now().timestamp()).await? {
        return Ok("pending".to_string());
    }
    current_job_status(pool, job.id).await
}

async fn current_job_status(pool: &SqlitePool, job_id: i64) -> Result<String> {
    Ok(get_sync_job(pool, job_id)
        .await?
        .ok_or_else(|| Error::InconsistentTask("sync job disappeared".to_string()))?
        .status)
}

async fn fail_owned_run(
    pool: &SqlitePool,
    job_id: i64,
    run_id: i64,
    owner_token: &str,
    generation: i64,
    message: &str,
) -> Result<()> {
    let _ = finish_sync_run_at(
        pool,
        &FinishRunRequest {
            job_id,
            run_id,
            owner_token: owner_token.to_string(),
            generation,
            next_status: SyncJobStatus::Failed,
            finished_at: Utc::now().to_rfc3339(),
            stats_json: None,
            error: Some(message.to_string()),
        },
        Utc::now().timestamp(),
    )
    .await?;
    Ok(())
}

async fn ensure_owned(
    pool: &SqlitePool,
    job_id: i64,
    run_id: i64,
    owner_token: &str,
    generation: i64,
) -> Result<()> {
    let owned: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sync_jobs WHERE id=? AND status='running' AND enabled=1 \
         AND current_run_id=? AND owner_token=? AND generation=? AND lease_until_epoch>? \
         AND EXISTS(SELECT 1 FROM accounts a WHERE a.id=sync_jobs.account_id AND a.enabled=1)",
    )
    .bind(job_id)
    .bind(run_id)
    .bind(owner_token)
    .bind(generation)
    .bind(Utc::now().timestamp())
    .fetch_one(pool)
    .await?;
    if owned != 1 {
        return Err(Error::InconsistentTask(format!(
            "sync job {job_id} ownership was fenced"
        )));
    }
    Ok(())
}

fn request_from_job(
    job: &crate::storage::internal::entities::SyncJobDto,
) -> Result<CollectionRequest> {
    let command_type = match job.kind.as_str() {
        "collect_user_posts" => CommandType::CollectUserPosts,
        "collect_comments" => CommandType::CollectComments,
        "collect_comment_replies" => CommandType::CollectCommentReplies,
        other => {
            return Err(Error::FormatError(format!(
                "unsupported persistent sync job kind: {other}"
            )));
        }
    };
    let payload = job
        .payload_json
        .as_deref()
        .map(serde_json::from_str::<Value>)
        .transpose()?
        .unwrap_or_else(|| Value::Object(Default::default()));
    let stream = if job.resource_key.starts_with("account:") {
        match command_type {
            CommandType::CollectUserPosts => payload
                .get("uid")
                .and_then(value_i64)
                .map(|uid| format!("user:{uid}:posts")),
            CommandType::CollectComments => payload
                .get("post_id")
                .and_then(value_i64)
                .map(|post_id| format!("post:{post_id}:comments")),
            CommandType::CollectCommentReplies => payload
                .get("post_id")
                .and_then(value_i64)
                .zip(payload.get("root_comment_id").and_then(value_i64))
                .map(|(post_id, root_id)| format!("post:{post_id}:comment:{root_id}:replies")),
            _ => None,
        }
        .ok_or_else(|| Error::FormatError("persistent job payload has no protocol stream".into()))?
    } else {
        job.resource_key.clone()
    };
    Ok(CollectionRequest {
        command_type,
        stream,
        payload,
    })
}

fn value_i64(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
}
