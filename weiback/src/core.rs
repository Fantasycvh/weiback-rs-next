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
use crate::media_downloader::MediaDownloaderHandle;
use crate::models::User;
use crate::refresh_scheduler::{PersistentScheduler, RefreshScheduleConfig};
use crate::sidecar::{
    CollectionRequest, CollectionStatus, CommandType, DEFAULT_HANDSHAKE_TIMEOUT, Sidecar,
    collection_spawn_options, run_collection_interruptible,
};
use crate::storage::StorageImpl;
use crate::storage::internal::entities::{
    AccountDto, JobControlResult, MonitoredUserDto, SyncJobDto, SyncJobSpec, SyncRunDto,
    delete_account, delete_monitored_user, enqueue_sync_job_spec, get_accounts,
    get_monitored_users, get_sync_job, get_sync_jobs, get_sync_run_history,
    recover_interrupted_sync_jobs, resume_sync_job, retry_sync_job, save_account,
    save_monitored_user,
};
use crate::sync_executor::{
    ControlStopResult, JobExecutor, WorkerRegistry, WorkerShutdownSummary,
    account_session_resolver, validate_account_session,
};
pub use task::{
    BackupFavoritesOptions, BackupUserPostsOptions, CleanupInvalidPostsOptions, DeletePostOptions,
    ExportJobOptions, PaginatedPostInfo, PostQuery, TaskContext, TaskRequest, UserPostFilter,
};
pub use task_handler::TaskHandler;
pub use task_manager::{Task, TaskError, TaskEventListener, TaskManager, TaskType};

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
}

