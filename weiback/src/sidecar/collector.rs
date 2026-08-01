//! Rust 侧采集执行器。
//!
//! 把 Sidecar 事件流（post/comment/media_reference/checkpoint/done/error）转成
//! 批事务写入，并发布可信进度。实现 P1-B 门槛：
//!
//! - 数据事件与 checkpoint 同一事务提交（Rust 是唯一写入者）；
//! - 崩溃（进程退出/超时）时未提交批被丢弃，任务标记 `Interrupted`；
//! - 重启后从 `sync_checkpoints` 读取最后已提交游标续传，已提交页不丢不重；
//! - 认证失效/限流以事件上报，不损坏任务或数据库。

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, SyncSender, TryRecvError},
    },
    time::{Duration, Instant},
};

use chrono::Utc;
use serde_json::{Value, json};
use sqlx::SqlitePool;
use tracing::{info, warn};

use crate::error::{Error, Result};
use crate::models::User;
use crate::sidecar::protocol::{CommandEnvelope, CommandType, EventType, new_uuid_v7};
use crate::sidecar::supervisor::{Sidecar, SidecarError};
use crate::storage::internal::entities::transactional::CommitPlan;
use crate::storage::internal::entities::{
    CheckpointOwner, CommentDto, MediaDto, SyncCheckpointDto, get_sync_checkpoint, get_sync_job,
    heartbeat_sync_run_at,
};
use crate::storage::internal::post::PostInternal;

/// 一次采集请求。
#[derive(Debug, Clone)]
pub struct CollectionRequest {
    /// 命令类型：`CollectUserPosts` / `CollectComments` / `CollectCommentReplies`。
    pub command_type: CommandType,
    /// 逻辑资源流（`user:{uid}:posts` 等），也是 checkpoint 的键。
    pub stream: String,
    /// collect 命令 payload（uid/post_id/max_pages 等，不含 checkpoint）。
    pub payload: Value,
}

/// 采集结束状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollectionStatus {
    /// 正常完成（done status=completed）。
    Completed,
    /// 被取消或提前停止（done status=stopped / cancelled / REQUEST_CANCELLED）。
    Stopped,
    /// 持久任务被人工暂停。
    Paused,
    /// 持久任务被人工取消。
    Cancelled,
    /// Sidecar 崩溃或超时，任务应标记为 `Interrupted`。
    Interrupted,
    /// 应用正常退出，任务中断并重排但不消耗故障恢复预算。
    Shutdown,
    /// 上游错误（认证失效等），任务应标记为 `Failed`。
    Failed,
    /// Persistent executor must persist a gate and requeue the job.
    RateLimited,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateLimitScope {
    Account,
    Endpoint,
    Request,
}

