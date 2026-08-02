//! The `core` module is the heart of the application, coordinating task execution,
//! user session management, and high-level operations.
//!
//! It provides the [`Core`] struct, which serves as the primary interface for the
//! frontend (via Tauri or CLI) to trigger actions like backing up posts, exporting
//! data, and managing login states.
//!
//! Key components within this module include:
//! - [`TaskHandler`]: Implements the specific logic for various backup and export tasks.
//! - [`TaskManager`]: Tracks the status and progress of currently running tasks.
//! - [`PostProcesser`]: Handles the downloading of media and insertion of posts into storage.

pub mod post_processer;
pub mod task;
pub mod task_handler;
pub mod task_manager;

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::sync::{
    Arc, Mutex, MutexGuard,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use tokio::{spawn, task::JoinHandle};
use tracing::{error, info, warn};
use weibosdk_rs::{ApiClient as SdkApiClient, api_client::LoginState, session::Session};

#[cfg(not(feature = "dev-mode"))]
use crate::api::DefaultApiClient;
#[cfg(feature = "dev-mode")]
use crate::api::DevApiClient;
use crate::config::get_config;
use crate::error::Result;
use crate::exporter::ExporterImpl;
use crate::media_downloader::{
    DownloaderWorkerSummary, DownloaderWorkerTask, MediaDownloaderHandle,
    MediaDownloaderStatusListener,
};
use crate::media_pipeline::{
    MediaPipeline, MediaPipelineConfig, MediaWorkerSummary, MediaWorkerTask,
};
use crate::models::User;
use crate::refresh_scheduler::{PersistentScheduler, RefreshScheduleConfig};
use crate::sidecar::{
    CollectionRequest, CollectionStatus, CommandType, DEFAULT_HANDSHAKE_TIMEOUT, Sidecar,
    collection_spawn_options, run_collection_interruptible,
};
use crate::storage::StorageImpl;
use crate::storage::internal::entities::{
    AccountDto, JobControlResult, MonitoredUserDto, OwnerMediaDto, SyncJobDto, SyncJobSpec,
    SyncRunDto, delete_account, delete_monitored_user, enqueue_sync_job_spec, get_accounts,
    get_comments, get_monitored_users, get_owner_media, get_rate_limit_gates, get_sync_job,
    get_sync_jobs, get_sync_run_history, recover_interrupted_sync_jobs, resume_sync_job,
    retry_media, retry_sync_job, save_account, save_monitored_user,
};
use crate::sync_executor::{
    ControlStopResult, JobExecutor, WorkerRegistry, WorkerShutdownSummary,
    account_session_resolver, validate_account_session,
};
use crate::user_backup::{
    UserBackupPaths, UserBackupSummary, UserBackupVerification, UserRestoreSummary,
    create_user_backup, list_user_backups, preflight_restore_user_backup, restore_user_backup,
    verify_user_backup,
};
pub use task::{
    BackupFavoritesOptions, BackupUserPostsOptions, CleanupInvalidPostsOptions, DeletePostOptions,
    ExportJobOptions, PaginatedPostInfo, PostQuery, TaskContext, TaskRequest, UserPostFilter,
};
pub use task_handler::TaskHandler;
pub use task_manager::{Task, TaskError, TaskEventListener, TaskManager, TaskStatus, TaskType};

/// Runs a short-lived task and logs the error if it fails.
///
/// This provides consistent error logging for short tasks, mirroring the pattern
/// used by long tasks in [`handle_task_request`].
macro_rules! run_short_task {
    ($self:expr, $task_name:expr, $expr:expr) => {{
        match $expr.await {
            Ok(ret) => Ok(ret),
            Err(e) => {
                error!(concat!($task_name, " failed: {}"), e);
                Err(e)
            }
        }
    }};
}

#[cfg(not(feature = "dev-mode"))]
type TH = TaskHandler<DefaultApiClient, StorageImpl, ExporterImpl, MediaDownloaderHandle>;
#[cfg(feature = "dev-mode")]
type TH = TaskHandler<DevApiClient, StorageImpl, ExporterImpl, MediaDownloaderHandle>;

#[cfg(feature = "dev-mode")]
type CurrentSdkApiClient = SdkApiClient<crate::dev_client::DevClient>;
#[cfg(not(feature = "dev-mode"))]
type CurrentSdkApiClient = SdkApiClient<weibosdk_rs::Client>;

/// The main application engine that orchestrates all services.
///
/// `Core` maintains the state of running tasks and provides high-level methods for
/// interacting with Weibo APIs and local storage. It is typically wrapped in an
/// [`Arc`] and shared across the application.
pub struct Core {
    next_task_id: AtomicU64,
    task_handler: Arc<TH>,
    task_manager: Arc<TaskManager>,
    sdk_api_client: Arc<CurrentSdkApiClient>,
    db_pool: SqlitePool,
    persistent_workers: Arc<WorkerRegistry>,
    shutdown_admission: ShutdownAdmission,
    persistent_scheduler: Mutex<PersistentSchedulerLifecycle>,
    ad_hoc_collection: Mutex<Option<AdHocCollectionTask>>,
    media_pipeline: MediaPipeline,
    media_worker: Mutex<Option<MediaWorkerTask>>,
    legacy_media_downloader: MediaDownloaderHandle,
    legacy_media_worker: Mutex<Option<DownloaderWorkerTask>>,
    long_task: Mutex<Option<LongTask>>,
}

#[derive(Default)]
struct ShutdownAdmission {
    shutting_down: AtomicBool,
    active_writers: AtomicU64,
    gate: Mutex<()>,
}

struct WriteAdmission<'a> {
    admission: &'a ShutdownAdmission,
}

impl Drop for WriteAdmission<'_> {
    fn drop(&mut self) {
        self.admission
            .active_writers
            .fetch_sub(1, Ordering::Release);
    }
}

impl ShutdownAdmission {
    fn enter(&self) -> Result<MutexGuard<'_, ()>> {
        if self.shutting_down.load(Ordering::Acquire) {
            return Err(crate::error::Error::InconsistentTask(
                "core is shutting down".to_string(),
            ));
        }
        let gate = self.gate.lock()?;
        if self.shutting_down.load(Ordering::Acquire) {
            return Err(crate::error::Error::InconsistentTask(
                "core is shutting down".to_string(),
            ));
        }
        Ok(gate)
    }

    fn begin_shutdown(&self) {
        let _gate = self.gate.lock().unwrap_or_else(|error| error.into_inner());
        self.shutting_down.store(true, Ordering::Release);
    }

    fn begin_write(&self) -> Result<WriteAdmission<'_>> {
        let _gate = self.gate.lock()?;
        if self.shutting_down.load(Ordering::Acquire) {
            return Err(crate::error::Error::InconsistentTask(
                "core is shutting down".to_string(),
            ));
        }
        self.active_writers.fetch_add(1, Ordering::AcqRel);
        Ok(WriteAdmission { admission: self })
    }

    fn has_active_writers(&self) -> bool {
        self.active_writers.load(Ordering::Acquire) != 0
    }

    fn end_shutdown(&self) {
        let _gate = self.gate.lock().unwrap_or_else(|error| error.into_inner());
        self.shutting_down.store(false, Ordering::Release);
    }
}