#[derive(Default)]
struct ShutdownAdmission {
    shutting_down: AtomicBool,
    gate: Mutex<()>,
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
    ) -> Result<Self> {
        Ok(Self {
            next_task_id: AtomicU64::new(1),
            task_handler: Arc::new(task_handler),
            task_manager: Arc::new(TaskManager::new()),
            sdk_api_client,
            db_pool,
            persistent_workers: Arc::new(WorkerRegistry::new()),
            shutdown_admission: ShutdownAdmission::default(),
            persistent_scheduler: Mutex::new(PersistentSchedulerLifecycle::default()),
            ad_hoc_collection: Mutex::new(None),
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
        let handle = spawn(scheduler.run_until_cancelled(
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
        PersistentShutdownSummary {
            scheduler,
            ad_hoc,
            workers,
            database_failures,
        }
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

    pub async fn get_accounts(&self) -> Result<Vec<AccountDto>> {
        get_accounts(&self.db_pool).await
    }

    pub async fn save_account(&self, mut account: AccountDto) -> Result<i64> {
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

    pub async fn delete_account(&self, id: i64) -> Result<bool> {
        delete_account(&self.db_pool, id).await
    }

    pub async fn get_monitored_users(&self) -> Result<Vec<MonitoredUserDto>> {
        get_monitored_users(&self.db_pool).await
    }

    pub async fn save_monitored_user(&self, mut user: MonitoredUserDto) -> Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        if user.created_at.is_empty() {
            user.created_at = now.clone();
        }
        user.updated_at = Some(now);
        save_monitored_user(&self.db_pool, &user).await
    }

    pub async fn delete_monitored_user(&self, account_id: i64, uid: i64) -> Result<bool> {
        delete_monitored_user(&self.db_pool, account_id, uid).await
    }

    pub async fn enqueue_sync_job(&self, spec: SyncJobSpec) -> Result<i64> {
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
        let worker_stop = JobExecutor::new(self.db_pool.clone(), self.persistent_workers.clone())
            .pause(job_id, std::time::Duration::from_secs(5))
            .await?;
        Ok(SyncJobControlOutcome {
            job: self.require_sync_job(job_id).await?,
            worker_stop,
        })
    }

    pub async fn cancel_sync_job(&self, job_id: i64) -> Result<SyncJobControlOutcome> {
        let worker_stop = JobExecutor::new(self.db_pool.clone(), self.persistent_workers.clone())
            .cancel(job_id, std::time::Duration::from_secs(5))
            .await?;
        Ok(SyncJobControlOutcome {
            job: self.require_sync_job(job_id).await?,
            worker_stop,
        })
    }

    pub async fn resume_sync_job(&self, job_id: i64) -> Result<SyncJobDto> {
        let now = chrono::Utc::now().to_rfc3339();
        let _: JobControlResult = resume_sync_job(&self.db_pool, job_id, &now).await?;
        self.require_sync_job(job_id).await
    }

    pub async fn retry_sync_job(&self, job_id: i64) -> Result<SyncJobDto> {
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
        info!("send_code called for phone number: {phone_number}");
        self.sdk_api_client
            .get_sms_code(phone_number)
            .await
            .inspect_err(|e| error!("get_sms_code failed: {e}"))?;
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
        info!("login called with a sms_code");
        match self.sdk_api_client.login_state() {
            LoginState::WaitingForCode { .. } => {
                info!("Attempting to login with SMS code.");
                self.sdk_api_client
                    .login(&sms_code)
                    .await
                    .inspect_err(|e| error!("SDK login failed: {e}"))?;
                info!("Login successful.");
                let session_path = get_config()
                    .read()
                    .expect("config lock failed")
                    .session_path
                    .clone();
                let session = self.sdk_api_client.session().inspect_err(|e| {
                    error!("get session after login failed: {e}");
                })?;
                session.save(session_path).inspect_err(|e| {
                    error!("save session to disk failed: {e}");
                })?;

                let user: User = serde_json::from_value(session.user.clone()).inspect_err(|e| {
                    error!("parse user from session failed: {e}");
                })?;
                let user_id = user.id;

                let th = self.task_handler.clone();
                let ctx = self.create_short_task_context();
                let user_clone = user.clone();
                spawn(async move {
                    if let Err(e) = th.save_user_info(ctx, &user_clone).await {
                        error!("Save user info failed: {e}");
                    }
                });
                info!("Logged in user {} saved.", user_id);

                Ok(user)
            }
            LoginState::LoggedIn { .. } => {
                warn!("Already logged in, skipping login.");
                let session = self.sdk_api_client.session().inspect_err(|e| {
                    error!("get session in AlreadyLoggedIn branch failed: {e}");
                })?;
                let user: User = serde_json::from_value(session.user).inspect_err(|e| {
                    error!("parse user from session in AlreadyLoggedIn branch failed: {e}");
                })?;
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
                    error!("parse user from session failed: {e}");
                })
            })
            .transpose()?)
    }

    /// Attempts to restore a session from the saved session file.
    ///
    /// Useful for persisting login across application restarts.
    pub async fn login_with_session(&self) -> Result<()> {
        let session_path = get_config().read()?.session_path.clone();
        if let Ok(session) = Session::load(session_path.as_path()) {
            let api_client = self.sdk_api_client.clone();
            if let Err(e) = api_client.login_with_session(session).await {
                error!("login with session failed: {e}");
            }
            info!("login with session successfully");
            match api_client.session() {
                Ok(session) => {
                    if let Err(e) = session.save(session_path) {
                        error!("save new session failed: {e}");
                    }
                }
                Err(e) => {
                    error!("get new session failed: {e}");
                }
            }
        }
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
        let ctx = self.create_short_task_context();
        run_short_task!(
            self,
            "delete_post",
            self.task_handler.delete_post(ctx, options)
        )
    }

    /// Re-fetches a single post from the Weibo API and updates local storage.
    pub async fn rebackup_post(&self, id: i64) -> Result<()> {
        let ctx = self.create_short_task_context();
        run_short_task!(
            self,
            "rebackup_post",
            self.task_handler.rebackup_post(ctx, id)
        )
    }

    /// Retrieves the raw image data (blob) for a given picture ID.
    pub async fn get_picture_blob(&self, id: &str) -> Result<Option<Bytes>> {
        let ctx = self.create_short_task_context();
        run_short_task!(
            self,
            "get_picture_blob",
            self.task_handler.get_picture_blob(ctx, id)
        )
    }

    /// Retrieves the raw video data (blob) for a given video URL.
    pub async fn get_video_blob(&self, url: &str) -> Result<Option<Bytes>> {
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
                        if let Err(e) = blocking_task_manager.update_progress_for(
                            task_id,
                            progress,
                            total.max(progress),
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
        let ctx = self.create_long_task_context();
        let id = ctx.task_id.unwrap();
        if let TaskRequest::Export(options) = request {
            self.task_manager
                .start_task(id, TaskType::Export, "导出帖子".into(), 0)?;
            spawn(handle_task_request(
                self.task_handler.clone(),
                ctx,
                TaskRequest::Export(options),
            ));
            Ok(())
        } else {
            Err(crate::error::Error::InconsistentTask(
                "Invalid task request for export_posts".into(),
            ))
        }
    }

    /// Clean up redundant or low-resolution images.
    pub async fn cleanup_pictures(&self, request: TaskRequest) -> Result<()> {
        let ctx = self.create_long_task_context();
        let id = ctx.task_id.unwrap();
        if let TaskRequest::CleanupPictures(options) = request {
            self.task_manager.start_task(
                id,
                TaskType::CleanupPictures,
                "清理重复图片".into(),
                0,
            )?;
            spawn(handle_task_request(
                self.task_handler.clone(),
                ctx,
                TaskRequest::CleanupPictures(options),
            ));
            Ok(())
        } else {
            Err(crate::error::Error::InconsistentTask(
                "Invalid task request for cleanup_pictures".into(),
            ))
        }
    }

    /// Clean up invalid or outdated avatars.
    pub async fn cleanup_outdated_avatars(&self) -> Result<()> {
        let ctx = self.create_long_task_context();
        let id = ctx.task_id.unwrap();
        self.task_manager
            .start_task(id, TaskType::CleanupAvatars, "清理失效头像".into(), 0)?;
        spawn(handle_task_request(
            self.task_handler.clone(),
            ctx,
            TaskRequest::CleanupOutdatedAvatars,
        ));
        Ok(())
    }

    /// Clean up invalid posts.
    pub async fn cleanup_invalid_posts(&self, request: TaskRequest) -> Result<()> {
        let ctx = self.create_long_task_context();
        let id = ctx.task_id.unwrap();
        if let TaskRequest::CleanupInvalidPosts(options) = request {
            self.task_manager.start_task(
                id,
                TaskType::CleanupInvalidPosts,
                "清理失效帖子".into(),
                0,
            )?;
            spawn(handle_task_request(
                self.task_handler.clone(),
                ctx,
                TaskRequest::CleanupInvalidPosts(options),
            ));
            Ok(())
        } else {
            Err(crate::error::Error::InconsistentTask(
                "Invalid task request for cleanup_invalid_posts".into(),
            ))
        }
    }

    /// Starts a long-running task to backup a user's posts.
    pub async fn backup_user(&self, request: TaskRequest) -> Result<()> {
        let ctx = self.create_long_task_context();
        let id = ctx.task_id.unwrap();
        let total = request.total() as u64;
        self.task_manager
            .start_task(id, TaskType::BackupUser, "备份用户微博".into(), total)?;
        spawn(handle_task_request(self.task_handler.clone(), ctx, request));
        Ok(())
    }

    /// Starts a long-running task to backup the current user's favorites.
    pub async fn backup_favorites(&self, request: TaskRequest) -> Result<()> {
        let ctx = self.create_long_task_context();
        let id = ctx.task_id.unwrap();
        let total = request.total() as u64;
        self.task_manager
            .start_task(id, TaskType::BackupFavorites, "备份收藏".into(), total)?;
        spawn(handle_task_request(self.task_handler.clone(), ctx, request));
        Ok(())
    }

    /// Starts a long-running task to unfavorite posts that are in the local database.
    pub async fn unfavorite_posts(&self) -> Result<()> {
        let ctx = self.create_long_task_context();
        let id = ctx.task_id.unwrap();
        let total = 0; // Will be updated later in task_handler
        self.task_manager
            .start_task(id, TaskType::UnfavoritePosts, "取消收藏".into(), total)?;
        spawn(handle_task_request(
            self.task_handler.clone(),
            ctx,
            TaskRequest::UnfavoritePosts,
        ));
        Ok(())
    }

    /// Starts a long-running task to re-backup posts.
    pub async fn rebackup_posts(&self, request: TaskRequest) -> Result<()> {
        let ctx = self.create_long_task_context();
        let id = ctx.task_id.unwrap();
        let total = 0; // Will be updated in task_handler
        self.task_manager
            .start_task(id, TaskType::RebackupPosts, "批量重新备份".into(), total)?;
        spawn(handle_task_request(self.task_handler.clone(), ctx, request));
        Ok(())
    }

    /// Starts a long-running task to re-backup posts with missing images.
    pub async fn rebackup_missing_images(&self, request: TaskRequest) -> Result<()> {
        let ctx = self.create_long_task_context();
        let id = ctx.task_id.unwrap();
        let total = 0; // Will be updated in task_handler
        self.task_manager.start_task(
            id,
            TaskType::RebackupMissingImages,
            "重新备份缺失图片".into(),
            total,
        )?;
        spawn(handle_task_request(self.task_handler.clone(), ctx, request));
        Ok(())
    }

    /// Starts a long-running task to clean up invalid pictures.
    pub async fn cleanup_invalid_pictures(&self, request: TaskRequest) -> Result<()> {
        let ctx = self.create_long_task_context();
        let id = ctx.task_id.unwrap();
        let total = 0; // Will be updated in task_handler
        self.task_manager.start_task(
            id,
            TaskType::CleanupInvalidPictures,
            "清理失效图片".into(),
            total,
        )?;
        spawn(handle_task_request(self.task_handler.clone(), ctx, request));
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
    use super::ShutdownAdmission;
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
}
