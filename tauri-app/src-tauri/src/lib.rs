mod error;

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use serde::{Deserialize, Serialize};
use serde_with::{DisplayFromStr, serde_as};
use tauri::{self, App, AppHandle, Emitter, Manager, State, ipc::Response};
use tracing::{debug, error, info, warn};
use weiback::builder::CoreBuilder;
use weiback::config::{Config, get_config};
use weiback::core::{
    BackupFavoritesOptions, BackupUserPostsOptions, CleanupInvalidPostsOptions, Core,
    DeletePostOptions, ExportJobOptions, PostQuery, SyncJobControlOutcome, TaskEventListener,
    TaskRequest,
    task::{BackupType, CleanupPicturesOptions, PaginatedPostInfo},
    task_manager::{Task, TaskError},
};
use weiback::media_downloader::{DownloaderStatus, MediaDownloaderStatusListener};
use weiback::models::User;
use weiback::storage::internal::entities::{
    AccountDto, MonitoredUserDto, OwnerMediaWire, RefreshTier, SyncJobDto, SyncJobSpec, SyncRunDto,
    owner_media_wire,
};
use weiback::sync_executor::ControlStopResult;

use error::{Error, INVALID_SYNC_INPUT, Result, SYNC_OPERATION_FAILED, stable_sync_error};

const MAX_SYNC_PAGES: u32 = 1_000;
const MIN_SYNC_PRIORITY: i32 = 0;
const MAX_SYNC_PRIORITY: i32 = 1_000;
const MAX_REFRESH_INTERVAL_SECS: i64 = 7 * 24 * 60 * 60;
const MAX_REFRESH_JITTER_SECS: i64 = MAX_REFRESH_INTERVAL_SECS;
const MAX_QUERY_SOURCE_LEN: usize = 128;
const INVALID_LEGACY_INPUT: &str = "Invalid legacy import source";
const LEGACY_IMPORT_FAILED: &str = "Legacy import failed; see the application log";
const USER_BACKUP_FAILED: &str = "User data backup operation failed; see the application log";
const USER_RESTORE_ACTIVE_TASK: &str = "Restore blocked: an ordinary task is still active";
const USER_RESTORE_RESTART_REQUIRED: &str =
    "Restore failed after the live data connection closed; restart the application";

#[derive(Debug, Serialize, Clone)]
#[serde(tag = "status")]
pub enum BackendStatus {
    Uninitialized,
    Running {
        #[serde(skip_serializing_if = "Option::is_none")]
        warning: Option<String>,
    },
    Error {
        message: String,
    },
}

pub struct BackendState {
    pub status: Mutex<BackendStatus>,
    pub exit_started: AtomicBool,
}

/// A reporter that forwards task events to the Tauri frontend via `emit`.
struct TauriTaskEventListener {
    app_handle: tauri::AppHandle,
}

impl TaskEventListener for TauriTaskEventListener {
    fn on_task_updated(&self, task: &Task) {
        debug!("emit task-updated to frontend: {task:?}");
        let _ = self.app_handle.emit("task-updated", task);
    }

    fn on_task_error(&self, error: &TaskError) {
        debug!("emit task-error to frontend: {error:?}");
        let _ = self.app_handle.emit("task-error", error);
    }
}

impl MediaDownloaderStatusListener for TauriTaskEventListener {
    fn on_status_updated(&self, status: &DownloaderStatus) {
        debug!("emit downloader-status to frontend: {status:?}");
        let _ = self.app_handle.emit("downloader-status", status);
    }
}