#[derive(Default)]
struct PersistentSchedulerLifecycle {
    task: Option<PersistentSchedulerTask>,
}

struct PersistentSchedulerTask {
    cancelled: Arc<AtomicBool>,
    wake: Arc<tokio::sync::Notify>,
    handle: JoinHandle<()>,
}

struct AdHocCollectionTask {
    cancelled: Arc<AtomicBool>,
    handle: JoinHandle<()>,
}

struct LongTask {
    task_id: u64,
    handle: JoinHandle<()>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SyncJobControlOutcome {
    pub job: SyncJobDto,
    pub worker_stop: ControlStopResult,
}

impl SyncJobControlOutcome {
    pub fn degraded(&self) -> bool {
        self.worker_stop.is_degraded()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SchedulerShutdownStatus {
    NotRunning,
    Stopped,
    TimedOut,
    JoinFailed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistentShutdownSummary {
    pub scheduler: SchedulerShutdownStatus,
    pub ad_hoc: SchedulerShutdownStatus,
    pub workers: WorkerShutdownSummary,
    pub database_failures: Vec<PersistentShutdownFailure>,
    pub media: MediaWorkerSummary,
    pub legacy_media: DownloaderWorkerSummary,
    pub long_task: SchedulerShutdownStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistentShutdownFailure {
    pub job_id: i64,
    pub error: String,
}

impl PersistentShutdownSummary {
    pub fn degraded(&self) -> bool {
        matches!(
            self.scheduler,
            SchedulerShutdownStatus::TimedOut | SchedulerShutdownStatus::JoinFailed
        ) || self.workers.degraded()
            || matches!(
                self.ad_hoc,
                SchedulerShutdownStatus::TimedOut | SchedulerShutdownStatus::JoinFailed
            )
            || !self.database_failures.is_empty()
            || !self.media.stopped
            || !self.legacy_media.stopped
            || matches!(
                self.long_task,
                SchedulerShutdownStatus::TimedOut | SchedulerShutdownStatus::JoinFailed
            )
    }
}

impl Core {
    /// Creates a new `Core` instance.
    ///
    /// This is an internal constructor used by `CoreBuilder`.
    pub(crate) fn new(
        task_handler: TH,
        sdk_api_client: Arc<CurrentSdkApiClient>,
        db_pool: SqlitePool,
        legacy_media_downloader: MediaDownloaderHandle,
        legacy_media_worker: DownloaderWorkerTask,
    ) -> Result<Self> {
        let config = get_config().read()?.clone();
        Ok(Self {
            next_task_id: AtomicU64::new(1),
            task_handler: Arc::new(task_handler),
            task_manager: Arc::new(TaskManager::new()),
            sdk_api_client,
            db_pool: db_pool.clone(),
            persistent_workers: Arc::new(WorkerRegistry::new()),
            shutdown_admission: ShutdownAdmission::default(),
            persistent_scheduler: Mutex::new(PersistentSchedulerLifecycle::default()),
            ad_hoc_collection: Mutex::new(None),
            media_pipeline: MediaPipeline::new(
                db_pool.clone(),
                reqwest::Client::new(),
                MediaPipelineConfig {
                    media_root: config.media_path,
                    max_bytes: config.media_max_bytes,
                    poll_interval: config.media_poll_interval,
                    allow_http: false,
                    allow_private_network: false,
                    max_redirects: 5,
                },
            ),
            media_worker: Mutex::new(None),
            legacy_media_downloader,
            legacy_media_worker: Mutex::new(Some(legacy_media_worker)),
            long_task: Mutex::new(None),
        })
    }

    fn persistent_executor(&self) -> Result<JobExecutor> {
        let session_path = get_config().read()?.session_path.clone();
        let options = collection_spawn_options(&session_path)
            .map_err(|error| crate::error::Error::FormatError(error.to_string()))?;
        Ok(JobExecutor::with_spawn_options(
            self.db_pool.clone(),
            self.persistent_workers.clone(),
            options.clone(),
        )
        .with_account_resolver(account_session_resolver(options)))
    }

    /// Starts the single background loop that scans monitored users and drains the queue.
    pub fn start_persistent_scheduler(&self) -> Result<bool> {
        let _admission = match self.shutdown_admission.enter() {
            Ok(admission) => admission,
            Err(crate::error::Error::InconsistentTask(_)) => return Ok(false),
            Err(error) => return Err(error),
        };
        let mut media_worker = self.media_worker.lock()?;
        if media_worker
            .as_ref()
            .is_some_and(MediaWorkerTask::is_finished)
        {
            media_worker.take();
        }
        if media_worker.is_none() {
            tokio::runtime::Handle::try_current().map_err(|error| {
                crate::error::Error::Tokio(format!(
                    "media worker requires an active Tokio runtime: {error}"
                ))
            })?;
            *media_worker = Some(MediaWorkerTask::spawn(self.media_pipeline.clone()));
        }
        drop(media_worker);
        let mut lifecycle = self.persistent_scheduler.lock()?;
        if lifecycle.task.is_some() {
            return Ok(false);
        }
        let executor = self.persistent_executor()?;
        let scheduler = PersistentScheduler::new(
            self.db_pool.clone(),
            executor,
            RefreshScheduleConfig::default(),
        );
        let cancelled = Arc::new(AtomicBool::new(false));
        let wake = Arc::new(tokio::sync::Notify::new());
        let runtime = tokio::runtime::Handle::try_current().map_err(|error| {
            crate::error::Error::Tokio(format!(
                "persistent scheduler requires an active Tokio runtime: {error}"
            ))
        })?;
        let handle = runtime.spawn(scheduler.run_until_cancelled(
            std::time::Duration::from_secs(1),
            cancelled.clone(),
            wake.clone(),
        ));
        lifecycle.task = Some(PersistentSchedulerTask {
            cancelled,
            wake,
            handle,
        });
        Ok(true)
    }

    pub async fn shutdown_persistent_tasks(
        &self,
        timeout: std::time::Duration,
    ) -> PersistentShutdownSummary {
        self.shutdown_admission.begin_shutdown();
        let wait_timeout =
            timeout.max(DEFAULT_HANDSHAKE_TIMEOUT + std::time::Duration::from_secs(3));
        let deadline = tokio::time::Instant::now() + wait_timeout;
        let legacy_media_deadline = tokio::time::Instant::now() + timeout;
        let scheduler_task = match self.persistent_scheduler.lock() {
            Ok(mut lifecycle) => lifecycle.task.take(),
            Err(_) => None,
        };
        if let Some(task) = &scheduler_task {
            task.cancelled.store(true, Ordering::Release);
            task.wake.notify_waiters();
        }
        let ad_hoc_task = self
            .ad_hoc_collection
            .lock()
            .ok()
            .and_then(|mut task| task.take());
        if let Some(task) = &ad_hoc_task {
            task.cancelled.store(true, Ordering::Release);
        }

        self.persistent_workers.begin_shutdown();
        let media_task = self
            .media_worker
            .lock()
            .ok()
            .and_then(|mut task| task.take());
        let legacy_media_task = self
            .legacy_media_worker
            .lock()
            .ok()
            .and_then(|mut task| task.take());
        let long_task = self.long_task.lock().ok().and_then(|mut task| task.take());
        if let Some(task) = &legacy_media_task {
            task.cancel();
        }
        if let Some(task) = &long_task {
            task.handle.abort();
            let _ = self.task_manager.cancel_for(task.task_id);
        }
        let database_failures = Vec::new();
        let registry = self.persistent_workers.clone();
        let worker_task = tokio::task::spawn_blocking(move || registry.shutdown_all(wait_timeout));
        let workers = match worker_task.await {
            Ok(summary) => summary,
            Err(error) => WorkerShutdownSummary {
                workers: vec![crate::sync_executor::WorkerShutdownOutcome {
                    job_id: 0,
                    worker_stop: ControlStopResult::StopFailed(error.to_string()),
                }],
            },
        };
        let scheduler = match scheduler_task {
            None => SchedulerShutdownStatus::NotRunning,
            Some(mut task) => match tokio::time::timeout_at(deadline, &mut task.handle).await {
                Ok(Ok(())) => SchedulerShutdownStatus::Stopped,
                Ok(Err(_)) => SchedulerShutdownStatus::JoinFailed,
                Err(_) => {
                    if let Ok(mut lifecycle) = self.persistent_scheduler.lock() {
                        lifecycle.task = Some(task);
                    }
                    SchedulerShutdownStatus::TimedOut
                }
            },
        };
        let ad_hoc = match ad_hoc_task {
            None => SchedulerShutdownStatus::NotRunning,
            Some(mut task) => match tokio::time::timeout_at(deadline, &mut task.handle).await {
                Ok(Ok(())) => SchedulerShutdownStatus::Stopped,
                Ok(Err(_)) => SchedulerShutdownStatus::JoinFailed,
                Err(_) => {
                    if let Ok(mut lifecycle) = self.ad_hoc_collection.lock() {
                        *lifecycle = Some(task);
                    }
                    SchedulerShutdownStatus::TimedOut
                }
            },
        };
        let media = match media_task {
            Some(task) => task.shutdown(wait_timeout).await,
            None => MediaWorkerSummary {
                stopped: true,
                join_failed: None,
            },
        };
        let legacy_media = match legacy_media_task {
            Some(mut task) => {
                let remaining = legacy_media_deadline
                    .checked_duration_since(tokio::time::Instant::now())
                    .unwrap_or_default();
                let summary = task.shutdown(remaining).await;
                if !summary.stopped
                    && let Ok(mut lifecycle) = self.legacy_media_worker.lock()
                {
                    *lifecycle = Some(task);
                }
                summary
            }
            None => DownloaderWorkerSummary {
                stopped: true,
                join_failed: None,
            },
        };
        let long_task = match long_task {
            None => SchedulerShutdownStatus::NotRunning,
            Some(mut task) => match tokio::time::timeout_at(deadline, &mut task.handle).await {
                Ok(Ok(())) | Ok(Err(_)) => SchedulerShutdownStatus::Stopped,
                Err(_) => {
                    if let Ok(mut lifecycle) = self.long_task.lock() {
                        *lifecycle = Some(task);
                    }
                    SchedulerShutdownStatus::TimedOut
                }
            },
        };
        PersistentShutdownSummary {
            scheduler,
            ad_hoc,
            workers,
            database_failures,
            media,
            legacy_media,
            long_task,
        }
    }

    pub fn set_legacy_media_downloader_status_listener(
        &self,
        listener: Box<dyn MediaDownloaderStatusListener>,
    ) {
        self.legacy_media_downloader.set_status_listener(listener);
    }

    pub async fn recover_persistent_tasks(&self) -> Result<()> {
        let now = chrono::Utc::now();
        let summary =
            recover_interrupted_sync_jobs(&self.db_pool, now.timestamp(), &now.to_rfc3339())
                .await?;
        if summary.requeued > 0 || summary.failed > 0 {
            info!(
                requeued = summary.requeued,
                failed = summary.failed,
                "recovered expired persistent tasks"
            );
        }
        Ok(())
    }

    fn user_backup_paths(&self) -> Result<UserBackupPaths> {
        let config = get_config().read()?.clone();
        let imports_dir = config
            .db_path
            .parent()
            .ok_or_else(|| {
                crate::error::Error::FormatError("configured data root is invalid".into())
            })?
            .join("imports");
        Ok(
            UserBackupPaths::new(config.db_path, config.media_path, imports_dir)
                .with_legacy_media_roots(config.picture_path, config.video_path),
        )
    }

    pub async fn create_user_backup(&self) -> Result<UserBackupSummary> {
        let _admission = self.shutdown_admission.begin_write()?;
        create_user_backup(&self.db_pool, &self.user_backup_paths()?).await
    }

    pub async fn list_user_backups(&self) -> Result<Vec<UserBackupSummary>> {
        list_user_backups(&self.user_backup_paths()?).await
    }

    pub async fn verify_user_backup(&self, id: &str) -> Result<UserBackupVerification> {
        verify_user_backup(&self.user_backup_paths()?, id).await
    }

    /// Stops all writers before swapping on-disk user data. A successful restore requires
    /// process restart because this Core intentionally retains no reopenable pool.
    pub async fn restore_user_backup(&self, id: &str) -> Result<UserRestoreSummary> {
        // Reject bad input before stopping workers or closing the live connection pool.
        verify_user_backup(&self.user_backup_paths()?, id).await?;
        preflight_restore_user_backup(&self.user_backup_paths()?, id).await?;
        self.shutdown_admission.begin_shutdown();
        if self.shutdown_admission.has_active_writers() || self.task_manager.has_active_task()? {
            self.shutdown_admission.end_shutdown();
            return Err(crate::error::Error::InconsistentTask(
                "cannot restore while an ordinary task or writer is active; cancel it and wait for completion".into(),
            ));
        }
        let shutdown = self
            .shutdown_persistent_tasks(std::time::Duration::from_secs(5))
            .await;
        if shutdown.degraded() {
            self.shutdown_admission.end_shutdown();
            return Err(crate::error::Error::InconsistentTask(
                "cannot restore while persistent workers are still stopping".into(),
            ));
        }
        self.db_pool.close().await;
        restore_user_backup(&self.user_backup_paths()?, id).await.map_err(|error| {
            crate::error::Error::InconsistentTask(format!(
                "restore failed after the live database pool was closed; restart required: {error}"
            ))
        })
    }

    pub async fn get_accounts(&self) -> Result<Vec<AccountDto>> {
        get_accounts(&self.db_pool).await
    }

    pub async fn save_account(&self, mut account: AccountDto) -> Result<i64> {
        let _admission = self.shutdown_admission.begin_write()?;
        let session_root = get_config()
            .read()?
            .session_path
            .parent()
            .ok_or_else(|| {
                crate::error::Error::FormatError("configured session root is invalid".into())
            })?
            .to_path_buf();
        validate_account_session(&account, &session_root)?;
        let now = chrono::Utc::now().to_rfc3339();
        if account.created_at.is_empty() {
            account.created_at = now.clone();
        }
        account.updated_at = Some(now);
        let id = save_account(&self.db_pool, &account).await?;
        if !account.enabled {
            let job_ids = get_sync_jobs(&self.db_pool)
                .await?
                .into_iter()
                .filter(|job| job.account_id == id && self.persistent_workers.contains(job.id))
                .map(|job| job.id)
                .collect::<Vec<_>>();
            for job_id in job_ids {
                let registry = self.persistent_workers.clone();
                let stopped = tokio::task::spawn_blocking(move || {
                    registry.stop_fenced_job(job_id, std::time::Duration::from_secs(5))
                })
                .await
                .map_err(|error| crate::error::Error::Tokio(error.to_string()))?;
                if stopped.is_degraded() {
                    warn!(job_id, result = ?stopped, "disabled account worker stop degraded");
                }
            }
        }
        Ok(id)
    }

    /// Reports whether an account points to a usable session inside the configured session root.
    pub fn account_session_ready(&self, account: &AccountDto) -> Result<bool> {
        let session_root = get_config()
            .read()?
            .session_path
            .parent()
            .map(std::path::Path::to_path_buf)
            .ok_or_else(|| {
                crate::error::Error::FormatError("configured session root is invalid".into())
            })?;
        Ok(account.enabled && validate_account_session(account, &session_root).is_ok())
    }

    pub async fn delete_account(&self, id: i64) -> Result<bool> {
        let _admission = self.shutdown_admission.begin_write()?;
        delete_account(&self.db_pool, id).await
    }

    pub async fn get_monitored_users(&self) -> Result<Vec<MonitoredUserDto>> {
        get_monitored_users(&self.db_pool).await
    }

    pub async fn get_owner_media(
        &self,
        owner_type: &str,
        owner_id: Option<i64>,
    ) -> Result<Vec<OwnerMediaDto>> {
        get_owner_media(&self.db_pool, owner_type, owner_id).await
    }

    /// Returns approved local bytes, or a bounded remote preview for an owned queued row.
    pub async fn get_media_blob(
        &self,
        owner_type: &str,
        owner_id: i64,
        media_id: i64,
    ) -> Result<Option<crate::storage::internal::entities::MediaBlob>> {
        if media_id <= 0 || owner_id <= 0 || !matches!(owner_type, "post" | "user" | "comment") {
            return Ok(None);
        }
        let config = get_config().read()?.clone();
        crate::storage::internal::entities::get_media_blob_with_preview(
            &self.db_pool,
            &config.media_path,
            &self.media_pipeline,
            owner_type,
            owner_id,
            media_id,
        )
        .await
    }

    pub async fn retry_media(&self, media_id: i64) -> Result<bool> {
        let _admission = self.shutdown_admission.begin_write()?;
        retry_media(&self.db_pool, media_id, &chrono::Utc::now().to_rfc3339()).await
    }

    pub async fn get_post_detail(&self, id: i64) -> Result<Option<task::PostInfo>> {
        self.task_handler.get_post_detail(id).await
    }

    pub async fn get_post_comments(
        &self,
        post_id: i64,
        root_id: Option<i64>,
        offset: u32,
        limit: u32,
    ) -> Result<task::PaginatedCommentWire> {
        get_comments(&self.db_pool, post_id, root_id, offset, limit).await
    }

    pub async fn get_sync_diagnostics(&self) -> Result<serde_json::Value> {
        let accounts = get_accounts(&self.db_pool).await?;
        let session_root = get_config()
            .read()?
            .session_path
            .parent()
            .map(std::path::Path::to_path_buf);
        let session_ready = session_root
            .as_deref()
            .map(|root| {
                accounts
                    .iter()
                    .filter(|account| {
                        account.enabled && validate_account_session(account, root).is_ok()
                    })
                    .count()
            })
            .unwrap_or(0);
        let media_counts =
            sqlx::query_as::<_, (String, i64)>("SELECT status,COUNT(*) FROM media GROUP BY status")
                .fetch_all(&self.db_pool)
                .await?;
        let mut media = serde_json::Map::new();
        for status in ["pending", "downloading", "failed", "downloaded"] {
            media.insert(
                status.into(),
                serde_json::json!(
                    media_counts
                        .iter()
                        .find(|(key, _)| key == status)
                        .map(|(_, count)| *count)
                        .unwrap_or(0)
                ),
            );
        }
        let gates = get_rate_limit_gates(&self.db_pool)
            .await?
            .into_iter()
            .map(|gate| {
                serde_json::json!({
                    "account_id": gate.account_id.to_string(), "endpoint": gate.endpoint_key,
                    "next_allowed": gate.next_allowed_epoch, "backoff": gate.backoff_level
                })
            })
            .collect::<Vec<_>>();
        let sidecar = tokio::task::spawn_blocking(|| {
            let Some(program) = crate::sidecar::supervisor::resolve_sidecar_command() else {
                return (
                    serde_json::json!({"healthy": false, "status": "missing", "version": null, "protocol_version": null}),
                    serde_json::json!({"installed": false, "version": null, "status": "unavailable"}),
                );
            };
            let options = crate::sidecar::SpawnOptions { program, handshake_timeout: std::time::Duration::from_secs(3), ..Default::default() };
            match crate::sidecar::Sidecar::spawn_with_handshake(&options) {
                Ok((mut sidecar, ready, capabilities)) => {
                    let shutdown = sidecar.shutdown(std::time::Duration::from_millis(500)).is_ok();
                    let installed = capabilities.get("browser_installed").and_then(serde_json::Value::as_bool).unwrap_or(false);
                    (
                    serde_json::json!({"healthy": shutdown, "status": if shutdown { "ok" } else { "degraded" },
                        "version": ready.get("sidecar_version").cloned().or_else(|| capabilities.get("sidecar_version").cloned()).unwrap_or(serde_json::Value::Null),
                        "protocol_version": ready.get("protocol_version").cloned().or_else(|| capabilities.get("protocol_version").cloned()).unwrap_or(serde_json::Value::Null)}),
                    serde_json::json!({"installed": installed,
                        "version": capabilities.get("browser_version").cloned().unwrap_or(serde_json::Value::Null),
                        "status": if installed { "ok" } else { "missing" }}))
                }
                Err(_) => (
                    serde_json::json!({"healthy": false, "status": "unhealthy", "version": null, "protocol_version": null}),
                    serde_json::json!({"installed": false, "version": null, "status": "unknown"}),
                ),
            }
        }).await.map_err(|error| crate::error::Error::Tokio(error.to_string()))?;
        Ok(serde_json::json!({
            "app": {"version": env!("CARGO_PKG_VERSION"), "health": "ok"},
            "sidecar": sidecar.0,
            "chromium": sidecar.1,
            "browser": sidecar.1,
            "accounts": {"total": accounts.len(), "enabled": accounts.iter().filter(|a| a.enabled).count(), "session_ready": session_ready},
            "media": media,
            "rate_gates": gates,
            "auth": if self.sdk_api_client.session().is_ok() {
                serde_json::json!({"ready": 1, "not_ready": 0})
            } else {
                serde_json::json!({"ready": 0, "not_ready": 1})
            }
        }))
    }

    pub async fn save_monitored_user(&self, mut user: MonitoredUserDto) -> Result<()> {
        let _admission = self.shutdown_admission.begin_write()?;
        let now = chrono::Utc::now().to_rfc3339();
        if user.created_at.is_empty() {
            user.created_at = now.clone();
        }
        user.updated_at = Some(now);
        save_monitored_user(&self.db_pool, &user).await
    }

    pub async fn delete_monitored_user(&self, account_id: i64, uid: i64) -> Result<bool> {
        let _admission = self.shutdown_admission.begin_write()?;
        delete_monitored_user(&self.db_pool, account_id, uid).await
    }

    pub async fn enqueue_sync_job(&self, spec: SyncJobSpec) -> Result<i64> {
        let _admission = self.shutdown_admission.begin_write()?;
        let now = chrono::Utc::now();
        enqueue_sync_job_spec(&self.db_pool, &spec, now.timestamp(), &now.to_rfc3339()).await
    }

    pub async fn get_sync_jobs(&self) -> Result<Vec<SyncJobDto>> {
        get_sync_jobs(&self.db_pool).await
    }

    pub async fn get_sync_run_history(&self, job_id: i64, limit: u64) -> Result<Vec<SyncRunDto>> {
        get_sync_run_history(&self.db_pool, job_id, limit.min(1000)).await
    }

    pub async fn pause_sync_job(&self, job_id: i64) -> Result<SyncJobControlOutcome> {
        let _admission = self.shutdown_admission.begin_write()?;
        let worker_stop = JobExecutor::new(self.db_pool.clone(), self.persistent_workers.clone())
            .pause(job_id, std::time::Duration::from_secs(5))
            .await?;
        Ok(SyncJobControlOutcome {
            job: self.require_sync_job(job_id).await?,
            worker_stop,
        })
    }

    pub async fn cancel_sync_job(&self, job_id: i64) -> Result<SyncJobControlOutcome> {
        let _admission = self.shutdown_admission.begin_write()?;
        let worker_stop = JobExecutor::new(self.db_pool.clone(), self.persistent_workers.clone())
            .cancel(job_id, std::time::Duration::from_secs(5))
            .await?;
        Ok(SyncJobControlOutcome {
            job: self.require_sync_job(job_id).await?,
            worker_stop,
        })
    }

    pub async fn resume_sync_job(&self, job_id: i64) -> Result<SyncJobDto> {
        let _admission = self.shutdown_admission.begin_write()?;
        let now = chrono::Utc::now().to_rfc3339();
        let _: JobControlResult = resume_sync_job(&self.db_pool, job_id, &now).await?;
        self.require_sync_job(job_id).await
    }

    pub async fn retry_sync_job(&self, job_id: i64) -> Result<SyncJobDto> {
        let _admission = self.shutdown_admission.begin_write()?;
        let now = chrono::Utc::now().to_rfc3339();
        let _: JobControlResult = retry_sync_job(&self.db_pool, job_id, &now).await?;
        self.require_sync_job(job_id).await
    }

    async fn require_sync_job(&self, job_id: i64) -> Result<SyncJobDto> {
        get_sync_job(&self.db_pool, job_id).await?.ok_or_else(|| {
            crate::error::Error::InconsistentTask(format!("sync job {job_id} not found"))
        })
    }

    /// Retrieves the status of the currently active long-running task.
    ///
    /// # Returns
    /// A `Result` containing `Some(Task)` if a task is running or recently finished, or `None`.
    pub async fn get_current_task(&self) -> Result<Option<Task>> {
        self.task_manager.get_current()
    }

    /// Collects and removes all non-fatal task errors (e.g., download failures).
    ///
    /// This should be called periodically by the UI to report issues to the user.
    pub fn get_and_clear_task_errors(&self) -> Result<Vec<TaskError>> {
        self.task_manager.get_and_clear_task_errors()
    }

    /// Gets the Weibo UID of the currently logged-in user.
    ///
    /// # Errors
    /// Returns an error if no active session is found.
    pub fn get_my_uid(&self) -> Result<String> {
        Ok(self.sdk_api_client.session()?.uid.clone())
    }

    /// Retrieves a user's screen name from local storage by their UID.
    ///
    /// # Arguments
    /// * `uid` - The unique identifier of the user.
    pub async fn get_username_by_id(&self, uid: i64) -> Result<Option<String>> {
        self.task_handler
            .get_user(uid)
            .await
            .map(|opt| opt.map(|u| u.screen_name))
    }

    /// Searches for users in local storage whose screen name starts with the given prefix.
    pub async fn search_users_by_screen_name_prefix(&self, prefix: &str) -> Result<Vec<User>> {
        self.task_handler
            .search_users_by_screen_name_prefix(prefix)
            .await
    }

    /// Sets a listener for task-related events.
    ///
    /// This allows the UI or other components to receive real-time updates
    /// without polling.
    pub fn set_task_event_listener(&self, listener: Box<dyn TaskEventListener>) -> Result<()> {
        self.task_manager.set_listener(listener)
    }

    // ========================= login stuff =========================

    /// Requests an SMS login code for the specified phone number.
    ///
    /// # Arguments
    /// * `phone_number` - The phone number to send the code to (e.g., "13800138000").
    pub async fn get_sms_code(&self, phone_number: String) -> Result<()> {
        info!("SMS code requested");
        self.sdk_api_client
            .get_sms_code(phone_number)
            .await
            .inspect_err(|_| error!("SMS code request failed"))?;
        Ok(())
    }

    /// Completes the login process using an SMS code.
    ///
    /// This method updates the session, saves it to disk, and persists the logged-in
    /// user's information to local storage.
    ///
    /// # Arguments
    /// * `sms_code` - The code received via SMS.
    ///
    /// # Errors
    /// Returns an error if the login fails or if the system is not in the `WaitingForCode` state.
    pub async fn login(&self, sms_code: String) -> Result<User> {
        let _admission = self.shutdown_admission.begin_write()?;
        info!("login called with a sms_code");
        match self.sdk_api_client.login_state() {
            LoginState::WaitingForCode { .. } => {
                info!("Attempting to login with SMS code.");
                self.sdk_api_client
                    .login(&sms_code)
                    .await
                    .inspect_err(|_| error!("SDK login failed"))?;
                info!("Login successful.");
                let session_path = get_config()
                    .read()
                    .expect("config lock failed")
                    .session_path
                    .clone();
                let session = self.sdk_api_client.session().inspect_err(|e| {
                    let _ = e;
                    error!("SMS login session retrieval failed");
                })?;
                session.save(session_path).inspect_err(|e| {
                    let _ = e;
                    error!("SMS login session persistence failed");
                })?;

                let user: User = serde_json::from_value(session.user.clone()).inspect_err(|e| {
                    let _ = e;
                    error!("SMS login user parsing failed");
                })?;
                let user_id = user.id;

                let ctx = self.create_short_task_context();
                self.task_handler.save_user_info(ctx, &user).await?;
                self.ensure_sync_account(&user).await?;
                info!("Logged in user {} saved.", user_id);

                Ok(user)
            }
            LoginState::LoggedIn { .. } => {
                warn!("Already logged in, skipping login.");
                let session = self.sdk_api_client.session().inspect_err(|e| {
                    let _ = e;
                    error!("Existing login session retrieval failed");
                })?;
                let user: User = serde_json::from_value(session.user).inspect_err(|e| {
                    let _ = e;
                    error!("Existing login user parsing failed");
                })?;
                self.ensure_sync_account(&user).await?;
                Ok(user)
            }
            LoginState::Init => {
                error!("Wrong login state to login: Init");
                Err(crate::error::Error::InconsistentTask(
                    "FATAL: wrong login state to login".to_string(),
                ))
            }
        }
    }

    /// Checks the current login state and returns the logged-in user if available.
    pub async fn login_state(&self) -> Result<Option<User>> {
        info!("get login state");
        Ok(self
            .sdk_api_client
            .session()
            .ok()
            .map(|s| {
                serde_json::from_value(s.user.clone()).inspect_err(|e| {
                    let _ = e;
                    error!("Login state user parsing failed");
                })
            })
            .transpose()?)
    }

    /// Attempts to restore a session from the saved session file.
    ///
    /// Useful for persisting login across application restarts.
    pub async fn login_with_session(&self) -> Result<Option<User>> {
        let session_path = get_config().read()?.session_path.clone();
        if !session_path.is_file() {
            return Ok(None);
        }
        let session = Session::load(session_path.as_path())?;
        let api_client = self.sdk_api_client.clone();
        api_client.login_with_session(session).await?;
        let session = api_client.session()?;
        session.save(&session_path)?;
        let user: User = serde_json::from_value(session.user.clone())?;
        let ctx = self.create_short_task_context();
        self.task_handler.save_user_info(ctx, &user).await?;
        self.ensure_sync_account(&user).await?;
        info!(user_id = user.id, "saved login session restored");
        Ok(Some(user))
    }

    async fn ensure_sync_account(&self, user: &User) -> Result<()> {
        let session_path = get_config().read()?.session_path.clone();
        let session_ref = session_path
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .ok_or_else(|| {
                crate::error::Error::FormatError("configured session path has no file name".into())
            })?
            .to_string();
        self.save_account(AccountDto {
            id: 0,
            provider: "weibo".into(),
            uid: user.id.to_string(),
            display_name: Some(user.screen_name.clone()),
            session_ref,
            enabled: true,
            created_at: String::new(),
            updated_at: None,
        })
        .await?;
        Ok(())
    }

    // ========================= short tasks =========================

    /// Queries local posts based on the provided search and filter criteria.
    pub async fn query_posts(&self, query: PostQuery) -> Result<PaginatedPostInfo> {
        let ctx = self.create_short_task_context();
        run_short_task!(
            self,
            "query_posts",
            self.task_handler.query_posts(ctx, query)
        )
    }

    /// Deletes a post from local storage.
    pub async fn delete_post(&self, options: DeletePostOptions) -> Result<()> {
        let _admission = self.shutdown_admission.begin_write()?;
        let ctx = self.create_short_task_context();
        run_short_task!(
            self,
            "delete_post",
            self.task_handler.delete_post(ctx, options)
        )
    }

    /// Re-fetches a single post from the Weibo API and updates local storage.
    pub async fn rebackup_post(&self, id: i64) -> Result<()> {
        let _admission = self.shutdown_admission.begin_write()?;
        let ctx = self.create_short_task_context();
        run_short_task!(
            self,
            "rebackup_post",
            self.task_handler.rebackup_post(ctx, id)
        )
    }

    /// Retrieves the raw image data (blob) for a given picture ID.
    pub async fn get_picture_blob(&self, id: &str) -> Result<Option<Bytes>> {
        let _admission = self.shutdown_admission.begin_write()?;
        let ctx = self.create_short_task_context();
        run_short_task!(
            self,
            "get_picture_blob",
            self.task_handler.get_picture_blob(ctx, id)
        )
    }

    /// Retrieves the raw video data (blob) for a given video URL.
    pub async fn get_video_blob(&self, url: &str) -> Result<Option<Bytes>> {
        let _admission = self.shutdown_admission.begin_write()?;
        let ctx = self.create_short_task_context();
        run_short_task!(
            self,
            "get_video_blob",
            self.task_handler.get_video_blob(ctx, url)
        )
    }

    // ========================= long tasks =========================

    /// Starts collecting a user's posts through the Python sidecar.
    pub async fn collect_user_posts(&self, uid: i64, max_pages: u32) -> Result<()> {
        self.start_sidecar_collection(
            TaskType::CollectUserPosts,
            "采集用户微博",
            CollectionRequest {
                command_type: CommandType::CollectUserPosts,
                stream: format!("user:{uid}:posts"),
                payload: serde_json::json!({
                    "uid": uid.to_string(),
                    "max_pages": bounded_max_pages(max_pages),
                }),
            },
        )
    }

    /// Starts collecting first-level comments for a post.
    pub async fn collect_comments(&self, post_id: i64, max_pages: u32) -> Result<()> {
        self.start_sidecar_collection(
            TaskType::CollectComments,
            "采集微博评论",
            CollectionRequest {
                command_type: CommandType::CollectComments,
                stream: format!("post:{post_id}:comments"),
                payload: serde_json::json!({
                    "post_id": post_id.to_string(),
                    "max_pages": bounded_max_pages(max_pages),
                }),
            },
        )
    }

    /// Starts collecting replies below a first-level comment.
    pub async fn collect_comment_replies(
        &self,
        post_id: i64,
        root_comment_id: i64,
        max_pages: u32,
    ) -> Result<()> {
        self.start_sidecar_collection(
            TaskType::CollectCommentReplies,
            "采集评论回复",
            CollectionRequest {
                command_type: CommandType::CollectCommentReplies,
                stream: format!("post:{post_id}:comment:{root_comment_id}:replies"),
                payload: serde_json::json!({
                    "post_id": post_id.to_string(),
                    "root_comment_id": root_comment_id.to_string(),
                    "max_pages": bounded_max_pages(max_pages),
                }),
            },
        )
    }

    fn start_sidecar_collection(
        &self,
        task_type: TaskType,
        description: &str,
        request: CollectionRequest,
    ) -> Result<()> {
        let _admission = self.shutdown_admission.enter()?;
        let mut lifecycle = self.ad_hoc_collection.lock()?;
        if lifecycle
            .as_ref()
            .is_some_and(|task| !task.handle.is_finished())
        {
            return Err(crate::error::Error::InconsistentTask(
                "sidecar collection is already running".into(),
            ));
        }
        lifecycle.take();
        let session_path = get_config().read()?.session_path.clone();
        let task_id = self.next_task_id.fetch_add(1, Ordering::Relaxed);
        self.task_manager
            .start_task(task_id, task_type, description.into(), 0)?;

        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = cancelled.clone();
        let pool = self.db_pool.clone();
        let task_manager = self.task_manager.clone();
        let handle = spawn(async move {
            let blocking_task_manager = task_manager.clone();
            let runtime = tokio::runtime::Handle::current();
            let result = tokio::task::spawn_blocking(move || {
                let options = collection_spawn_options(&session_path)
                    .map_err(|e| crate::error::Error::FormatError(e.to_string()))?;
                let (mut sidecar, _, _) =
                    match Sidecar::spawn_with_handshake_cancellable(&options, || {
                        worker_cancelled.load(Ordering::Acquire)
                    }) {
                        Ok(sidecar) => sidecar,
                        Err(crate::sidecar::SidecarError::HandshakeCancelled) => {
                            return Ok(crate::sidecar::CollectionSummary {
                                status: CollectionStatus::Shutdown,
                                error: Some("application shutdown during sidecar handshake".into()),
                                ..crate::sidecar::CollectionSummary::default()
                            });
                        }
                        Err(error) => {
                            return Err(crate::error::Error::FormatError(error.to_string()));
                        }
                    };
                let result = runtime.block_on(run_collection_interruptible(
                    &mut sidecar,
                    &pool,
                    &request,
                    &worker_cancelled,
                    |progress, total| {
                        if let Err(e) = publish_collection_progress(
                            &blocking_task_manager,
                            task_id,
                            progress,
                            total,
                        ) {
                            warn!("failed to publish collection progress: {e}");
                        }
                    },
                    std::time::Duration::from_secs(60),
                ));
                let _ = sidecar.shutdown(std::time::Duration::from_millis(500));
                result
            })
            .await;

            match result {
                Ok(Ok(summary)) => {
                    finish_collection_task(&task_manager, task_id, summary.status, summary.error)
                }
                Ok(Err(error)) => fail_collection_task(&task_manager, task_id, error.to_string()),
                Err(error) => fail_collection_task(&task_manager, task_id, error.to_string()),
            }
        });
        *lifecycle = Some(AdHocCollectionTask { cancelled, handle });
        Ok(())
    }

    /// Export local posts to another format (e.g., HTML).
    pub async fn export_posts(&self, request: TaskRequest) -> Result<()> {
        if let TaskRequest::Export(options) = request {
            self.start_long_task(
                TaskType::Export,
                "导出帖子",
                0,
                TaskRequest::Export(options),
            )
        } else {
            Err(crate::error::Error::InconsistentTask(
                "Invalid task request for export_posts".into(),
            ))
        }
    }

    /// Clean up redundant or low-resolution images.
    pub async fn cleanup_pictures(&self, request: TaskRequest) -> Result<()> {
        if let TaskRequest::CleanupPictures(options) = request {
            self.start_long_task(
                TaskType::CleanupPictures,
                "清理重复图片",
                0,
                TaskRequest::CleanupPictures(options),
            )
        } else {
            Err(crate::error::Error::InconsistentTask(
                "Invalid task request for cleanup_pictures".into(),
            ))
        }
    }

    /// Clean up invalid or outdated avatars.
    pub async fn cleanup_outdated_avatars(&self) -> Result<()> {
        self.start_long_task(
            TaskType::CleanupAvatars,
            "清理失效头像",
            0,
            TaskRequest::CleanupOutdatedAvatars,
        )
    }

    /// Clean up invalid posts.
    pub async fn cleanup_invalid_posts(&self, request: TaskRequest) -> Result<()> {
        if let TaskRequest::CleanupInvalidPosts(options) = request {
            self.start_long_task(
                TaskType::CleanupInvalidPosts,
                "清理失效帖子",
                0,
                TaskRequest::CleanupInvalidPosts(options),
            )
        } else {
            Err(crate::error::Error::InconsistentTask(
                "Invalid task request for cleanup_invalid_posts".into(),
            ))
        }
    }

    /// Starts a long-running task to backup a user's posts.
    pub async fn backup_user(&self, request: TaskRequest) -> Result<()> {
        let total = request.total() as u64;
        self.start_long_task(TaskType::BackupUser, "备份用户微博", total, request)
    }

    /// Starts a long-running task to backup the current user's favorites.
    pub async fn backup_favorites(&self, request: TaskRequest) -> Result<()> {
        let total = request.total() as u64;
        self.start_long_task(TaskType::BackupFavorites, "备份收藏", total, request)
    }

    /// Starts a long-running task to unfavorite posts that are in the local database.
    pub async fn unfavorite_posts(&self) -> Result<()> {
        self.start_long_task(
            TaskType::UnfavoritePosts,
            "取消收藏",
            0,
            TaskRequest::UnfavoritePosts,
        )
    }

    /// Starts a long-running task to re-backup posts.
    pub async fn rebackup_posts(&self, request: TaskRequest) -> Result<()> {
        self.start_long_task(TaskType::RebackupPosts, "批量重新备份", 0, request)
    }

    /// Starts a long-running task to re-backup posts with missing images.
    pub async fn rebackup_missing_images(&self, request: TaskRequest) -> Result<()> {
        self.start_long_task(
            TaskType::RebackupMissingImages,
            "重新备份缺失图片",
            0,
            request,
        )
    }

    /// Starts a long-running task to clean up invalid pictures.
    pub async fn cleanup_invalid_pictures(&self, request: TaskRequest) -> Result<()> {
        self.start_long_task(TaskType::CleanupInvalidPictures, "清理失效图片", 0, request)
    }

    fn start_long_task(
        &self,
        task_type: TaskType,
        description: &str,
        total: u64,
        request: TaskRequest,
    ) -> Result<()> {
        let _admission = self.shutdown_admission.enter()?;
        let mut lifecycle = self.long_task.lock()?;
        if lifecycle
            .as_ref()
            .is_some_and(|task| !task.handle.is_finished())
        {
            return Err(crate::error::Error::InconsistentTask(
                "a long-running task is already running".into(),
            ));
        }
        lifecycle.take();
        let ctx = self.create_long_task_context();
        let task_id = ctx.task_id.expect("long task context must have an id");
        self.task_manager
            .start_task(task_id, task_type, description.into(), total)?;
        let handle = spawn(handle_task_request(self.task_handler.clone(), ctx, request));
        *lifecycle = Some(LongTask { task_id, handle });
        Ok(())
    }

    // ========================= context creators =========================

    /// Creates a task context for long-running tasks, including a unique task ID.
    fn create_long_task_context(&self) -> Arc<TaskContext> {
        let id = self.next_task_id.fetch_add(1, Ordering::Relaxed);
        Arc::new(TaskContext {
            task_id: Some(id),
            config: get_config().read().unwrap().clone(),
            task_manager: self.task_manager.clone(),
        })
    }

    /// Creates a task context for short-lived operations that do not require progress tracking.
    fn create_short_task_context(&self) -> Arc<TaskContext> {
        Arc::new(TaskContext {
            task_id: None,
            config: get_config().read().unwrap().clone(),
            task_manager: self.task_manager.clone(),
        })
    }
}

fn publish_collection_progress(
    task_manager: &TaskManager,
    task_id: u64,
    committed: u64,
    total: u64,
) -> Result<()> {
    task_manager.update_progress_for(task_id, committed, total)
}

fn bounded_max_pages(max_pages: u32) -> u32 {
    max_pages.clamp(1, 1000)
}

fn finish_collection_task(
    task_manager: &TaskManager,
    task_id: u64,
    status: CollectionStatus,
    error_message: Option<String>,
) {
    let result = match status {
        CollectionStatus::Completed => task_manager.finish_for(task_id),
        CollectionStatus::Stopped => task_manager.cancel_for(task_id),
        CollectionStatus::Paused => task_manager.pause_for(task_id),
        CollectionStatus::Cancelled => task_manager.cancel_for(task_id),
        CollectionStatus::RateLimited => task_manager.interrupt_for(task_id),
        CollectionStatus::Interrupted => task_manager.interrupt_for(task_id),
        CollectionStatus::Shutdown => task_manager.interrupt_for(task_id),
        CollectionStatus::Failed => task_manager.fail_for(
            task_id,
            error_message.unwrap_or_else(|| "sidecar collection failed".to_string()),
        ),
    };
    if let Err(error) = result {
        warn!("failed to update collection task status: {error}");
    }
}

fn fail_collection_task(task_manager: &TaskManager, task_id: u64, message: String) {
    if let Err(error) = task_manager.fail_for(task_id, message) {
        warn!("failed to mark collection task as failed: {error}");
    }
}

#[tracing::instrument(skip(task_handler, ctx), fields(task_id = ctx.task_id))]
async fn handle_task_request(task_handler: Arc<TH>, ctx: Arc<TaskContext>, request: TaskRequest) {
    let task_id = ctx.task_id.unwrap();
    info!("Handling task request for task_id: {}", task_id);

    let res = match request {
        TaskRequest::BackupUser(options) => task_handler.backup_user(ctx.clone(), options).await,
        TaskRequest::UnfavoritePosts => task_handler.unfavorite_posts(ctx.clone()).await,
        TaskRequest::BackupFavorites(options) => {
            task_handler.backup_favorites(ctx.clone(), options).await
        }
        TaskRequest::RebackupPosts(query) => task_handler.rebackup_posts(ctx.clone(), query).await,
        TaskRequest::RebackupMissingImages(query) => {
            task_handler
                .rebackup_missing_images(ctx.clone(), query)
                .await
        }
        TaskRequest::CleanupInvalidPictures => {
            task_handler.cleanup_invalid_pictures(ctx.clone()).await
        }
        TaskRequest::Export(options) => task_handler.export_posts(ctx.clone(), options).await,
        TaskRequest::CleanupPictures(options) => {
            task_handler.cleanup_pictures(ctx.clone(), options).await
        }
        TaskRequest::CleanupOutdatedAvatars => {
            task_handler.cleanup_outdated_avatars(ctx.clone()).await
        }
        TaskRequest::CleanupInvalidPosts(options) => {
            task_handler
                .cleanup_invalid_posts(ctx.clone(), options)
                .await
        }
    };

    if let Err(err) = res {
        error!("Task {} failed: {}", task_id, err);
        if let Err(e) = ctx.task_manager.fail(err.to_string()) {
            error!("Failed to set task {} as failed: {}", task_id, e);
        }
    } else {
        info!("Task {} completed successfully", task_id);
        if let Err(e) = ctx.task_manager.finish() {
            error!("Failed to set task {} as finished: {}", task_id, e);
        }
    }
}

#[cfg(test)]
mod shutdown_admission_tests {
    use super::{
        PersistentShutdownSummary, SchedulerShutdownStatus, ShutdownAdmission,
        publish_collection_progress,
    };
    use crate::core::{TaskManager, TaskType, task_manager::TaskStatus};
    use crate::media_downloader::DownloaderWorkerSummary;
    use crate::media_pipeline::MediaWorkerSummary;
    use crate::sync_executor::WorkerShutdownSummary;
    use std::{
        sync::{Arc, mpsc},
        time::Duration,
    };

    #[test]
    fn shutdown_admission_rejects_new_starts_after_shutdown_begins() {
        let admission = ShutdownAdmission::default();
        admission.begin_shutdown();
        assert!(admission.enter().is_err());
    }

    #[test]
    fn shutdown_waits_for_in_flight_start_critical_section() {
        let admission = Arc::new(ShutdownAdmission::default());
        let start_guard = admission.enter().unwrap();
        let (done_tx, done_rx) = mpsc::channel();
        let shutting_down = admission.clone();
        let shutdown = std::thread::spawn(move || {
            shutting_down.begin_shutdown();
            done_tx.send(()).unwrap();
        });

        assert!(done_rx.recv_timeout(Duration::from_millis(50)).is_err());
        drop(start_guard);
        done_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        shutdown.join().unwrap();
        assert!(admission.enter().is_err());
    }

    #[test]
    fn collection_progress_preserves_unknown_total_until_sidecar_knows_it() {
        let manager = TaskManager::new();
        manager
            .start_task(7, TaskType::CollectComments, "comments".into(), 0)
            .unwrap();

        publish_collection_progress(&manager, 7, 20, 0).unwrap();
        let task = manager.get_current().unwrap().unwrap();
        assert_eq!(task.progress, 20);
        assert_eq!(task.total, 0);
        assert_eq!(task.status, TaskStatus::InProgress);

        publish_collection_progress(&manager, 7, 25, 40).unwrap();
        let task = manager.get_current().unwrap().unwrap();
        assert_eq!(task.progress, 25);
        assert_eq!(task.total, 40);
    }

    #[test]
    fn shutdown_is_degraded_when_legacy_downloader_does_not_stop() {
        let summary = PersistentShutdownSummary {
            scheduler: SchedulerShutdownStatus::NotRunning,
            ad_hoc: SchedulerShutdownStatus::NotRunning,
            workers: WorkerShutdownSummary::default(),
            database_failures: Vec::new(),
            media: MediaWorkerSummary {
                stopped: true,
                join_failed: None,
            },
            legacy_media: DownloaderWorkerSummary {
                stopped: false,
                join_failed: Some("timed out".into()),
            },
            long_task: SchedulerShutdownStatus::NotRunning,
        };

        assert!(summary.degraded());
    }
}