impl RateLimitScope {
    pub fn parse_protocol(value: &str) -> Result<Self> {
        match value {
            "request" => Ok(Self::Request),
            "endpoint" => Ok(Self::Endpoint),
            "account" => Ok(Self::Account),
            _ => Err(Error::FormatError(format!(
                "unknown rate-limit scope: {value}"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateLimitInfo {
    pub scope: RateLimitScope,
    pub retry_after_ms: Option<u64>,
}

/// Worker registry 发给持久采集器的控制动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionControl {
    Pause,
    Cancel,
    Shutdown,
}

/// 控制请求携带一次性 ack；ack 仅在 kill+wait 确认后发送。
#[derive(Debug)]
pub struct ExecutionControlRequest {
    pub action: ExecutionControl,
    pub ack: SyncSender<std::result::Result<(), String>>,
}

/// 持久运行的 fencing 身份和轮询参数。
pub struct PersistentExecution<'a> {
    pub job_id: i64,
    pub run_id: i64,
    pub generation: i64,
    pub owner_token: &'a str,
    /// Account-scoped durable key; distinct from the protocol event stream.
    pub checkpoint_stream: &'a str,
    pub control_rx: &'a Receiver<ExecutionControlRequest>,
    pub poll_interval: Duration,
    pub heartbeat_interval: Duration,
    pub lease_duration: Duration,
}

/// 采集结果摘要。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectionSummary {
    /// 结束状态。
    pub status: CollectionStatus,
    /// 最后已提交 checkpoint 的 fetched_count。
    pub fetched_count: u64,
    /// 已提交的页数。
    pub pages: u64,
    /// 失败/中断时的可诊断错误。
    pub error: Option<String>,
    pub rate_limit: Option<RateLimitInfo>,
}

impl Default for CollectionSummary {
    fn default() -> Self {
        Self {
            status: CollectionStatus::Stopped,
            fetched_count: 0,
            pages: 0,
            error: None,
            rate_limit: None,
        }
    }
}

/// 运行一次采集：发送 collect 命令，消费事件流，逐 checkpoint 批事务提交。
///
/// `on_progress` 在每页提交后回调 `(fetched_count, total_expected)`。
/// `event_timeout` 是单次事件读取超时；超时按 Sidecar 崩溃处理（`Interrupted`）。
pub async fn run_collection(
    sidecar: &mut Sidecar,
    pool: &SqlitePool,
    request: &CollectionRequest,
    on_progress: impl FnMut(u64, u64),
    event_timeout: Duration,
) -> Result<CollectionSummary> {
    run_collection_with_execution(
        sidecar,
        pool,
        request,
        &mut None,
        None,
        on_progress,
        event_timeout,
    )
    .await
}

pub async fn run_collection_cancellable(
    sidecar: &mut Sidecar,
    pool: &SqlitePool,
    request: &CollectionRequest,
    cancelled: &AtomicBool,
    on_progress: impl FnMut(u64, u64),
    event_timeout: Duration,
) -> Result<CollectionSummary> {
    run_collection_with_execution(
        sidecar,
        pool,
        request,
        &mut None,
        Some(ExternalStop::Cancel(cancelled)),
        on_progress,
        event_timeout,
    )
    .await
}

/// Runs an ad-hoc collection that becomes interrupted when the process shuts down.
pub async fn run_collection_interruptible(
    sidecar: &mut Sidecar,
    pool: &SqlitePool,
    request: &CollectionRequest,
    interrupted: &AtomicBool,
    on_progress: impl FnMut(u64, u64),
    event_timeout: Duration,
) -> Result<CollectionSummary> {
    run_collection_with_execution(
        sidecar,
        pool,
        request,
        &mut None,
        Some(ExternalStop::Interrupt(interrupted)),
        on_progress,
        event_timeout,
    )
    .await
}

/// 持久模式采集；ad-hoc 调用继续走 [`run_collection`]。
pub async fn run_collection_persistent(
    sidecar: &mut Sidecar,
    pool: &SqlitePool,
    request: &CollectionRequest,
    execution: &mut PersistentExecution<'_>,
    on_progress: impl FnMut(u64, u64),
    event_timeout: Duration,
) -> Result<CollectionSummary> {
    run_collection_with_execution(
        sidecar,
        pool,
        request,
        &mut Some(execution),
        None,
        on_progress,
        event_timeout,
    )
    .await
}

async fn run_collection_with_execution(
    sidecar: &mut Sidecar,
    pool: &SqlitePool,
    request: &CollectionRequest,
    execution: &mut Option<&mut PersistentExecution<'_>>,
    external_stop: Option<ExternalStop<'_>>,
    mut on_progress: impl FnMut(u64, u64),
    event_timeout: Duration,
) -> Result<CollectionSummary> {
    // 1. 加载已有 checkpoint（续传），并把游标注入命令 payload。
    let mut payload = request.payload.clone();
    let checkpoint_stream = execution
        .as_deref()
        .map(|persistent| persistent.checkpoint_stream)
        .unwrap_or(&request.stream);
    if let Some(checkpoint) = load_checkpoint_cursor(pool, checkpoint_stream).await? {
        payload["checkpoint"] = checkpoint;
    }
    let initial_fetched_count = payload
        .get("checkpoint")
        .and_then(|checkpoint| checkpoint.get("fetched_count"))
        .and_then(Value::as_u64)
        .unwrap_or(0);

    let command = CommandEnvelope::new(new_uuid_v7(), request.command_type, payload);
    info!(
        "collection start: stream={} command={}",
        request.stream, command.command_type
    );
    sidecar
        .send_command(&command)
        .map_err(|e| Error::FormatError(format!("send command failed: {e}")))?;

    let mut summary = CollectionSummary {
        fetched_count: initial_fetched_count,
        ..CollectionSummary::default()
    };
    let mut batch = PendingBatch::default();
    let mut done = false;
    let mut last_sequence = 0;
    let mut event_deadline = Instant::now() + event_timeout;
    let mut next_heartbeat = Instant::now();

    while !done {
        if external_stop.is_some_and(ExternalStop::triggered) {
            sidecar
                .kill_and_wait()
                .map_err(|error| Error::FormatError(error.to_string()))?;
            summary.status = external_stop
                .map(ExternalStop::status)
                .unwrap_or(CollectionStatus::Interrupted);
            break;
        }
        if let Some(status) = poll_persistent_control(sidecar, execution)? {
            summary.status = status;
            break;
        }
        if let Some(persistent) = execution.as_deref_mut()
            && Instant::now() >= next_heartbeat
        {
            let now = Utc::now();
            let lease_until = now.timestamp()
                + i64::try_from(persistent.lease_duration.as_secs()).unwrap_or(i64::MAX);
            if !heartbeat_sync_run_at(
                pool,
                persistent.job_id,
                persistent.run_id,
                persistent.owner_token,
                persistent.generation,
                now.timestamp(),
                lease_until,
                &now.to_rfc3339(),
            )
            .await?
            {
                if let Ok(request) = persistent.control_rx.recv_timeout(persistent.poll_interval) {
                    let stopped = sidecar.kill_and_wait().map_err(|error| error.to_string());
                    let _ = request.ack.send(stopped.clone());
                    stopped.map_err(Error::FormatError)?;
                    summary.status = match request.action {
                        ExecutionControl::Pause => CollectionStatus::Paused,
                        ExecutionControl::Cancel => CollectionStatus::Cancelled,
                        ExecutionControl::Shutdown => CollectionStatus::Shutdown,
                    };
                    break;
                }
                sidecar
                    .kill_and_wait()
                    .map_err(|error| Error::FormatError(error.to_string()))?;
                summary.status = controlled_status_from_db(pool, persistent.job_id).await?;
                break;
            }
            next_heartbeat = Instant::now() + persistent.heartbeat_interval;
        }

        let remaining = event_deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            summary.status = CollectionStatus::Interrupted;
            summary.error = Some("sidecar event receive timed out".to_string());
            break;
        }
        let slice = execution
            .as_deref()
            .map(|persistent| persistent.poll_interval.min(remaining))
            .or_else(|| external_stop.map(|_| Duration::from_millis(100).min(remaining)))
            .unwrap_or(remaining);
        let event = match sidecar.next_event(slice) {
            Ok(event) => event,
            Err(SidecarError::Exited { code, .. }) => {
                summary.status = CollectionStatus::Interrupted;
                summary.error = Some(format!("sidecar exited with code {code:?}"));
                break;
            }
            Err(SidecarError::RecvTimeout(_)) if execution.is_some() || external_stop.is_some() => {
                continue;
            }
            Err(SidecarError::RecvTimeout(_)) => {
                summary.status = CollectionStatus::Interrupted;
                summary.error = Some("sidecar event receive timed out".to_string());
                break;
            }
            Err(e) => {
                summary.status = CollectionStatus::Interrupted;
                summary.error = Some(format!("sidecar error: {e}"));
                break;
            }
        };

        if event.request_id.as_deref() != Some(command.request_id.as_str()) {
            return Err(Error::FormatError(
                "sidecar event request_id mismatch".to_string(),
            ));
        }
        let stream = event
            .stream
            .clone()
            .ok_or_else(|| Error::FormatError("sidecar event stream missing".to_string()))?;
        if stream != request.stream {
            return Err(Error::FormatError(format!(
                "sidecar event stream mismatch: expected {}, got {stream}",
                request.stream
            )));
        }
        let sequence = event
            .sequence
            .ok_or_else(|| Error::FormatError("sidecar event sequence missing".to_string()))?;
        validate_event_sequence(last_sequence, sequence)?;
        last_sequence = sequence;
        event_deadline = Instant::now() + event_timeout;
        match event.event_type {
            EventType::User => {
                let user = user_from_payload(&event.payload).ok_or_else(|| {
                    Error::FormatError(format!("invalid user event {}", event.event_id))
                })?;
                batch.users.push(user);
            }
            EventType::Post => {
                batch.posts.push(post_from_payload(&event.payload)?);
            }
            EventType::Comment => {
                batch.comments.push(comment_from_payload(&event.payload)?);
            }
            EventType::MediaReference => {
                batch.media.push(media_from_payload(&event.payload)?);
            }
            EventType::Checkpoint => {
                let mut checkpoint =
                    checkpoint_from_payload(checkpoint_stream, &event.payload, sequence)?;
                if let Some(persistent) = execution.as_deref() {
                    checkpoint.job_id = Some(persistent.job_id);
                    checkpoint.run_id = Some(persistent.run_id);
                    checkpoint.generation = Some(persistent.generation);
                    checkpoint.owner_token = Some(persistent.owner_token.to_string());
                    checkpoint.owner = CheckpointOwner::Persistent {
                        run_id: persistent.run_id,
                        generation: persistent.generation,
                        owner_token: persistent.owner_token.to_string(),
                    };
                }
                let event_id = event.event_id.clone();
                let plan = CommitPlan {
                    request_id: Some(command.request_id.clone()),
                    stream: checkpoint_stream.to_string(),
                    sequence: sequence as i64,
                    event_id: event_id.clone(),
                    users: std::mem::take(&mut batch.users),
                    posts: std::mem::take(&mut batch.posts),
                    comments: std::mem::take(&mut batch.comments),
                    media: std::mem::take(&mut batch.media),
                    checkpoint: checkpoint.clone(),
                    processed_at: now(),
                };
                if let Err(error) = plan.execute_at(pool, Utc::now().timestamp()).await {
                    if let Some(persistent) = execution.as_deref_mut() {
                        let status = controlled_status_from_db(pool, persistent.job_id).await?;
                        if matches!(
                            status,
                            CollectionStatus::Paused | CollectionStatus::Cancelled
                        ) {
                            sidecar
                                .kill_and_wait()
                                .map_err(|error| Error::FormatError(error.to_string()))?;
                            summary.status = status;
                            break;
                        }
                    }
                    return Err(error);
                }
                summary.pages += 1;
                summary.fetched_count = checkpoint.fetched_count as u64;
                let total = event.total_expected.unwrap_or(0);
                on_progress(summary.fetched_count, total);
                info!(
                    "collection checkpoint committed: stream={stream} event={event_id} fetched={}",
                    checkpoint.fetched_count
                );
            }
            EventType::Done => {
                if !batch.is_empty() {
                    return Err(Error::FormatError(
                        "sidecar completed with uncommitted data after the last checkpoint"
                            .to_string(),
                    ));
                }
                summary.status = done_status_from_payload(&event.payload)?;
                done = true;
            }
            EventType::Cancelled => {
                summary.status = CollectionStatus::Stopped;
                done = true;
            }
            EventType::Error => {
                let code = event
                    .payload
                    .get("code")
                    .and_then(Value::as_str)
                    .unwrap_or("UNKNOWN");
                summary.status = if code == "REQUEST_CANCELLED" {
                    CollectionStatus::Stopped
                } else {
                    CollectionStatus::Failed
                };
                let message = event
                    .payload
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                summary.error = Some(format!("{code}: {message}"));
                done = true;
            }
            EventType::AuthRequired => {
                summary.status = CollectionStatus::Failed;
                summary.error = Some(
                    event
                        .payload
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("auth required")
                        .to_string(),
                );
                done = true;
            }
            EventType::RateLimited => {
                let scope = RateLimitScope::parse_protocol(
                    event
                        .payload
                        .get("scope")
                        .and_then(Value::as_str)
                        .ok_or_else(|| Error::FormatError("rate-limit scope missing".into()))?,
                )?;
                let retry_after_ms = event.payload.get("retry_after_ms").and_then(Value::as_u64);
                summary.rate_limit = Some(RateLimitInfo {
                    scope,
                    retry_after_ms,
                });
                if execution.is_some() {
                    sidecar
                        .kill_and_wait()
                        .map_err(|error| Error::FormatError(error.to_string()))?;
                    summary.status = CollectionStatus::RateLimited;
                    done = true;
                } else {
                    warn!("collection rate limited: stream={stream}");
                }
            }
            EventType::Warning => {
                if let Some(message) = event.payload.get("message").and_then(Value::as_str) {
                    warn!("collection warning: {message}");
                }
            }
            EventType::Started
            | EventType::Progress
            | EventType::Ready
            | EventType::Capabilities => {
                // 握手期与流内开始/进度事件：仅记录。
                debug_skip(&event.event_type);
            }
        }
    }

    Ok(summary)
}

#[derive(Clone, Copy)]
enum ExternalStop<'a> {
    Cancel(&'a AtomicBool),
    Interrupt(&'a AtomicBool),
}

impl ExternalStop<'_> {
    fn triggered(self) -> bool {
        match self {
            Self::Cancel(flag) | Self::Interrupt(flag) => flag.load(Ordering::Acquire),
        }
    }

    fn status(self) -> CollectionStatus {
        match self {
            Self::Cancel(_) => CollectionStatus::Cancelled,
            Self::Interrupt(_) => CollectionStatus::Interrupted,
        }
    }
}

fn poll_persistent_control(
    sidecar: &mut Sidecar,
    execution: &mut Option<&mut PersistentExecution<'_>>,
) -> Result<Option<CollectionStatus>> {
    let Some(persistent) = execution.as_deref_mut() else {
        return Ok(None);
    };
    let request = match persistent.control_rx.try_recv() {
        Ok(request) => request,
        Err(TryRecvError::Empty) => return Ok(None),
        Err(TryRecvError::Disconnected) => return Ok(None),
    };
    let stopped = sidecar.kill_and_wait().map_err(|error| error.to_string());
    let _ = request.ack.send(stopped.clone());
    stopped.map_err(Error::FormatError)?;
    Ok(Some(match request.action {
        ExecutionControl::Pause => CollectionStatus::Paused,
        ExecutionControl::Cancel => CollectionStatus::Cancelled,
        ExecutionControl::Shutdown => CollectionStatus::Shutdown,
    }))
}

async fn controlled_status_from_db(pool: &SqlitePool, job_id: i64) -> Result<CollectionStatus> {
    let job = get_sync_job(pool, job_id)
        .await?
        .ok_or_else(|| Error::InconsistentTask(format!("sync job {job_id} not found")))?;
    Ok(match job.status.as_str() {
        "paused" => CollectionStatus::Paused,
        "cancelled" => CollectionStatus::Cancelled,
        _ => CollectionStatus::Interrupted,
    })
}

/// 从 `sync_checkpoints` 读取游标并还原为命令的 `checkpoint` payload。
///
/// 数据库以 `{"cursor": {...}}` 包裹格式存储（与 entities 测试一致）；
/// 命令 schema 要求 checkpoint 是 cursor 对象本身，故此处提取内层。
async fn load_checkpoint_cursor(pool: &SqlitePool, stream: &str) -> Result<Option<Value>> {
    let Some(checkpoint) = get_sync_checkpoint(pool, stream).await? else {
        return Ok(None);
    };
    let Some(cursor_json) = checkpoint.cursor_json.as_deref() else {
        return Ok(None);
    };
    let value = serde_json::from_str::<Value>(cursor_json)?;
    if let Some(cursor) = value.get("cursor") {
        let mut cursor = cursor.as_object().cloned().ok_or_else(|| {
            Error::FormatError("stored checkpoint cursor is not an object".to_string())
        })?;
        cursor.insert("fetched_count".to_string(), json!(checkpoint.fetched_count));
        Ok(Some(Value::Object(cursor)))
    } else {
        // 兼容只存 cursor 对象本身的旧格式。
        let mut cursor = value.as_object().cloned().ok_or_else(|| {
            Error::FormatError("stored checkpoint cursor is not an object".to_string())
        })?;
        cursor.insert("fetched_count".to_string(), json!(checkpoint.fetched_count));
        Ok(Some(Value::Object(cursor)))
    }
}

/// 解析 `checkpoint` 事件 payload 为数据库 checkpoint。
fn checkpoint_from_payload(
    stream: &str,
    payload: &Value,
    sequence: u64,
) -> Result<SyncCheckpointDto> {
    let cursor = payload
        .get("cursor")
        .and_then(Value::as_object)
        .filter(|cursor| {
            cursor.get("max_id").is_some()
                && cursor.get("max_id_type").and_then(Value::as_i64).is_some()
        })
        .ok_or_else(|| Error::FormatError("invalid checkpoint cursor".to_string()))?;
    let cursor_json = json!({ "cursor": cursor }).to_string();
    let fetched_count = payload
        .get("fetched_count")
        .and_then(Value::as_u64)
        .and_then(|count| i64::try_from(count).ok())
        .ok_or_else(|| Error::FormatError("invalid checkpoint fetched_count".to_string()))?;
    Ok(SyncCheckpointDto {
        stream: stream.to_string(),
        cursor_json: Some(cursor_json),
        fetched_count,
        last_sequence: Some(sequence as i64),
        updated_at: now(),
        job_id: None,
        run_id: None,
        generation: None,
        owner_token: None,
        owner: CheckpointOwner::AdHoc,
    })
}

/// 解析 `user` 事件 payload 为 `User` 模型。
///
/// URL 字段缺失或非法时使用占位，避免因头像地址异常中断整批。
fn user_from_payload(payload: &Value) -> Option<User> {
    let id = get_i64(payload, "id")?;
    Some(User {
        id,
        screen_name: get_str(payload, "screen_name").unwrap_or("").to_string(),
        domain: get_str(payload, "domain").unwrap_or("").to_string(),
        avatar_hd: parse_url_or_placeholder(get_str(payload, "avatar_hd")),
        avatar_large: parse_url_or_placeholder(get_str(payload, "avatar_large")),
        profile_image_url: parse_url_or_placeholder(get_str(payload, "profile_image_url")),
        following: payload
            .get("following")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        follow_me: payload
            .get("follow_me")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

/// 解析 `post` 事件 payload 为 `PostInternal`。
///
/// id/uid 必填；其余字段缺失时用默认值，保证单条坏数据不阻塞整批。
fn post_from_payload(payload: &Value) -> Result<PostInternal> {
    let id =
        get_i64(payload, "id").ok_or_else(|| Error::FormatError("post.id missing".to_string()))?;
    let content_status = get_str(payload, "content_status")
        .filter(|s| *s == "partial" || *s == "complete")
        .unwrap_or("complete");
    let is_long_text = match payload.get("is_long_text") {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0) != 0,
        _ => false,
    };
    Ok(PostInternal {
        id,
        uid: get_i64(payload, "uid"),
        text: get_str(payload, "text").unwrap_or("").to_string(),
        created_at: get_str(payload, "created_at").unwrap_or("").to_string(),
        attitudes_count: get_i64(payload, "attitudes_count"),
        attitudes_status: get_i64(payload, "attitudes_status").unwrap_or(0),
        comments_count: get_i64(payload, "comments_count"),
        reposts_count: get_i64(payload, "reposts_count"),
        repost_type: get_i64(payload, "repost_type"),
        retweeted_id: get_i64(payload, "retweeted_id"),
        pic_num: get_i64(payload, "pic_num"),
        edit_count: get_i64(payload, "edit_count"),
        deleted: payload
            .get("deleted")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        favorited: payload
            .get("favorited")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        mblogid: get_str(payload, "mblogid").unwrap_or("").to_string(),
        geo: get_value(payload, "geo"),
        mix_media_ids: get_value(payload, "mix_media_ids"),
        mix_media_info: get_value(payload, "mix_media_info"),
        page_info: get_value(payload, "page_info"),
        pic_ids: get_value(payload, "pic_ids"),
        pic_infos: get_value(payload, "pic_infos"),
        url_struct: get_value(payload, "url_struct"),
        tag_struct: get_value(payload, "tag_struct"),
        topic_ids: get_value(payload, "topic_ids"),
        at_users: get_value(payload, "at_users"),
        raw_data: get_value(payload, "raw_data"),
        source: get_str(payload, "source").map(str::to_string),
        region_name: get_str(payload, "region_name").map(str::to_string),
        bid: get_str(payload, "bid").map(str::to_string),
        location: get_str(payload, "location").map(str::to_string),
        video_url: get_str(payload, "video_url").map(str::to_string),
        content_status: content_status.to_string(),
        fetch_error: get_str(payload, "fetch_error").map(str::to_string),
        first_fetched_at: get_str(payload, "first_fetched_at").map(str::to_string),
        last_refreshed_at: get_str(payload, "last_refreshed_at").map(str::to_string),
        is_long_text,
    })
}

/// 解析 `comment` 事件 payload 为 `CommentDto`。
fn comment_from_payload(payload: &Value) -> Result<CommentDto> {
    let id = get_i64(payload, "id")
        .ok_or_else(|| Error::FormatError("comment.id missing".to_string()))?;
    let post_id = get_i64(payload, "post_id")
        .ok_or_else(|| Error::FormatError("comment.post_id missing".to_string()))?;
    let content_status = get_str(payload, "content_status")
        .filter(|s| *s == "partial" || *s == "complete")
        .unwrap_or("complete");
    Ok(CommentDto {
        id,
        post_id,
        root_id: get_i64(payload, "root_id"),
        parent_id: get_i64(payload, "parent_id"),
        user_id: get_i64(payload, "user_id"),
        text: get_str(payload, "text").unwrap_or("").to_string(),
        created_at: get_str(payload, "created_at").unwrap_or("").to_string(),
        depth: get_i64(payload, "depth").unwrap_or(0),
        child_count: get_i64(payload, "child_count").unwrap_or(0),
        like_count: get_i64(payload, "like_count").unwrap_or(0),
        source: get_str(payload, "source").map(str::to_string),
        media_json: None,
        raw_data: get_string_value(payload, "raw_data"),
        content_status: content_status.to_string(),
        deleted: payload
            .get("deleted")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        first_fetched_at: get_str(payload, "first_fetched_at").map(str::to_string),
        last_refreshed_at: get_str(payload, "last_refreshed_at").map(str::to_string),
    })
}

/// 解析 `media_reference` 事件 payload 为 `MediaDto`。
fn media_from_payload(payload: &Value) -> Result<MediaDto> {
    let url = get_str(payload, "url")
        .ok_or_else(|| Error::FormatError("media_reference.url missing".to_string()))?;
    Ok(MediaDto {
        id: 0,
        owner_type: get_str(payload, "owner_type").unwrap_or("post").to_string(),
        owner_id: get_i64(payload, "owner_id"),
        media_type: get_str(payload, "media_type")
            .unwrap_or("picture")
            .to_string(),
        url: url.to_string(),
        local_path: None,
        status: "pending".to_string(),
        retry_count: 0,
        last_error: None,
        created_at: now(),
        updated_at: None,
    })
}

/// 当前批次累积的数据，checkpoint 到达时同事务提交。
#[derive(Default)]
struct PendingBatch {
    users: Vec<User>,
    posts: Vec<PostInternal>,
    comments: Vec<CommentDto>,
    media: Vec<MediaDto>,
}

impl PendingBatch {
    fn is_empty(&self) -> bool {
        self.users.is_empty()
            && self.posts.is_empty()
            && self.comments.is_empty()
            && self.media.is_empty()
    }
}

fn now() -> String {
    Utc::now().to_rfc3339()
}

/// 读取可选字符串字段（忽略 null）。
fn get_str<'a>(payload: &'a Value, key: &str) -> Option<&'a str> {
    payload.get(key).and_then(Value::as_str)
}

/// 读取可选整数字段，兼容 JSON 字符串数字（如 `"123"`）。
fn get_i64(payload: &Value, key: &str) -> Option<i64> {
    match payload.get(key) {
        Some(Value::Number(n)) => n.as_i64(),
        Some(Value::String(s)) => s.parse::<i64>().ok(),
        _ => None,
    }
}

/// 读取可选字段为原始 `Value`（忽略 null）。
fn get_value(payload: &Value, key: &str) -> Option<Value> {
    payload.get(key).filter(|v| !v.is_null()).cloned()
}

/// 读取可选字段为字符串（对象序列化为 JSON 字符串）。
fn get_string_value(payload: &Value, key: &str) -> Option<String> {
    match payload.get(key) {
        Some(Value::String(s)) => Some(s.clone()),
        Some(v) if !v.is_null() => Some(v.to_string()),
        _ => None,
    }
}

fn done_status_from_payload(payload: &Value) -> Result<CollectionStatus> {
    match payload.get("status").and_then(Value::as_str) {
        Some("completed") => Ok(CollectionStatus::Completed),
        Some("stopped") => Ok(CollectionStatus::Stopped),
        Some(status) => Err(Error::FormatError(format!(
            "invalid sidecar done status: {status}"
        ))),
        None => Err(Error::FormatError(
            "sidecar done status missing".to_string(),
        )),
    }
}

fn validate_event_sequence(previous: u64, current: u64) -> Result<()> {
    let expected = previous
        .checked_add(1)
        .ok_or_else(|| Error::FormatError("sidecar event sequence overflow".to_string()))?;
    if current != expected {
        return Err(Error::FormatError(format!(
            "sidecar event sequence gap or reordering: expected {expected}, got {current}"
        )));
    }
    Ok(())
}

/// 解析 URL；失败或缺失时用占位地址。
fn parse_url_or_placeholder(raw: Option<&str>) -> url::Url {
    raw.and_then(|s| url::Url::parse(s).ok())
        .unwrap_or_else(|| url::Url::parse("https://example.invalid/").expect("placeholder url"))
}

fn debug_skip(event_type: &EventType) {
    use tracing::debug;
    debug!("collection event ignored: {event_type}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload_with(extra: Value) -> Value {
        let mut base = json!({
            "id": "9001",
            "uid": "10001",
            "text": "你好",
            "content_status": "partial",
            "is_long_text": true,
            "attitudes_count": "3",
            "pic_ids": ["a", "b"],
            "raw_data": {"status": 200}
        });
        if let Some(obj) = base.as_object_mut() {
            for (k, v) in extra.as_object().cloned().unwrap_or_default() {
                obj.insert(k, v);
            }
        }
        base
    }

    #[test]
    fn post_payload_parses_extended_fields() {
        let payload = payload_with(json!({}));
        let post = post_from_payload(&payload).unwrap();
        assert_eq!(post.id, 9001);
        assert_eq!(post.uid, Some(10001));
        assert_eq!(post.text, "你好");
        assert_eq!(post.content_status, "partial");
        assert!(post.is_long_text);
        assert_eq!(post.attitudes_count, Some(3));
        assert_eq!(post.pic_ids, Some(json!(["a", "b"])));
        assert_eq!(post.raw_data, Some(json!({"status": 200})));
    }

    #[test]
    fn post_payload_missing_id_errors() {
        let payload = json!({"uid": "10001"});
        assert!(post_from_payload(&payload).is_err());
    }

    #[test]
    fn post_payload_defaults_when_missing() {
        let payload = json!({"id": "1", "uid": "2"});
        let post = post_from_payload(&payload).unwrap();
        assert_eq!(post.text, "");
        assert_eq!(post.content_status, "complete");
        assert!(!post.is_long_text);
        assert_eq!(post.attitudes_count, None);
    }

    #[test]
    fn post_payload_invalid_content_status_falls_back() {
        let payload = payload_with(json!({"content_status": "weird"}));
        let post = post_from_payload(&payload).unwrap();
        assert_eq!(post.content_status, "complete");
    }

    #[test]
    fn comment_payload_parses() {
        let payload = json!({
            "id": "42",
            "post_id": "9001",
            "root_id": "42",
            "user_id": "10001",
            "text": "二楼",
            "depth": 1,
            "child_count": "3",
            "like_count": "5"
        });
        let comment = comment_from_payload(&payload).unwrap();
        assert_eq!(comment.id, 42);
        assert_eq!(comment.post_id, 9001);
        assert_eq!(comment.depth, 1);
        assert_eq!(comment.child_count, 3);
        assert_eq!(comment.like_count, 5);
        assert_eq!(comment.content_status, "complete");
    }

    #[test]
    fn media_payload_requires_url() {
        assert!(media_from_payload(&json!({"owner_type": "post"})).is_err());
        let media = media_from_payload(&json!({
            "owner_type": "post",
            "owner_id": "9001",
            "media_type": "picture",
            "url": "https://wx1.example.com/a.jpg"
        }))
        .unwrap();
        assert_eq!(media.url, "https://wx1.example.com/a.jpg");
        assert_eq!(media.status, "pending");
        assert_eq!(media.owner_id, Some(9001));
    }

    #[test]
    fn done_status_rejects_missing_and_unknown_values() {
        assert!(done_status_from_payload(&json!({})).is_err());
        assert!(done_status_from_payload(&json!({"status": "unexpected"})).is_err());
        assert_eq!(
            done_status_from_payload(&json!({"status": "completed"})).unwrap(),
            CollectionStatus::Completed
        );
        assert_eq!(
            done_status_from_payload(&json!({"status": "stopped"})).unwrap(),
            CollectionStatus::Stopped
        );
    }

    #[test]
    fn event_sequence_rejects_gaps_duplicates_and_out_of_order_values() {
        assert!(validate_event_sequence(0, 1).is_ok());
        assert!(validate_event_sequence(1, 2).is_ok());
        assert!(validate_event_sequence(0, 2).is_err());
        assert!(validate_event_sequence(2, 2).is_err());
        assert!(validate_event_sequence(2, 1).is_err());
    }

    #[test]
    fn checkpoint_payload_parses_with_cursor_envelope() {
        let payload = json!({
            "cursor": {"max_id": "p1_after", "max_id_type": 0},
            "fetched_count": 20,
            "has_more": true
        });
        let checkpoint = checkpoint_from_payload("user:123:posts", &payload, 20).unwrap();
        assert_eq!(checkpoint.stream, "user:123:posts");
        assert_eq!(checkpoint.fetched_count, 20);
        assert_eq!(checkpoint.last_sequence, Some(20));
        let cursor_json: Value = serde_json::from_str(&checkpoint.cursor_json.unwrap()).unwrap();
        assert_eq!(cursor_json["cursor"]["max_id"], "p1_after");
    }

    #[test]
    fn checkpoint_payload_rejects_missing_required_fields() {
        assert!(checkpoint_from_payload("user:123:posts", &json!({}), 1).is_err());
        assert!(
            checkpoint_from_payload(
                "user:123:posts",
                &json!({
                    "cursor": {"max_id": "next", "max_id_type": 0},
                    "fetched_count": -1
                }),
                1,
            )
            .is_err()
        );
    }

    #[test]
    fn user_payload_parses_with_placeholder_urls() {
        let payload = json!({
            "id": "10001",
            "screen_name": "小明",
            "domain": "xiaoming"
        });
        let user = user_from_payload(&payload).unwrap();
        assert_eq!(user.id, 10001);
        assert_eq!(user.screen_name, "小明");
        assert!(user.avatar_hd.as_str().starts_with("https://"));
    }

    #[test]
    fn checkpoint_storage_is_wrapped_and_command_payload_is_cursor() {
        // 数据库存储格式为包裹格式；命令注入时提取内层 cursor 对象。
        let stored = json!({"cursor": {"max_id": "p1", "max_id_type": 0}});
        let command_checkpoint = stored.get("cursor").cloned().unwrap_or(stored);
        assert_eq!(command_checkpoint["max_id"], "p1");
        assert_eq!(command_checkpoint["max_id_type"], 0);
    }
}