/// A wrapper for Weibo IDs to handle conversion from string/number in Tauri commands.
#[serde_as]
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
pub struct WeiboId(#[serde_as(as = "DisplayFromStr")] i64);

impl From<WeiboId> for i64 {
    fn from(id: WeiboId) -> Self {
        id.0
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct MediaBlobWire {
    pub content_type: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AccountWireDto {
    pub id: String,
    pub provider: String,
    pub uid: String,
    pub display_name: Option<String>,
    pub enabled: bool,
    pub has_session: bool,
    pub session_ready: bool,
    pub created_at: String,
    pub updated_at: Option<String>,
}

impl AccountWireDto {
    fn from_account(account: AccountDto, session_ready: bool) -> Self {
        Self {
            id: account.id.to_string(),
            provider: account.provider,
            uid: account.uid,
            display_name: account.display_name,
            enabled: account.enabled,
            has_session: !account.session_ref.is_empty(),
            session_ready,
            created_at: account.created_at,
            updated_at: account.updated_at,
        }
    }
}

async fn require_ready_sync_account(core: &Core, id: i64) -> Result<()> {
    let account = core
        .get_accounts()
        .await
        .map_err(|error| sync_core_error("require_ready_sync_account/get_accounts", error))?
        .into_iter()
        .find(|account| account.id == id)
        .ok_or_else(|| stable_sync_error("Selected sync account was not found"))?;
    if !account.enabled {
        return Err(stable_sync_error("Selected sync account is disabled"));
    }
    if !core
        .account_session_ready(&account)
        .map_err(|error| sync_core_error("require_ready_sync_account/session", error))?
    {
        return Err(stable_sync_error(
            "Selected sync account session is unavailable; log in again",
        ));
    }
    Ok(())
}

async fn require_logged_in(core: &Core) -> Result<()> {
    if core
        .login_state()
        .await
        .map_err(|error| sync_core_error("require_logged_in", error))?
        .is_none()
    {
        return Err(stable_sync_error("Log in before starting an online backup"));
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
pub struct UserSearchWireDto {
    pub id: String,
    pub screen_name: String,
}

impl From<User> for UserSearchWireDto {
    fn from(user: User) -> Self {
        Self {
            id: user.id.to_string(),
            screen_name: user.screen_name,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct SaveSyncAccountInput {
    pub id: Option<WeiboId>,
    pub provider: String,
    pub uid: String,
    pub display_name: Option<String>,
    pub session_ref: Option<String>,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct MonitoredUserWireDto {
    pub account_id: String,
    pub uid: String,
    pub screen_name: Option<String>,
    pub refresh_strategy: String,
    pub enabled: bool,
    pub last_refreshed_at: Option<String>,
    pub created_at: String,
    pub updated_at: Option<String>,
    pub tier: RefreshTier,
    pub interval_secs: String,
    pub jitter_secs: String,
    pub next_refresh_epoch: String,
    pub last_refresh_epoch: Option<String>,
}

impl From<MonitoredUserDto> for MonitoredUserWireDto {
    fn from(user: MonitoredUserDto) -> Self {
        Self {
            account_id: user.account_id.to_string(),
            uid: user.uid.to_string(),
            screen_name: user.screen_name,
            refresh_strategy: user.refresh_strategy,
            enabled: user.enabled,
            last_refreshed_at: user.last_refreshed_at,
            created_at: user.created_at,
            updated_at: user.updated_at,
            tier: user.tier,
            interval_secs: user.interval_secs.to_string(),
            jitter_secs: user.jitter_secs.to_string(),
            next_refresh_epoch: user.next_refresh_epoch.to_string(),
            last_refresh_epoch: user.last_refresh_epoch.map(|epoch| epoch.to_string()),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct SaveMonitoredUserInput {
    pub account_id: WeiboId,
    pub uid: WeiboId,
    pub screen_name: Option<String>,
    pub refresh_strategy: String,
    pub enabled: bool,
    pub tier: RefreshTier,
    pub interval_secs: i64,
    pub jitter_secs: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SyncJobWireDto {
    pub id: String,
    pub resource_key: String,
    pub name: String,
    pub kind: String,
    pub status: String,
    pub priority: String,
    pub schedule_config: Option<String>,
    pub enabled: bool,
    pub recovery_count: String,
    pub max_recovery_attempts: String,
    pub available_at: Option<String>,
    pub available_at_epoch: String,
    pub claimed_at: Option<String>,
    pub current_run_id: Option<String>,
    pub created_at: String,
    pub updated_at: Option<String>,
    pub account_id: String,
    pub endpoint_key: String,
}

impl From<SyncJobDto> for SyncJobWireDto {
    fn from(job: SyncJobDto) -> Self {
        Self {
            id: job.id.to_string(),
            resource_key: job.resource_key,
            name: job.name,
            kind: job.kind,
            status: job.status,
            priority: job.priority.to_string(),
            schedule_config: job.schedule_config,
            enabled: job.enabled,
            recovery_count: job.recovery_count.to_string(),
            max_recovery_attempts: job.max_recovery_attempts.to_string(),
            available_at: job.available_at,
            available_at_epoch: job.available_at_epoch.to_string(),
            claimed_at: job.claimed_at,
            current_run_id: job.current_run_id.map(|id| id.to_string()),
            created_at: job.created_at,
            updated_at: job.updated_at,
            account_id: job.account_id.to_string(),
            endpoint_key: job.endpoint_key,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SyncJobControlWireOutcome {
    pub job: SyncJobWireDto,
    pub worker_stop: ControlStopWireResult,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", content = "detail", rename_all = "snake_case")]
pub enum ControlStopWireResult {
    Stopped { pid: u32 },
    WorkerNotFound,
    WorkerStarting,
    StopTimedOut { pid: u32 },
    StopFailed(String),
}

#[derive(Debug, Clone, Serialize)]
pub struct SyncRunWireDto {
    pub id: String,
    pub job_id: String,
    pub status: String,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub stats_json: Option<String>,
    pub attempt: String,
    pub updated_at: Option<String>,
}

impl From<SyncRunDto> for SyncRunWireDto {
    fn from(run: SyncRunDto) -> Self {
        Self {
            id: run.id.to_string(),
            job_id: run.job_id.to_string(),
            status: run.status,
            started_at: run.started_at,
            finished_at: run.finished_at,
            stats_json: run.stats_json,
            attempt: run.attempt.to_string(),
            updated_at: run.updated_at,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[allow(clippy::enum_variant_names)]
enum SyncJobCommandSpec {
    CollectUserPosts {
        account_id: WeiboId,
        uid: WeiboId,
        max_pages: Option<u32>,
        priority: i32,
    },
    CollectComments {
        account_id: WeiboId,
        post_id: WeiboId,
        max_pages: Option<u32>,
        priority: i32,
    },
    CollectCommentReplies {
        account_id: WeiboId,
        post_id: WeiboId,
        root_comment_id: WeiboId,
        max_pages: Option<u32>,
        priority: i32,
    },
}

impl From<SyncJobCommandSpec> for SyncJobSpec {
    fn from(spec: SyncJobCommandSpec) -> Self {
        match spec {
            SyncJobCommandSpec::CollectUserPosts {
                account_id,
                uid,
                max_pages,
                priority,
            } => Self::CollectUserPosts {
                account_id: account_id.into(),
                uid: uid.into(),
                max_pages: max_pages.map(u64::from),
                priority: i64::from(priority),
            },
            SyncJobCommandSpec::CollectComments {
                account_id,
                post_id,
                max_pages,
                priority,
            } => Self::CollectComments {
                account_id: account_id.into(),
                post_id: post_id.into(),
                max_pages: max_pages.map(u64::from),
                priority: i64::from(priority),
            },
            SyncJobCommandSpec::CollectCommentReplies {
                account_id,
                post_id,
                root_comment_id,
                max_pages,
                priority,
            } => Self::CollectCommentReplies {
                account_id: account_id.into(),
                post_id: post_id.into(),
                root_comment_id: root_comment_id.into(),
                max_pages: max_pages.map(u64::from),
                priority: i64::from(priority),
            },
        }
    }
}

fn sync_core_error(operation: &'static str, error: impl std::fmt::Display) -> Error {
    error!(operation, error = %error, "sync command failed");
    stable_sync_error(SYNC_OPERATION_FAILED)
}

fn sms_auth_error(stage: &'static str) -> Error {
    error!(stage, "SMS authentication failed");
    stable_sync_error(SYNC_OPERATION_FAILED)
}

fn valid_phone_number(value: &str) -> bool {
    (6..=32).contains(&value.len())
        && value.chars().enumerate().all(|(index, character)| {
            character.is_ascii_digit() || (index == 0 && character == '+')
        })
}

fn valid_sms_code(value: &str) -> bool {
    (4..=12).contains(&value.len()) && value.chars().all(|character| character.is_ascii_digit())
}

fn validate_session_ref(session_ref: &str) -> Result<()> {
    if session_ref.is_empty()
        || session_ref.contains('\0')
        || session_ref.starts_with('/')
        || session_ref.starts_with('\\')
        || session_ref.as_bytes().get(1) == Some(&b':')
        || session_ref.split(['/', '\\']).any(|part| part == "..")
    {
        return Err(stable_sync_error(INVALID_SYNC_INPUT));
    }
    Ok(())
}

fn validate_account_input(input: &SaveSyncAccountInput) -> Result<()> {
    if input.provider.trim().is_empty()
        || input.provider.len() > 64
        || input.uid.trim().is_empty()
        || input.uid.len() > 128
    {
        return Err(stable_sync_error(INVALID_SYNC_INPUT));
    }
    if let Some(session_ref) = input.session_ref.as_deref() {
        validate_session_ref(session_ref)?;
    }
    Ok(())
}

fn validate_monitor_input(input: &SaveMonitoredUserInput) -> Result<()> {
    if input.account_id.0 <= 0 || input.uid.0 <= 0 {
        return Err(stable_sync_error(INVALID_SYNC_INPUT));
    }
    if !matches!(input.refresh_strategy.as_str(), "hot" | "warm" | "cold") {
        return Err(stable_sync_error(INVALID_SYNC_INPUT));
    }
    if !(1..=MAX_REFRESH_INTERVAL_SECS).contains(&input.interval_secs)
        || !(0..=MAX_REFRESH_JITTER_SECS).contains(&input.jitter_secs)
        || input.jitter_secs > input.interval_secs
    {
        return Err(stable_sync_error(INVALID_SYNC_INPUT));
    }
    Ok(())
}

fn validate_job_spec(spec: &SyncJobCommandSpec) -> Result<()> {
    let (account_id, resource_id, max_pages, priority) = match spec {
        SyncJobCommandSpec::CollectUserPosts {
            account_id,
            uid,
            max_pages,
            priority,
            ..
        } => (account_id.0, uid.0, max_pages, priority),
        SyncJobCommandSpec::CollectComments {
            account_id,
            post_id,
            max_pages,
            priority,
            ..
        } => (account_id.0, post_id.0, max_pages, priority),
        SyncJobCommandSpec::CollectCommentReplies {
            account_id,
            post_id,
            root_comment_id,
            max_pages,
            priority,
            ..
        } => {
            if root_comment_id.0 <= 0 {
                return Err(stable_sync_error(INVALID_SYNC_INPUT));
            }
            (account_id.0, post_id.0, max_pages, priority)
        }
    };
    if account_id <= 0
        || resource_id <= 0
        || max_pages.is_some_and(|pages| !(1..=MAX_SYNC_PAGES).contains(&pages))
        || !(MIN_SYNC_PRIORITY..=MAX_SYNC_PRIORITY).contains(priority)
    {
        return Err(stable_sync_error(INVALID_SYNC_INPUT));
    }
    Ok(())
}

fn safe_worker_stop(result: ControlStopResult) -> ControlStopWireResult {
    match result {
        ControlStopResult::Stopped { pid } => ControlStopWireResult::Stopped { pid },
        ControlStopResult::WorkerNotFound => ControlStopWireResult::WorkerNotFound,
        ControlStopResult::WorkerStarting => ControlStopWireResult::WorkerStarting,
        ControlStopResult::StopTimedOut { pid } => ControlStopWireResult::StopTimedOut { pid },
        ControlStopResult::StopFailed(error) => {
            error!(error = %error, "sync worker stop failed");
            ControlStopWireResult::StopFailed(SYNC_OPERATION_FAILED.to_string())
        }
    }
}

fn sync_control_wire(outcome: SyncJobControlOutcome) -> SyncJobControlWireOutcome {
    SyncJobControlWireOutcome {
        job: outcome.job.into(),
        worker_stop: safe_worker_stop(outcome.worker_stop),
    }
}

fn validated_legacy_source(raw: &str) -> Result<PathBuf> {
    let path = Path::new(raw);
    if !path.is_absolute() || path.file_name().and_then(|name| name.to_str()) != Some("weiback.db")
    {
        return Err(stable_sync_error(INVALID_LEGACY_INPUT));
    }
    let source = path
        .canonicalize()
        .map_err(|_| stable_sync_error(INVALID_LEGACY_INPUT))?;
    let root = source
        .parent()
        .ok_or_else(|| stable_sync_error(INVALID_LEGACY_INPUT))?
        .canonicalize()
        .map_err(|_| stable_sync_error(INVALID_LEGACY_INPUT))?;
    if !source.is_file() || !source.starts_with(&root) {
        return Err(stable_sync_error(INVALID_LEGACY_INPUT));
    }
    Ok(source)
}

#[tauri::command]
async fn get_backend_status(state: State<'_, BackendState>) -> Result<BackendStatus> {
    Ok(state.status.lock().unwrap().clone())
}

async fn perform_init_backend(app_handle: &AppHandle, state: &BackendState) -> BackendStatus {
    {
        let status_guard = state.status.lock().unwrap();
        if let BackendStatus::Running { .. } = *status_guard {
            return status_guard.clone();
        }
    }

    info!("Initializing backend core...");
    // Attempt to initialize config from files.
    let mut warning = None;
    if let Err(e) = weiback::config::init() {
        warn!("Config initialization failed, using default: {e}");
        warning = Some("Configuration could not be loaded; defaults are in use".to_string());
        // Fallback to in-memory default configuration.
        weiback::config::init_default();
    }

    match CoreBuilder::new().build() {
        Ok(core) => {
            core.set_legacy_media_downloader_status_listener(Box::new(TauriTaskEventListener {
                app_handle: app_handle.clone(),
            }));

            if let Err(e) = core.set_task_event_listener(Box::new(TauriTaskEventListener {
                app_handle: app_handle.clone(),
            })) {
                error!("Failed to set task event listener: {e}");
            }

            if let Err(error) = core.start_persistent_scheduler() {
                warn!(error = %error, "Persistent scheduler is unavailable");
                warning = Some(match warning {
                    Some(existing) => format!("{existing}; persistent scheduler unavailable"),
                    None => {
                        "Persistent scheduler is unavailable; see the application log".to_string()
                    }
                });
            }
            if let Err(error) = core.login_with_session().await {
                warn!(error = %error, "Saved login session could not be restored");
                warning = Some(match warning {
                    Some(existing) => {
                        format!("{existing}; saved login session could not be restored")
                    }
                    None => "Saved login session could not be restored; log in again".to_string(),
                });
            }

            app_handle.manage(core);
            let mut status_guard = state.status.lock().unwrap();
            *status_guard = BackendStatus::Running { warning };
            info!("Backend initialized successfully");
            status_guard.clone()
        }
        Err(e) => {
            error!("Backend initialization failed: {e}");
            let mut status_guard = state.status.lock().unwrap();
            *status_guard = BackendStatus::Error {
                message: "Backend initialization failed; see the application log".to_string(),
            };
            status_guard.clone()
        }
    }
}

#[tauri::command]
async fn init_backend(
    app_handle: AppHandle,
    state: State<'_, BackendState>,
) -> Result<BackendStatus> {
    Ok(perform_init_backend(&app_handle, &state).await)
}

#[tauri::command(async)]
async fn get_current_task_status(core: State<'_, Arc<Core>>) -> Result<Option<Task>> {
    core.get_current_task()
        .await
        .map_err(|e| Error(e.to_string()))
}

#[tauri::command(async)]
async fn get_and_clear_task_errors(core: State<'_, Arc<Core>>) -> Result<Vec<TaskError>> {
    core.get_and_clear_task_errors()
        .map_err(|e| Error(e.to_string()))
}

#[tauri::command(async)]
async fn get_picture_blob(core: State<'_, Arc<Core>>, id: String) -> Result<Response> {
    match core.get_picture_blob(&id).await {
        Ok(Some(blob)) => {
            debug!("get_picture_blob called, id: {id}");
            Ok(Response::new(blob.to_vec()))
        }
        Ok(None) => {
            warn!("get_picture_blob called: {id} not found");
            Err(Error("Picture not found".to_string()))
        }
        Err(e) => {
            error!("get_picture_blob called: {e:?}");
            Err(Error(e.to_string()))
        }
    }
}

#[tauri::command(async)]
async fn get_video_blob(core: State<'_, Arc<Core>>, url: String) -> Result<Response> {
    match core.get_video_blob(&url).await {
        Ok(Some(blob)) => {
            debug!("get_video_blob called, url: {url}");
            Ok(Response::new(blob.to_vec()))
        }
        Ok(None) => {
            warn!("get_video_blob called: {url} not found");
            Err(Error("Video not found".to_string()))
        }
        Err(e) => {
            error!("get_video_blob called: {e:?}");
            Err(Error(e.to_string()))
        }
    }
}

#[tauri::command]
fn get_config_command() -> Result<Config> {
    get_config()
        .read()
        .map(|guard| guard.clone())
        .map_err(|err| Error(err.to_string()))
}

#[tauri::command]
fn set_config_command(config: Config) -> Result<()> {
    weiback::config::save_config(&config).map_err(|e| Error(e.to_string()))
}

#[tauri::command]
async fn backup_user(
    core: State<'_, Arc<Core>>,
    uid: WeiboId,
    num_pages: u32,
    backup_type: BackupType,
) -> Result<()> {
    require_logged_in(&core).await?;
    info!(
        "backup_user called with uid: {:?}, pages num: {num_pages}, backup_type: {backup_type:?}",
        uid
    );
    Ok(core
        .backup_user(TaskRequest::BackupUser(BackupUserPostsOptions {
            uid: uid.into(),
            num_pages,
            backup_type,
        }))
        .await?)
}

#[tauri::command]
async fn backup_favorites(core: State<'_, Arc<Core>>, num_pages: u32) -> Result<()> {
    require_logged_in(&core).await?;
    info!("backup_favorites called with pages num: {num_pages}");
    Ok(core
        .backup_favorites(TaskRequest::BackupFavorites(BackupFavoritesOptions {
            num_pages,
        }))
        .await?)
}

#[tauri::command]
async fn collect_user_posts(
    core: State<'_, Arc<Core>>,
    uid: WeiboId,
    max_pages: u32,
) -> Result<()> {
    Ok(core.collect_user_posts(uid.into(), max_pages).await?)
}

#[tauri::command]
async fn collect_comments(
    core: State<'_, Arc<Core>>,
    post_id: WeiboId,
    max_pages: u32,
) -> Result<()> {
    Ok(core.collect_comments(post_id.into(), max_pages).await?)
}

#[tauri::command]
async fn collect_comment_replies(
    core: State<'_, Arc<Core>>,
    post_id: WeiboId,
    root_comment_id: WeiboId,
    max_pages: u32,
) -> Result<()> {
    Ok(core
        .collect_comment_replies(post_id.into(), root_comment_id.into(), max_pages)
        .await?)
}

#[tauri::command]
async fn get_sync_accounts(core: State<'_, Arc<Core>>) -> Result<Vec<AccountWireDto>> {
    let accounts = core
        .get_accounts()
        .await
        .map_err(|error| sync_core_error("get_sync_accounts", error))?;
    Ok(accounts
        .into_iter()
        .map(|account| {
            let session_ready = core.account_session_ready(&account).unwrap_or(false);
            AccountWireDto::from_account(account, session_ready)
        })
        .collect())
}

#[tauri::command]
async fn save_sync_account(
    core: State<'_, Arc<Core>>,
    input: SaveSyncAccountInput,
) -> Result<String> {
    validate_account_input(&input)?;
    let existing = core
        .get_accounts()
        .await
        .map_err(|error| sync_core_error("save_sync_account/get_accounts", error))?;
    let account = match input.id {
        Some(id) => {
            let Some(current) = existing.into_iter().find(|account| account.id == id.0) else {
                return Err(Error("Account not found".into()));
            };
            if current.provider != input.provider || current.uid != input.uid {
                return Err(Error("Account provider and uid cannot be changed".into()));
            }
            AccountDto {
                id: current.id,
                provider: current.provider,
                uid: current.uid,
                display_name: input.display_name,
                session_ref: input.session_ref.clone().unwrap_or(current.session_ref),
                enabled: input.enabled,
                created_at: current.created_at,
                updated_at: current.updated_at,
            }
        }
        None => {
            if existing
                .iter()
                .any(|account| account.provider == input.provider && account.uid == input.uid)
            {
                return Err(stable_sync_error(
                    "An account with this provider and uid already exists",
                ));
            }
            let session_ref = input.session_ref.clone().ok_or_else(|| {
                stable_sync_error("A session reference is required when creating an account")
            })?;
            validate_session_ref(&session_ref)?;
            AccountDto {
                id: 0,
                provider: input.provider,
                uid: input.uid,
                display_name: input.display_name,
                session_ref,
                enabled: input.enabled,
                created_at: String::new(),
                updated_at: None,
            }
        }
    };
    core.save_account(account)
        .await
        .map(|id| id.to_string())
        .map_err(|error| sync_core_error("save_sync_account", error))
}

#[tauri::command]
async fn delete_sync_account(core: State<'_, Arc<Core>>, id: WeiboId) -> Result<bool> {
    core.delete_account(id.into())
        .await
        .map_err(|error| sync_core_error("delete_sync_account", error))
}

#[tauri::command]
async fn get_monitored_users(core: State<'_, Arc<Core>>) -> Result<Vec<MonitoredUserWireDto>> {
    Ok(core
        .get_monitored_users()
        .await
        .map_err(|error| sync_core_error("get_monitored_users", error))?
        .into_iter()
        .map(Into::into)
        .collect())
}

#[tauri::command]
async fn save_monitored_user(
    core: State<'_, Arc<Core>>,
    input: SaveMonitoredUserInput,
) -> Result<()> {
    validate_monitor_input(&input)?;
    if input.enabled {
        require_ready_sync_account(&core, input.account_id.0).await?;
    }
    let existing = core
        .get_monitored_users()
        .await
        .map_err(|error| sync_core_error("save_monitored_user/get_monitored_users", error))?;
    let now_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default();
    let user = existing
        .into_iter()
        .find(|user| user.account_id == input.account_id.0 && user.uid == input.uid.0)
        .map(|current| MonitoredUserDto {
            account_id: current.account_id,
            uid: current.uid,
            screen_name: input.screen_name.clone(),
            refresh_strategy: input.refresh_strategy.clone(),
            enabled: input.enabled,
            last_refreshed_at: current.last_refreshed_at,
            created_at: current.created_at,
            updated_at: current.updated_at,
            tier: input.tier,
            interval_secs: input.interval_secs,
            jitter_secs: input.jitter_secs,
            next_refresh_epoch: current.next_refresh_epoch,
            last_refresh_epoch: current.last_refresh_epoch,
        })
        .unwrap_or(MonitoredUserDto {
            account_id: input.account_id.into(),
            uid: input.uid.into(),
            screen_name: input.screen_name,
            refresh_strategy: input.refresh_strategy,
            enabled: input.enabled,
            last_refreshed_at: None,
            created_at: String::new(),
            updated_at: None,
            tier: input.tier,
            interval_secs: input.interval_secs,
            jitter_secs: input.jitter_secs,
            next_refresh_epoch: now_epoch,
            last_refresh_epoch: None,
        });
    core.save_monitored_user(user)
        .await
        .map_err(|error| sync_core_error("save_monitored_user", error))
}

#[tauri::command]
async fn delete_monitored_user(
    core: State<'_, Arc<Core>>,
    account_id: WeiboId,
    uid: WeiboId,
) -> Result<bool> {
    core.delete_monitored_user(account_id.into(), uid.into())
        .await
        .map_err(|error| sync_core_error("delete_monitored_user", error))
}

#[tauri::command]
async fn enqueue_sync_job(core: State<'_, Arc<Core>>, spec: SyncJobCommandSpec) -> Result<String> {
    validate_job_spec(&spec)?;
    let account_id = match &spec {
        SyncJobCommandSpec::CollectUserPosts { account_id, .. }
        | SyncJobCommandSpec::CollectComments { account_id, .. }
        | SyncJobCommandSpec::CollectCommentReplies { account_id, .. } => account_id.0,
    };
    require_ready_sync_account(&core, account_id).await?;
    core.enqueue_sync_job(spec.into())
        .await
        .map(|id| id.to_string())
        .map_err(|error| sync_core_error("enqueue_sync_job", error))
}

#[tauri::command]
async fn get_sync_jobs(core: State<'_, Arc<Core>>) -> Result<Vec<SyncJobWireDto>> {
    Ok(core
        .get_sync_jobs()
        .await
        .map_err(|error| sync_core_error("get_sync_jobs", error))?
        .into_iter()
        .map(Into::into)
        .collect())
}

#[tauri::command]
async fn get_sync_run_history(
    core: State<'_, Arc<Core>>,
    job_id: WeiboId,
    limit: u64,
) -> Result<Vec<SyncRunWireDto>> {
    Ok(core
        .get_sync_run_history(job_id.into(), limit)
        .await
        .map_err(|error| sync_core_error("get_sync_run_history", error))?
        .into_iter()
        .map(Into::into)
        .collect())
}

#[tauri::command]
async fn pause_sync_job(
    core: State<'_, Arc<Core>>,
    job_id: WeiboId,
) -> Result<SyncJobControlWireOutcome> {
    core.pause_sync_job(job_id.into())
        .await
        .map(sync_control_wire)
        .map_err(|error| sync_core_error("pause_sync_job", error))
}

#[tauri::command]
async fn resume_sync_job(core: State<'_, Arc<Core>>, job_id: WeiboId) -> Result<SyncJobWireDto> {
    core.resume_sync_job(job_id.into())
        .await
        .map(Into::into)
        .map_err(|error| sync_core_error("resume_sync_job", error))
}

#[tauri::command]
async fn cancel_sync_job(
    core: State<'_, Arc<Core>>,
    job_id: WeiboId,
) -> Result<SyncJobControlWireOutcome> {
    core.cancel_sync_job(job_id.into())
        .await
        .map(sync_control_wire)
        .map_err(|error| sync_core_error("cancel_sync_job", error))
}

#[tauri::command]
async fn retry_sync_job(core: State<'_, Arc<Core>>, job_id: WeiboId) -> Result<SyncJobWireDto> {
    core.retry_sync_job(job_id.into())
        .await
        .map(Into::into)
        .map_err(|error| sync_core_error("retry_sync_job", error))
}

#[tauri::command]
async fn unfavorite_posts(core: State<'_, Arc<Core>>) -> Result<()> {
    require_logged_in(&core).await?;
    info!("unfavorite_posts called");
    Ok(core.unfavorite_posts().await?)
}

#[tauri::command]
async fn export_posts(core: State<'_, Arc<Core>>, options: ExportJobOptions) -> Result<()> {
    info!("export_from_local called with options: {options:?}");
    Ok(core.export_posts(TaskRequest::Export(options)).await?)
}

#[tauri::command]
async fn query_local_posts(
    core: State<'_, Arc<Core>>,
    query: PostQuery,
) -> Result<PaginatedPostInfo> {
    validate_post_query(&query)?;
    info!("query_local_posts called with query: {query:?}");
    core.query_posts(query)
        .await
        .map_err(|error| sync_core_error("query_local_posts", error))
}

fn validate_post_query(query: &PostQuery) -> Result<()> {
    if query.page == 0 || query.posts_per_page == 0 || query.posts_per_page > 100 {
        return Err(stable_sync_error(INVALID_SYNC_INPUT));
    }
    if query.user_id.is_some_and(|id| id <= 0) {
        return Err(stable_sync_error(INVALID_SYNC_INPUT));
    }
    if query
        .start_date
        .zip(query.end_date)
        .is_some_and(|(start, end)| start > end)
    {
        return Err(stable_sync_error(INVALID_SYNC_INPUT));
    }
    if query.search_term.as_ref().is_some_and(|term| {
        let value = match term {
            weiback::core::task::SearchTerm::Fuzzy(value)
            | weiback::core::task::SearchTerm::Strict(value) => value,
        };
        value.is_empty() || value.len() > 256 || value.contains('\0')
    }) {
        return Err(stable_sync_error(INVALID_SYNC_INPUT));
    }
    if let Some(value) = query.content_type.as_deref()
        && !matches!(value, "all" | "picture" | "video" | "text")
    {
        return Err(stable_sync_error(INVALID_SYNC_INPUT));
    }
    if let Some(value) = query.content_status.as_deref()
        && !matches!(value, "all" | "complete" | "partial" | "failed")
    {
        return Err(stable_sync_error(INVALID_SYNC_INPUT));
    }
    if query.source.as_deref().is_some_and(|value| {
        value.is_empty() || value.len() > MAX_QUERY_SOURCE_LEN || value.contains('\0')
    }) {
        return Err(stable_sync_error(INVALID_SYNC_INPUT));
    }
    Ok(())
}

#[tauri::command]
async fn get_post_detail(
    core: State<'_, Arc<Core>>,
    id: WeiboId,
) -> Result<Option<weiback::core::task::PostInfo>> {
    if id.0 <= 0 {
        return Err(stable_sync_error(INVALID_SYNC_INPUT));
    }
    core.get_post_detail(id.0)
        .await
        .map_err(|error| sync_core_error("get_post_detail", error))
}

#[tauri::command]
async fn get_post_comments(
    core: State<'_, Arc<Core>>,
    post_id: WeiboId,
    root_id: Option<WeiboId>,
    offset: u32,
    limit: u32,
) -> Result<weiback::core::task::PaginatedCommentWire> {
    if post_id.0 <= 0 || root_id.is_some_and(|id| id.0 <= 0) || !(1..=100).contains(&limit) {
        return Err(stable_sync_error(INVALID_SYNC_INPUT));
    }
    core.get_post_comments(post_id.0, root_id.map(|id| id.0), offset, limit)
        .await
        .map_err(|error| sync_core_error("get_post_comments", error))
}

#[tauri::command]
async fn get_owner_media(
    core: State<'_, Arc<Core>>,
    owner_type: String,
    owner_id: WeiboId,
) -> Result<Vec<OwnerMediaWire>> {
    if !matches!(owner_type.as_str(), "post" | "user" | "comment")
        || owner_id.0 <= 0
        || owner_type.len() > 16
    {
        return Err(stable_sync_error(INVALID_SYNC_INPUT));
    }
    let root = get_config()
        .read()
        .map_err(|error| Error(error.to_string()))?
        .media_path
        .clone();
    core.get_owner_media(&owner_type, Some(owner_id.0))
        .await
        .map(|media| {
            media
                .into_iter()
                .map(|item| owner_media_wire(item, &root))
                .collect()
        })
        .map_err(|error| sync_core_error("get_owner_media", error))
}

#[tauri::command]
async fn get_media_blob(
    core: State<'_, Arc<Core>>,
    owner_type: String,
    owner_id: WeiboId,
    media_id: WeiboId,
) -> Result<Option<MediaBlobWire>> {
    if !matches!(owner_type.as_str(), "post" | "user" | "comment")
        || owner_type.len() > 16
        || owner_id.0 <= 0
        || media_id.0 <= 0
    {
        return Err(stable_sync_error(INVALID_SYNC_INPUT));
    }
    core.get_media_blob(&owner_type, owner_id.0, media_id.0)
        .await
        .map(|blob| {
            blob.map(|blob| MediaBlobWire {
                content_type: blob.content_type,
                bytes: blob.bytes,
            })
        })
        .map_err(|error| {
            error!("get_media_blob failed: {error}");
            stable_sync_error(SYNC_OPERATION_FAILED)
        })
}

#[tauri::command]
async fn retry_media(core: State<'_, Arc<Core>>, media_id: WeiboId) -> Result<bool> {
    if media_id.0 <= 0 {
        return Err(stable_sync_error(INVALID_SYNC_INPUT));
    }
    core.retry_media(media_id.0)
        .await
        .map_err(|error| sync_core_error("retry_media", error))
}

#[tauri::command]
async fn get_sync_diagnostics(core: State<'_, Arc<Core>>) -> Result<serde_json::Value> {
    let mut diagnostics = core
        .get_sync_diagnostics()
        .await
        .map_err(|error| sync_core_error("get_sync_diagnostics", error))?;
    if diagnostics_contains_sensitive_keys(&diagnostics) {
        error!("diagnostics payload contained a forbidden field");
        return Err(stable_sync_error(SYNC_OPERATION_FAILED));
    }
    if let Some(object) = diagnostics.as_object_mut() {
        object.insert(
            "tauri".into(),
            serde_json::json!({"version": env!("CARGO_PKG_VERSION"), "webview": null}),
        );
    }
    Ok(diagnostics)
}

fn diagnostics_contains_sensitive_keys(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Object(object) => object.iter().any(|(key, value)| {
            matches!(
                key.as_str(),
                "session_ref"
                    | "session"
                    | "path"
                    | "local_path"
                    | "token"
                    | "owner_token"
                    | "last_error"
                    | "error"
            ) || diagnostics_contains_sensitive_keys(value)
        }),
        serde_json::Value::Array(values) => values.iter().any(diagnostics_contains_sensitive_keys),
        _ => false,
    }
}

#[tauri::command]
async fn get_sms_code(core: State<'_, Arc<Core>>, phone_number: String) -> Result<()> {
    let phone_number = phone_number.trim();
    if !valid_phone_number(phone_number) {
        return Err(stable_sync_error(INVALID_SYNC_INPUT));
    }
    info!("SMS code requested");
    core.get_sms_code(phone_number.to_owned())
        .await
        .map_err(|_| sms_auth_error("request_code"))
}

#[tauri::command]
async fn login(core: State<'_, Arc<Core>>, sms_code: String) -> Result<User> {
    let sms_code = sms_code.trim();
    if !valid_sms_code(sms_code) {
        return Err(stable_sync_error(INVALID_SYNC_INPUT));
    }
    info!("SMS login requested");
    core.login(sms_code.to_owned())
        .await
        .map_err(|_| sms_auth_error("verify_code"))
}

#[tauri::command]
async fn login_state(core: State<'_, Arc<Core>>) -> Result<Option<User>> {
    info!("login_state called");
    Ok(core.login_state().await?)
}

#[tauri::command]
async fn delete_post(core: State<'_, Arc<Core>>, options: DeletePostOptions) -> Result<()> {
    info!("delete_post called with options: {options:?}");
    Ok(core.delete_post(options).await?)
}

#[tauri::command]
async fn rebackup_post(core: State<'_, Arc<Core>>, id: WeiboId) -> Result<()> {
    info!("rebackup_post called with id: {id:?}");
    Ok(core.rebackup_post(id.into()).await?)
}

#[tauri::command]
async fn rebackup_posts(core: State<'_, Arc<Core>>, query: PostQuery) -> Result<()> {
    info!("rebackup_posts called with query: {query:?}");
    Ok(core
        .rebackup_posts(TaskRequest::RebackupPosts(query))
        .await?)
}

#[tauri::command]
async fn rebackup_missing_images(core: State<'_, Arc<Core>>, query: PostQuery) -> Result<()> {
    info!("rebackup_missing_images called with query: {query:?}");
    Ok(core
        .rebackup_missing_images(TaskRequest::RebackupMissingImages(query))
        .await?)
}

#[tauri::command]
async fn get_username_by_id(core: State<'_, Arc<Core>>, uid: WeiboId) -> Result<Option<String>> {
    core.get_username_by_id(uid.into())
        .await
        .map_err(|e| Error(e.to_string()))
}

#[tauri::command(async)]
async fn search_id_by_username_prefix(
    core: State<'_, Arc<Core>>,
    prefix: String,
) -> Result<Vec<UserSearchWireDto>> {
    info!("search_id_by_username_prefix called with prefix: {prefix}");
    core.search_users_by_screen_name_prefix(&prefix)
        .await
        .map(|users| users.into_iter().map(UserSearchWireDto::from).collect())
        .map_err(|e| Error(e.to_string()))
}

#[tauri::command]
async fn cleanup_pictures(
    core: State<'_, Arc<Core>>,
    options: CleanupPicturesOptions,
) -> Result<()> {
    info!("cleanup_pictures called with options: {options:?}");
    Ok(core
        .cleanup_pictures(TaskRequest::CleanupPictures(options))
        .await?)
}

#[tauri::command]
async fn cleanup_outdated_avatars(core: State<'_, Arc<Core>>) -> Result<()> {
    info!("cleanup_invalid_avatars called");
    Ok(core.cleanup_outdated_avatars().await?)
}

#[tauri::command]
async fn cleanup_invalid_posts(
    core: State<'_, Arc<Core>>,
    options: CleanupInvalidPostsOptions,
) -> Result<()> {
    info!("cleanup_invalid_posts called with options: {options:?}");
    Ok(core
        .cleanup_invalid_posts(TaskRequest::CleanupInvalidPosts(options))
        .await?)
}

#[tauri::command]
async fn cleanup_invalid_pictures(core: State<'_, Arc<Core>>) -> Result<()> {
    info!("cleanup_invalid_pictures called");
    Ok(core
        .cleanup_invalid_pictures(TaskRequest::CleanupInvalidPictures)
        .await?)
}

#[tauri::command]
async fn create_user_backup(
    core: State<'_, Arc<Core>>,
) -> Result<weiback::user_backup::UserBackupSummary> {
    core.create_user_backup().await.map_err(|error| {
        error!(error = %error, "create user backup failed");
        stable_sync_error(USER_BACKUP_FAILED)
    })
}

#[tauri::command]
async fn list_user_backups(
    core: State<'_, Arc<Core>>,
) -> Result<Vec<weiback::user_backup::UserBackupSummary>> {
    core.list_user_backups().await.map_err(|error| {
        error!(error = %error, "list user backups failed");
        stable_sync_error(USER_BACKUP_FAILED)
    })
}

#[tauri::command]
async fn verify_user_backup(
    core: State<'_, Arc<Core>>,
    backup_id: String,
) -> Result<weiback::user_backup::UserBackupVerification> {
    core.verify_user_backup(&backup_id).await.map_err(|error| {
        error!(error = %error, "verify user backup failed");
        stable_sync_error(USER_BACKUP_FAILED)
    })
}

#[tauri::command]
async fn restore_user_backup(
    app_handle: AppHandle,
    core: State<'_, Arc<Core>>,
    backup_id: String,
) -> Result<weiback::user_backup::UserRestoreSummary> {
    let summary = core
        .restore_user_backup(&backup_id)
        .await
        .map_err(|error| {
            error!(error = %error, "restore user backup failed");
            if error.to_string().contains("ordinary task is active") {
                return Error(USER_RESTORE_ACTIVE_TASK.into());
            }
            if error.to_string().contains("restart required") {
                return Error(USER_RESTORE_RESTART_REQUIRED.into());
            }
            stable_sync_error(USER_BACKUP_FAILED)
        })?;
    if summary.restart_required {
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            app_handle.exit(0);
        });
    }
    Ok(summary)
}

#[tauri::command]
fn detect_legacy_sources() -> Vec<weiback::legacy::LegacyDetection> {
    info!("detect_legacy_sources called");
    weiback::legacy::detect_legacy_sources(&dirs::data_dir().unwrap_or_default())
}

#[tauri::command]
async fn inspect_legacy_source(source_path: String) -> Result<weiback::legacy::LegacyDetection> {
    let source = validated_legacy_source(&source_path)?;
    let runtime = weiback::config::runtime_dirs();
    weiback::legacy::inspect_legacy_source(&source, &runtime.db_path)
        .await
        .map_err(|error| {
            warn!(error = %error, "legacy source inspection failed");
            stable_sync_error(INVALID_LEGACY_INPUT)
        })
}

#[tauri::command]
async fn import_legacy_source(source_path: String) -> Result<weiback::legacy::LegacyImportSummary> {
    let source = validated_legacy_source(&source_path)?;
    let runtime = weiback::config::runtime_dirs();
    runtime.ensure_created().map_err(|error| {
        error!(error = %error, "create import runtime directories failed");
        stable_sync_error(LEGACY_IMPORT_FAILED)
    })?;
    let pool =
        weiback::storage::database::create_db_pool_with_url(&runtime.db_path.to_string_lossy())
            .await
            .map_err(|error| {
                error!(error = %error, "open target database for legacy import failed");
                stable_sync_error(LEGACY_IMPORT_FAILED)
            })?;
    let result = weiback::legacy::import_legacy_source(
        &pool,
        weiback::legacy::LegacyImportRequest::new(source, runtime.media_dir, runtime.imports_dir),
    )
    .await
    .map_err(|error| {
        error!(error = %error, "legacy import failed");
        stable_sync_error(LEGACY_IMPORT_FAILED)
    });
    pool.close().await;
    result
}

/// Sidecar 握手诊断：解析并启动 collector，完成握手后返回
/// `ready`/`capabilities` 的 JSON 摘要；任何失败都返回可诊断错误。
///
/// 解析顺序：环境变量 `WEIBACK_COLLECTOR_CMD` → 可执行文件同目录的
/// `weiback-collector(.exe)`。额外的命令行参数可由
/// `WEIBACK_COLLECTOR_ARGS`（空格分隔）指定，开发时可用它指向
/// `-u -m weiback_collector`；发布后使用 externalBin 打包的二进制时留空。
#[tauri::command]
fn sidecar_diagnostics() -> Result<serde_json::Value> {
    let Some(program) = weiback::sidecar::supervisor::resolve_sidecar_command() else {
        return Err(Error(
            "Sidecar not found: set WEIBACK_COLLECTOR_CMD or place weiback-collector(.exe) next to the app".to_string(),
        ));
    };

    let args = std::env::var("WEIBACK_COLLECTOR_ARGS")
        .map(|s| {
            s.split_whitespace()
                .map(str::to_string)
                .collect::<Vec<String>>()
        })
        .unwrap_or_default();

    let options = weiback::sidecar::SpawnOptions {
        program,
        args,
        env: vec![("PYTHONUTF8".into(), "1".into())],
        cwd: None,
        handshake_timeout: std::time::Duration::from_secs(10),
    };

    match weiback::sidecar::Sidecar::spawn_with_handshake(&options) {
        Ok((mut sidecar, ready, capabilities)) => {
            let _ = sidecar.shutdown(std::time::Duration::from_millis(500));
            Ok(serde_json::json!({
                "ok": true,
                "ready": ready,
                "capabilities": capabilities,
            }))
        }
        Err(e) => Err(Error(format!(
            "sidecar handshake failed: {e} (set WEIBACK_COLLECTOR_CMD to the collector executable)"
        ))),
    }
}

pub fn run() -> Result<()> {
    info!("Starting application");

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_http::init())
        .plugin(tauri_plugin_shell::init())
        .setup(setup)
        .invoke_handler(tauri::generate_handler![
            get_backend_status,
            init_backend,
            backup_user,
            backup_favorites,
            collect_user_posts,
            collect_comments,
            collect_comment_replies,
            get_sync_accounts,
            save_sync_account,
            delete_sync_account,
            get_monitored_users,
            save_monitored_user,
            delete_monitored_user,
            enqueue_sync_job,
            get_sync_jobs,
            get_sync_run_history,
            pause_sync_job,
            resume_sync_job,
            cancel_sync_job,
            retry_sync_job,
            unfavorite_posts,
            export_posts,
            query_local_posts,
            get_post_detail,
            get_post_comments,
            get_owner_media,
            get_media_blob,
            retry_media,
            get_sync_diagnostics,
            get_sms_code,
            login,
            login_state,
            get_config_command,
            set_config_command,
            get_username_by_id,
            search_id_by_username_prefix,
            get_picture_blob,
            get_video_blob,
            delete_post,
            rebackup_post,
            rebackup_posts,
            rebackup_missing_images,
            get_current_task_status,
            get_and_clear_task_errors,
            cleanup_pictures,
            cleanup_outdated_avatars,
            cleanup_invalid_posts,
            cleanup_invalid_pictures,
            create_user_backup,
            list_user_backups,
            verify_user_backup,
            restore_user_backup,
            detect_legacy_sources,
            inspect_legacy_source,
            import_legacy_source,
            sidecar_diagnostics
        ])
        .build(tauri::generate_context!())
        .expect("tauri app build failed")
        .run(|app_handle, event| {
            if let tauri::RunEvent::ExitRequested { code, api, .. } = event
                && code.is_none()
            {
                api.prevent_exit();
                let state = app_handle.state::<BackendState>();
                if state
                    .exit_started
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_err()
                {
                    return;
                }
                let app_handle = app_handle.clone();
                tauri::async_runtime::spawn(async move {
                    if let Some(core) = app_handle.try_state::<Arc<Core>>() {
                        let summary = core
                            .shutdown_persistent_tasks(std::time::Duration::from_secs(5))
                            .await;
                        if summary.degraded() {
                            warn!(?summary, "persistent task shutdown was degraded");
                            app_handle
                                .state::<BackendState>()
                                .exit_started
                                .store(false, Ordering::Release);
                            return;
                        }
                        info!(?summary, "persistent tasks stopped");
                    }
                    #[cfg(feature = "dev-mode")]
                    weiback::dev_client::save_records();
                    app_handle.exit(0);
                });
            }
        });
    Ok(())
}

fn setup(app: &mut App) -> std::result::Result<(), Box<dyn std::error::Error>> {
    info!("Setting up Tauri application state");
    let state = BackendState {
        status: Mutex::new(BackendStatus::Uninitialized),
        exit_started: AtomicBool::new(false),
    };

    app.manage(state);
    info!("Tauri setup complete");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sync_wire_dtos_stringify_large_ids_and_exclude_sensitive_fields() {
        let account = AccountWireDto {
            id: i64::MAX.to_string(),
            provider: "weibo".into(),
            uid: i64::MAX.to_string(),
            display_name: Some("Alice".into()),
            enabled: true,
            has_session: true,
            session_ready: true,
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: None,
        };
        let job = SyncJobWireDto {
            id: i64::MAX.to_string(),
            resource_key: "user:1".into(),
            name: "Collect".into(),
            kind: "collect_user_posts".into(),
            status: "pending".into(),
            priority: "1".into(),
            schedule_config: None,
            enabled: true,
            recovery_count: "0".into(),
            max_recovery_attempts: "3".into(),
            available_at: None,
            available_at_epoch: "0".into(),
            claimed_at: None,
            current_run_id: Some(i64::MAX.to_string()),
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: None,
            account_id: i64::MAX.to_string(),
            endpoint_key: "posts".into(),
        };
        let run = SyncRunWireDto {
            id: i64::MAX.to_string(),
            job_id: i64::MAX.to_string(),
            status: "running".into(),
            started_at: "2026-01-01T00:00:00Z".into(),
            finished_at: None,
            stats_json: None,
            attempt: "1".into(),
            updated_at: None,
        };

        let account_json = serde_json::to_value(account).unwrap();
        let job_json = serde_json::to_value(job.clone()).unwrap();
        let run_json = serde_json::to_value(run).unwrap();
        assert_eq!(account_json["id"], json!(i64::MAX.to_string()));
        assert_eq!(account_json["session_ready"], json!(true));
        assert_eq!(job_json["current_run_id"], json!(i64::MAX.to_string()));
        assert_eq!(run_json["job_id"], json!(i64::MAX.to_string()));
        for value in [&account_json, &job_json, &run_json] {
            let object = value.as_object().unwrap();
            for forbidden in [
                "session_ref",
                "owner_token",
                "lease_until_epoch",
                "generation",
                "endpoint_gate_revision",
                "account_gate_revision",
                "payload_json",
                "error",
            ] {
                assert!(!object.contains_key(forbidden), "found {forbidden}");
            }
        }

        let control_json = serde_json::to_value(SyncJobControlWireOutcome {
            job,
            worker_stop: ControlStopWireResult::WorkerStarting,
        })
        .unwrap();
        assert_eq!(control_json["worker_stop"]["status"], "worker_starting");
        assert_eq!(control_json["job"]["id"], i64::MAX.to_string());
    }

    #[test]
    fn sync_inputs_have_only_public_editable_fields() {
        let account: SaveSyncAccountInput = serde_json::from_value(json!({
            "id": "9007199254740993",
            "provider": "weibo",
            "uid": "123",
            "display_name": "Alice",
            "session_ref": "sessions/alice.json",
            "enabled": true
        }))
        .unwrap();
        assert_eq!(i64::from(account.id.unwrap()), 9_007_199_254_740_993_i64);

        let patch: SaveSyncAccountInput = serde_json::from_value(json!({
            "id": "1",
            "provider": "weibo",
            "uid": "123",
            "display_name": "Renamed",
            "enabled": false
        }))
        .unwrap();
        assert!(patch.session_ref.is_none());

        let user: SaveMonitoredUserInput = serde_json::from_value(json!({
            "account_id": "1",
            "uid": "9007199254740993",
            "screen_name": "Alice",
            "refresh_strategy": "scheduled",
            "enabled": true,
            "tier": "hot",
            "interval_secs": 60,
            "jitter_secs": 5
        }))
        .unwrap();
        assert_eq!(i64::from(user.uid), 9_007_199_254_740_993_i64);
    }

    #[test]
    fn sync_boundary_validation_rejects_unsafe_paths_and_bad_ranges() {
        for path in [
            "../session.json",
            "/session.json",
            "C:\\session.json",
            "\\\\host\\session.json",
        ] {
            assert!(validate_session_ref(path).is_err(), "accepted {path}");
        }
        assert!(validate_session_ref("sessions/alice.json").is_ok());
        let mut account: SaveSyncAccountInput = serde_json::from_value(json!({
            "provider": "weibo", "uid": "2", "session_ref": "sessions/alice.json",
            "display_name": null, "enabled": true
        }))
        .unwrap();
        assert!(validate_account_input(&account).is_ok());
        account.provider.clear();
        assert!(validate_account_input(&account).is_err());

        let mut monitor: SaveMonitoredUserInput = serde_json::from_value(json!({
            "account_id": "1", "uid": "2", "refresh_strategy": "hot",
            "enabled": true, "tier": "hot", "interval_secs": 60, "jitter_secs": 5
        }))
        .unwrap();
        assert!(validate_monitor_input(&monitor).is_ok());
        monitor.refresh_strategy = "scheduled".into();
        assert!(validate_monitor_input(&monitor).is_err());
        monitor.refresh_strategy = "hot".into();
        monitor.jitter_secs = 61;
        assert!(validate_monitor_input(&monitor).is_err());

        let spec: SyncJobCommandSpec = serde_json::from_value(json!({
            "kind": "collect_user_posts", "account_id": "1", "uid": "2",
            "max_pages": 1001, "priority": 1
        }))
        .unwrap();
        assert!(validate_job_spec(&spec).is_err());
        let negative: SyncJobCommandSpec = serde_json::from_value(json!({
            "kind": "collect_comments", "account_id": "-1", "post_id": "2",
            "max_pages": 1, "priority": 1
        }))
        .unwrap();
        assert!(validate_job_spec(&negative).is_err());
    }

    #[test]
    fn p3_wires_preserve_large_ids_and_diagnostics_are_redacted() {
        let comment = weiback::storage::internal::entities::CommentWire {
            id: i64::MAX.to_string(),
            post_id: "9007199254740993".into(),
            root_id: Some("1".into()),
            parent_id: Some("1".into()),
            user_id: Some(i64::MAX.to_string()),
            text: "text".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            depth: 1,
            child_count: 0,
            like_count: 2,
            source: Some("web".into()),
            content_status: "complete".into(),
            deleted: false,
        };
        let media = OwnerMediaWire {
            id: i64::MAX.to_string(),
            owner_type: "comment".into(),
            owner_id: "9007199254740993".into(),
            media_type: "picture".into(),
            remote_url: "https://example.invalid/a.jpg".into(),
            local_available: false,
            status: "failed".into(),
            definition: Some("large".into()),
            retry_count: "3".into(),
            created_at: "created".into(),
            updated_at: Some("updated".into()),
        };
        let comment_json = serde_json::to_value(comment).unwrap();
        let media_json = serde_json::to_value(media).unwrap();
        assert_eq!(comment_json["id"], i64::MAX.to_string());
        assert_eq!(media_json["owner_id"], "9007199254740993");
        assert!(media_json.get("last_error").is_none());
        assert!(media_json.get("local_path").is_none());
        assert!(!diagnostics_contains_sensitive_keys(
            &json!({"accounts":{"total":1},"rate_gates":[{"account_id":"1","endpoint":"comments","next_allowed":1,"backoff":2}]})
        ));
        assert!(diagnostics_contains_sensitive_keys(
            &json!({"session_ref":"secret"})
        ));
        assert!(diagnostics_contains_sensitive_keys(
            &json!({"nested":{"local_path":"secret"}})
        ));
    }

    #[test]
    fn media_blob_wire_is_binary_safe_and_never_exposes_storage_metadata() {
        let value = serde_json::to_value(MediaBlobWire {
            content_type: "image/png".into(),
            bytes: vec![0, 1, 255],
        })
        .unwrap();
        assert_eq!(value["content_type"], "image/png");
        assert_eq!(value["bytes"], json!([0, 1, 255]));
        let object = value.as_object().unwrap();
        assert_eq!(object.len(), 2);
        for forbidden in [
            "local_path",
            "path",
            "remote_url",
            "url",
            "error",
            "last_error",
        ] {
            assert!(!object.contains_key(forbidden));
        }
    }

    #[test]
    fn owner_media_wire_exposes_only_conservative_public_https_urls() {
        for (url, expected) in [
            (
                "https://cdn.example.test/image.jpg",
                "https://cdn.example.test/image.jpg",
            ),
            ("http://cdn.example.test/image.jpg", ""),
            ("https://127.0.0.1/image.jpg", ""),
            ("https://localhost/image.jpg", ""),
            ("https://api.localhost/image.jpg", ""),
            ("https://user:password@cdn.example.test/image.jpg", ""),
            ("https://cdn.example.test/image.jpg?token=secret", ""),
            ("https://cdn.example.test/image.jpg#fragment", ""),
        ] {
            let wire = owner_media_wire(
                weiback::storage::internal::entities::OwnerMediaDto {
                    id: 1,
                    owner_type: "post".into(),
                    owner_id: Some(1),
                    media_type: "picture".into(),
                    url: url.into(),
                    local_path: None,
                    status: "pending".into(),
                    retry_count: 0,
                    last_error: None,
                    created_at: "now".into(),
                    updated_at: None,
                    definition: None,
                },
                std::path::Path::new("."),
            );
            assert_eq!(wire.remote_url, expected, "must not expose {url}");
        }
    }

    #[test]
    fn user_search_wire_stringifies_large_ids_and_excludes_private_profile_fields() {
        let value = serde_json::to_value(UserSearchWireDto {
            id: "9007199254740993".into(),
            screen_name: "large-id-user".into(),
        })
        .unwrap();
        assert_eq!(value["id"], "9007199254740993");
        assert_eq!(value["screen_name"], "large-id-user");
        let object = value.as_object().unwrap();
        assert!(!object.contains_key("avatar_hd"));
        assert!(!object.contains_key("domain"));
    }

    #[test]
    fn p3_comment_and_media_json_match_the_safe_frontend_contract() {
        let comments = weiback::core::task::PaginatedCommentWire {
            items: vec![weiback::storage::internal::entities::CommentWire {
                id: i64::MAX.to_string(),
                post_id: "9007199254740993".into(),
                root_id: None,
                parent_id: None,
                user_id: None,
                text: "text".into(),
                created_at: "now".into(),
                depth: 0,
                child_count: 0,
                like_count: 0,
                source: None,
                content_status: "complete".into(),
                deleted: false,
            }],
            total_items: "1".into(),
            offset: 0,
            limit: 20,
        };
        let comments_json = serde_json::to_value(comments).unwrap();
        assert_eq!(comments_json["total_items"], "1");
        assert!(comments_json["items"].is_array());
        assert!(comments_json.get("comments").is_none());

        let media = OwnerMediaWire {
            id: i64::MAX.to_string(),
            owner_type: "post".into(),
            owner_id: "1".into(),
            media_type: "picture".into(),
            remote_url: "https://example.invalid/a.jpg".into(),
            local_available: false,
            status: "failed".into(),
            definition: None,
            retry_count: "3".into(),
            created_at: "created".into(),
            updated_at: Some("updated".into()),
        };
        let serde_json::Value::Object(media_json) = serde_json::to_value(media).unwrap() else {
            panic!("object expected")
        };
        assert_eq!(media_json.len(), 11);
        for key in [
            "id",
            "owner_type",
            "owner_id",
            "media_type",
            "remote_url",
            "local_available",
            "status",
            "definition",
            "retry_count",
            "created_at",
            "updated_at",
        ] {
            assert!(media_json.contains_key(key), "missing {key}");
        }
        assert!(!media_json.contains_key("local_path"));
        assert!(!media_json.contains_key("last_error"));
    }

    #[test]
    fn p3_post_query_validation_is_strict() {
        let mut query = PostQuery::default();
        assert!(validate_post_query(&query).is_ok());
        query.posts_per_page = 101;
        assert!(validate_post_query(&query).is_err());
        query.posts_per_page = 20;
        query.content_type = Some("audio".into());
        assert!(validate_post_query(&query).is_err());
        query.content_type = Some("picture".into());
        query.content_status = Some("unknown".into());
        assert!(validate_post_query(&query).is_err());
    }

    #[test]
    fn sms_inputs_are_validated_and_errors_are_stable() {
        assert!(valid_phone_number("+8613800138000"));
        assert!(valid_phone_number("13800138000"));
        assert!(!valid_phone_number("138 0013 8000"));
        assert!(!valid_phone_number("123"));
        assert!(valid_sms_code("123456"));
        assert!(!valid_sms_code("12a456"));

        let error = sms_auth_error("verify_code");
        assert_eq!(error.0, SYNC_OPERATION_FAILED);
        assert!(!error.0.contains("13800138000"));
        assert!(!error.0.contains("123456"));
    }

    #[test]
    fn sync_worker_failure_is_mapped_without_detail() {
        let mapped = safe_worker_stop(ControlStopResult::StopFailed("secret db path".into()));
        let json = serde_json::to_value(mapped).unwrap();
        assert_eq!(json["detail"], SYNC_OPERATION_FAILED);
        assert!(!json.to_string().contains("secret db path"));
    }

    #[test]
    fn tauri_config_uses_the_permanent_next_identity() {
        let config: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();

        assert_eq!(config["productName"], "WeiBack Next");
        assert_eq!(config["mainBinaryName"], "weiback-next");
        assert_eq!(config["identifier"], "com.weiback.next");
        assert_eq!(config["app"]["windows"][0]["title"], "WeiBack Next");
    }
}
