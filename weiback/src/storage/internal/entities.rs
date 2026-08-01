//! 新实体表的内部 DTO 与 CRUD：评论、媒体队列、监控用户、同步任务、
//! 同步运行、同步 checkpoint、幂等事件。
//!
//! 设计约定：
//! - 所有时间字段以 RFC3339 字符串存储。
//! - `media.url`、`processed_events.event_id`、`sync_checkpoints.stream` 是唯一键，
//!   写入使用 `INSERT OR IGNORE` / `ON CONFLICT` 保证幂等。
//! - 业务数据（评论/媒体）与 checkpoint、幂等事件在同一事务提交
//!   （见 [`transactional::CommitPlan`]），Rust 是唯一写入者。

use sea_query::{Expr, Iden, OnConflict, Query, SqliteQueryBuilder};
use sea_query_binder::SqlxBinder;
use serde::{Deserialize, Serialize};
use sqlx::{Acquire, Executor, FromRow, Sqlite};
use tracing::warn;

use crate::error::Result;

/// 媒体所有者类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaOwnerType {
    Post,
    User,
    Comment,
}

impl MediaOwnerType {
    /// 序列化为数据库字符串。
    pub fn as_str(&self) -> &'static str {
        match self {
            MediaOwnerType::Post => "post",
            MediaOwnerType::User => "user",
            MediaOwnerType::Comment => "comment",
        }
    }
}

/// 媒体类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaType {
    Picture,
    Video,
    Avatar,
    Emoji,
}

impl MediaType {
    /// 序列化为数据库字符串。
    pub fn as_str(&self) -> &'static str {
        match self {
            MediaType::Picture => "picture",
            MediaType::Video => "video",
            MediaType::Avatar => "avatar",
            MediaType::Emoji => "emoji",
        }
    }
}

/// 媒体下载状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaStatus {
    Pending,
    Downloaded,
    Failed,
}

impl MediaStatus {
    /// 序列化为数据库字符串。
    pub fn as_str(&self) -> &'static str {
        match self {
            MediaStatus::Pending => "pending",
            MediaStatus::Downloaded => "downloaded",
            MediaStatus::Failed => "failed",
        }
    }
}

#[derive(Iden)]
#[iden = "comments"]
pub enum CommentIden {
    Table,
    Id,
    PostId,
    RootId,
    ParentId,
    UserId,
    Text,
    CreatedAt,
    Depth,
    ChildCount,
    LikeCount,
    Source,
    MediaJson,
    RawData,
    ContentStatus,
    Deleted,
    FirstFetchedAt,
    LastRefreshedAt,
}

#[derive(Iden)]
#[iden = "media"]
pub enum MediaIden {
    Table,
    Id,
    OwnerType,
    OwnerId,
    MediaType,
    Url,
    LocalPath,
    Status,
    RetryCount,
    LastError,
    CreatedAt,
    UpdatedAt,
}

#[derive(Iden)]
#[iden = "monitored_users"]
pub enum MonitoredUserIden {
    Table,
    AccountId,
    Uid,
    ScreenName,
    RefreshStrategy,
    Enabled,
    LastRefreshedAt,
    CreatedAt,
    UpdatedAt,
    Tier,
    IntervalSecs,
    JitterSecs,
    NextRefreshEpoch,
    LastRefreshEpoch,
}

#[derive(Iden)]
#[iden = "sync_jobs"]
pub enum SyncJobIden {
    Table,
    Id,
    ResourceKey,
    Name,
    Kind,
    PayloadJson,
    Status,
    Priority,
    ScheduleConfig,
    Enabled,
    RecoveryCount,
    MaxRecoveryAttempts,
    AvailableAt,
    AvailableAtEpoch,
    ClaimedAt,
    OwnerToken,
    LeaseUntilEpoch,
    CurrentRunId,
    Generation,
    LastError,
    CreatedAt,
    UpdatedAt,
    AccountId,
    EndpointKey,
    EndpointGateRevision,
    AccountGateRevision,
}

#[derive(Iden)]
#[iden = "sync_runs"]
pub enum SyncRunIden {
    Table,
    Id,
    JobId,
    Status,
    StartedAt,
    FinishedAt,
    StatsJson,
    Error,
    Attempt,
    UpdatedAt,
    OwnerToken,
    Generation,
    LeaseUntilEpoch,
}

#[derive(Iden)]
#[iden = "sync_checkpoints"]
pub enum SyncCheckpointIden {
    Table,
    Stream,
    CursorJson,
    FetchedCount,
    LastSequence,
    UpdatedAt,
    JobId,
    RunId,
    Generation,
    OwnerToken,
}

#[derive(Iden)]
#[iden = "processed_events"]
pub enum ProcessedEventIden {
    Table,
    Id,
    EventId,
    Stream,
    Sequence,
    RequestId,
    ProcessedAt,
}

/// 评论 DTO。
#[derive(Debug, Clone, PartialEq, FromRow, Serialize, Deserialize)]
pub struct CommentDto {
    pub id: i64,
    pub post_id: i64,
    pub root_id: Option<i64>,
    pub parent_id: Option<i64>,
    pub user_id: Option<i64>,
    pub text: String,
    pub created_at: String,
    pub depth: i64,
    pub child_count: i64,
    pub like_count: i64,
    pub source: Option<String>,
    pub media_json: Option<String>,
    pub raw_data: Option<String>,
    pub content_status: String,
    pub deleted: bool,
    pub first_fetched_at: Option<String>,
    pub last_refreshed_at: Option<String>,
}

/// 媒体队列 DTO。
#[derive(Debug, Clone, PartialEq, FromRow, Serialize, Deserialize)]
pub struct MediaDto {
    pub id: i64,
    pub owner_type: String,
    pub owner_id: Option<i64>,
    pub media_type: String,
    pub url: String,
    pub local_path: Option<String>,
    pub status: String,
    pub retry_count: i64,
    pub last_error: Option<String>,
    pub created_at: String,
    pub updated_at: Option<String>,
}

/// 监控用户 DTO。
#[derive(Debug, Clone, PartialEq, FromRow, Serialize, Deserialize)]
pub struct MonitoredUserDto {
    pub account_id: i64,
    pub uid: i64,
    pub screen_name: Option<String>,
    pub refresh_strategy: String,
    pub enabled: bool,
    pub last_refreshed_at: Option<String>,
    pub created_at: String,
    pub updated_at: Option<String>,
    pub tier: RefreshTier,
    pub interval_secs: i64,
    pub jitter_secs: i64,
    pub next_refresh_epoch: i64,
    pub last_refresh_epoch: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "TEXT", rename_all = "snake_case")]
pub enum RefreshTier {
    Hot,
    Warm,
    Cold,
}

impl RefreshTier {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hot => "hot",
            Self::Warm => "warm",
            Self::Cold => "cold",
        }
    }
}

/// Upsert an account reference. Secrets are deliberately outside this table.
pub async fn save_account<'c, A>(acquirer: A, account: &AccountDto) -> Result<i64>
where
    A: Acquire<'c, Database = Sqlite>,
{
    if account.provider.is_empty() || account.uid.is_empty() || account.session_ref.is_empty() {
        return Err(crate::error::Error::FormatError(
            "invalid account reference".into(),
        ));
    }
    let mut conn = acquirer.acquire().await?;
    if account.id > 0 {
        return sqlx::query_scalar(
            "UPDATE accounts SET display_name=?,session_ref=?,enabled=?,updated_at=? \
             WHERE id=? AND provider=? AND uid=? RETURNING id",
        )
        .bind(&account.display_name)
        .bind(&account.session_ref)
        .bind(account.enabled)
        .bind(&account.updated_at)
        .bind(account.id)
        .bind(&account.provider)
        .bind(&account.uid)
        .fetch_optional(&mut *conn)
        .await?
        .ok_or_else(|| {
            crate::error::Error::InconsistentTask(format!(
                "account {} identity does not match persisted account",
                account.id
            ))
        });
    }
    Ok(sqlx::query_scalar(
        "INSERT INTO accounts(provider,uid,display_name,session_ref,enabled,created_at,updated_at) \
         VALUES(?,?,?,?,?,?,?) ON CONFLICT(provider,uid) DO UPDATE SET \
         display_name=excluded.display_name,session_ref=excluded.session_ref,enabled=excluded.enabled,updated_at=excluded.updated_at \
         RETURNING id",
    )
    .bind(&account.provider).bind(&account.uid).bind(&account.display_name)
    .bind(&account.session_ref).bind(account.enabled).bind(&account.created_at)
    .bind(&account.updated_at).fetch_one(&mut *conn).await?)
}

pub async fn get_account<'e, E>(executor: E, id: i64) -> Result<Option<AccountDto>>
where
    E: Executor<'e, Database = Sqlite>,
{
    Ok(sqlx::query_as("SELECT id,provider,uid,display_name,session_ref,enabled,created_at,updated_at FROM accounts WHERE id=?")
        .bind(id).fetch_optional(executor).await?)
}

pub async fn get_accounts<'e, E>(executor: E) -> Result<Vec<AccountDto>>
where
    E: Executor<'e, Database = Sqlite>,
{
    Ok(sqlx::query_as(
        "SELECT id,provider,uid,display_name,session_ref,enabled,created_at,updated_at \
         FROM accounts ORDER BY provider,uid",
    )
    .fetch_all(executor)
    .await?)
}

pub async fn delete_account<'c, A>(acquirer: A, id: i64) -> Result<bool>
where
    A: Acquire<'c, Database = Sqlite>,
{
    let mut conn = acquirer.acquire().await?;
    let dependent: i64 = sqlx::query_scalar(
        "SELECT (SELECT COUNT(*) FROM monitored_users WHERE account_id=?) + \
         (SELECT COUNT(*) FROM sync_jobs WHERE account_id=?)",
    )
    .bind(id)
    .bind(id)
    .fetch_one(&mut *conn)
    .await?;
    if dependent > 0 {
        return Err(crate::error::Error::InconsistentTask(format!(
            "account {id} still owns persisted work"
        )));
    }
    Ok(sqlx::query("DELETE FROM accounts WHERE id=?")
        .bind(id)
        .execute(&mut *conn)
        .await?
        .rows_affected()
        == 1)
}

pub async fn get_rate_limit_gate<'e, E>(
    executor: E,
    account_id: i64,
    endpoint_key: &str,
) -> Result<Option<RateLimitGateDto>>
where
    E: Executor<'e, Database = Sqlite>,
{
    Ok(sqlx::query_as("SELECT account_id,endpoint_key,next_allowed_epoch,backoff_level,retry_after_epoch,updated_at,updated_at_epoch,revision FROM rate_limit_gates WHERE account_id=? AND endpoint_key=?")
        .bind(account_id).bind(endpoint_key).fetch_optional(executor).await?)
}

/// Monotonic gate upsert: a shorter observation never opens an existing gate.
pub async fn set_rate_limit_gate<'c, A>(
    acquirer: A,
    account_id: i64,
    endpoint_key: &str,
    next_allowed_epoch: i64,
    backoff_level: i64,
    retry_after_epoch: Option<i64>,
    updated_at: &str,
) -> Result<()>
where
    A: Acquire<'c, Database = Sqlite>,
{
    let mut conn = acquirer.acquire().await?;
    sqlx::query("INSERT INTO rate_limit_gates(account_id,endpoint_key,next_allowed_epoch,backoff_level,retry_after_epoch,updated_at,updated_at_epoch,revision) VALUES(?,?,?,?,?,?,?,1) \
        ON CONFLICT(account_id,endpoint_key) DO UPDATE SET \
        next_allowed_epoch=MAX(rate_limit_gates.next_allowed_epoch,excluded.next_allowed_epoch), \
        backoff_level=CASE WHEN excluded.next_allowed_epoch >= rate_limit_gates.next_allowed_epoch THEN MAX(rate_limit_gates.backoff_level,excluded.backoff_level) ELSE rate_limit_gates.backoff_level END, \
        retry_after_epoch=CASE WHEN excluded.next_allowed_epoch >= rate_limit_gates.next_allowed_epoch THEN excluded.retry_after_epoch ELSE rate_limit_gates.retry_after_epoch END,updated_at=excluded.updated_at,updated_at_epoch=MAX(rate_limit_gates.updated_at_epoch,excluded.updated_at_epoch),revision=rate_limit_gates.revision+1")
        .bind(account_id).bind(endpoint_key).bind(next_allowed_epoch).bind(backoff_level)
        .bind(retry_after_epoch).bind(updated_at).bind(next_allowed_epoch).execute(&mut *conn).await?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, FromRow, Serialize, Deserialize)]
pub struct AccountDto {
    pub id: i64,
    pub provider: String,
    pub uid: String,
    pub display_name: Option<String>,
    pub session_ref: String,
    pub enabled: bool,
    pub created_at: String,
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, FromRow, Serialize, Deserialize)]
pub struct RateLimitGateDto {
    pub account_id: i64,
    pub endpoint_key: String,
    pub next_allowed_epoch: i64,
    pub backoff_level: i64,
    pub retry_after_epoch: Option<i64>,
    pub updated_at: String,
    pub updated_at_epoch: i64,
    pub revision: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SyncJobSpec {
    CollectUserPosts {
        account_id: i64,
        uid: i64,
        max_pages: Option<u64>,
        priority: i64,
    },
    CollectComments {
        account_id: i64,
        post_id: i64,
        max_pages: Option<u64>,
        priority: i64,
    },
    CollectCommentReplies {
        account_id: i64,
        post_id: i64,
        root_comment_id: i64,
        max_pages: Option<u64>,
        priority: i64,
    },
}

/// 持久同步任务状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncJobStatus {
    Pending,
    Running,
    Paused,
    Interrupted,
    Completed,
    Failed,
    Cancelled,
}

impl SyncJobStatus {
    /// 数据库中的稳定字符串表示。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Interrupted => "interrupted",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

/// A worker claim. Epoch values are UTC Unix seconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimRequest {
    pub owner_token: String,
    pub now_epoch: i64,
    pub lease_until_epoch: i64,
    pub claimed_at: String,
}

/// Ownership proof attached to a checkpoint write.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum CheckpointOwner {
    /// P1 ad-hoc collection has no persistent job; progress is fenced by fetched_count.
    #[default]
    AdHoc,
    /// Persistent collection must still own the job's current run and generation.
    Persistent {
        run_id: i64,
        generation: i64,
        owner_token: String,
    },
}

/// Atomically finishes one run and its owning job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinishRunRequest {
    pub job_id: i64,
    pub run_id: i64,
    pub owner_token: String,
    pub generation: i64,
    pub next_status: SyncJobStatus,
    pub finished_at: String,
    pub stats_json: Option<String>,
    pub error: Option<String>,
}

/// 持久任务控制操作结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobControlResult {
    /// 本调用完成了状态迁移。
    Changed,
    /// 任务已经处于目标状态，重复控制安全幂等。
    AlreadyApplied,
}

/// 同步任务 DTO。
#[derive(Debug, Clone, PartialEq, FromRow, Serialize, Deserialize)]
pub struct SyncJobDto {
    pub id: i64,
    pub resource_key: String,
    pub name: String,
    pub kind: String,
    pub payload_json: Option<String>,
    pub status: String,
    pub priority: i64,
    pub schedule_config: Option<String>,
    pub enabled: bool,
    pub recovery_count: i64,
    pub max_recovery_attempts: i64,
    pub available_at: Option<String>,
    pub available_at_epoch: i64,
    pub claimed_at: Option<String>,
    pub owner_token: Option<String>,
    pub lease_until_epoch: Option<i64>,
    pub current_run_id: Option<i64>,
    pub generation: i64,
    pub last_error: Option<String>,
    pub created_at: String,
    pub updated_at: Option<String>,
    pub account_id: i64,
    pub endpoint_key: String,
    pub endpoint_gate_revision: i64,
    pub account_gate_revision: i64,
}

/// 同步运行 DTO。
#[derive(Debug, Clone, PartialEq, FromRow, Serialize, Deserialize)]
pub struct SyncRunDto {
    pub id: i64,
    pub job_id: i64,
    pub status: String,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub stats_json: Option<String>,
    pub error: Option<String>,
    pub attempt: i64,
    pub updated_at: Option<String>,
    pub owner_token: Option<String>,
    pub generation: i64,
    pub lease_until_epoch: Option<i64>,
}

/// 同步 checkpoint DTO。
#[derive(Debug, Clone, PartialEq, FromRow, Serialize, Deserialize)]
pub struct SyncCheckpointDto {
    pub stream: String,
    pub cursor_json: Option<String>,
    pub fetched_count: i64,
    pub last_sequence: Option<i64>,
    pub updated_at: String,
    pub job_id: Option<i64>,
    pub run_id: Option<i64>,
    pub generation: Option<i64>,
    pub owner_token: Option<String>,
    #[sqlx(skip)]
    #[serde(skip)]
    pub owner: CheckpointOwner,
}

/// 幂等事件 DTO。
#[derive(Debug, Clone, PartialEq, FromRow, Serialize, Deserialize)]
pub struct ProcessedEventDto {
    pub id: i64,
    pub event_id: String,
    pub stream: Option<String>,
    pub sequence: Option<i64>,
    pub request_id: Option<String>,
    pub processed_at: String,
}

/// 保存评论（按 id 幂等 upsert）。
pub async fn save_comment<'c, A>(acquirer: A, comment: &CommentDto) -> Result<()>
where
    A: Acquire<'c, Database = Sqlite>,
{
    let mut conn = acquirer.acquire().await?;
    let (sql, values) = Query::insert()
        .into_table(CommentIden::Table)
        .columns([
            CommentIden::Id,
            CommentIden::PostId,
            CommentIden::RootId,
            CommentIden::ParentId,
            CommentIden::UserId,
            CommentIden::Text,
            CommentIden::CreatedAt,
            CommentIden::Depth,
            CommentIden::ChildCount,
            CommentIden::LikeCount,
            CommentIden::Source,
            CommentIden::MediaJson,
            CommentIden::RawData,
            CommentIden::ContentStatus,
            CommentIden::Deleted,
            CommentIden::FirstFetchedAt,
            CommentIden::LastRefreshedAt,
        ])
        .values([
            comment.id.into(),
            comment.post_id.into(),
            comment.root_id.into(),
            comment.parent_id.into(),
            comment.user_id.into(),
            comment.text.clone().into(),
            comment.created_at.clone().into(),
            comment.depth.into(),
            comment.child_count.into(),
            comment.like_count.into(),
            comment.source.clone().into(),
            comment.media_json.clone().into(),
            comment.raw_data.clone().into(),
            comment.content_status.clone().into(),
            comment.deleted.into(),
            comment.first_fetched_at.clone().into(),
            comment.last_refreshed_at.clone().into(),
        ])?
        .on_conflict(
            OnConflict::column(CommentIden::Id)
                .update_columns([
                    CommentIden::PostId,
                    CommentIden::RootId,
                    CommentIden::ParentId,
                    CommentIden::UserId,
                    CommentIden::Text,
                    CommentIden::CreatedAt,
                    CommentIden::Depth,
                    CommentIden::ChildCount,
                    CommentIden::LikeCount,
                    CommentIden::Source,
                    CommentIden::MediaJson,
                    CommentIden::RawData,
                    CommentIden::ContentStatus,
                    CommentIden::Deleted,
                    CommentIden::FirstFetchedAt,
                    CommentIden::LastRefreshedAt,
                ])
                .to_owned(),
        )
        .build_sqlx(SqliteQueryBuilder);
    sqlx::query_with(&sql, values).execute(&mut *conn).await?;
    Ok(())
}

/// 按 id 读取评论。
pub async fn get_comment<'e, E>(executor: E, id: i64) -> Result<Option<CommentDto>>
where
    E: Executor<'e, Database = Sqlite>,
{
    let (sql, values) = Query::select()
        .columns([
            CommentIden::Id,
            CommentIden::PostId,
            CommentIden::RootId,
            CommentIden::ParentId,
            CommentIden::UserId,
            CommentIden::Text,
            CommentIden::CreatedAt,
            CommentIden::Depth,
            CommentIden::ChildCount,
            CommentIden::LikeCount,
            CommentIden::Source,
            CommentIden::MediaJson,
            CommentIden::RawData,
            CommentIden::ContentStatus,
            CommentIden::Deleted,
            CommentIden::FirstFetchedAt,
            CommentIden::LastRefreshedAt,
        ])
        .from(CommentIden::Table)
        .and_where(Expr::col(CommentIden::Id).eq(id))
        .build_sqlx(SqliteQueryBuilder);
    Ok(sqlx::query_as_with::<Sqlite, CommentDto, _>(&sql, values)
        .fetch_optional(executor)
        .await?)
}

/// 按 post_id 读取评论（二级回复一并返回，由调用方按 depth 组装树）。
pub async fn get_comments_by_post<'e, E>(executor: E, post_id: i64) -> Result<Vec<CommentDto>>
where
    E: Executor<'e, Database = Sqlite>,
{
    let (sql, values) = Query::select()
        .columns([
            CommentIden::Id,
            CommentIden::PostId,
            CommentIden::RootId,
            CommentIden::ParentId,
            CommentIden::UserId,
            CommentIden::Text,
            CommentIden::CreatedAt,
            CommentIden::Depth,
            CommentIden::ChildCount,
            CommentIden::LikeCount,
            CommentIden::Source,
            CommentIden::MediaJson,
            CommentIden::RawData,
            CommentIden::ContentStatus,
            CommentIden::Deleted,
            CommentIden::FirstFetchedAt,
            CommentIden::LastRefreshedAt,
        ])
        .from(CommentIden::Table)
        .and_where(Expr::col(CommentIden::PostId).eq(post_id))
        .order_by(CommentIden::Id, sea_query::Order::Asc)
        .build_sqlx(SqliteQueryBuilder);
    Ok(sqlx::query_as_with::<Sqlite, CommentDto, _>(&sql, values)
        .fetch_all(executor)
        .await?)
}

/// 保存媒体队列项（按 url 幂等 upsert）。
pub async fn save_media<'c, A>(acquirer: A, media: &MediaDto) -> Result<()>
where
    A: Acquire<'c, Database = Sqlite>,
{
    let mut conn = acquirer.acquire().await?;
    let (sql, values) = Query::insert()
        .into_table(MediaIden::Table)
        .columns([
            MediaIden::OwnerType,
            MediaIden::OwnerId,
            MediaIden::MediaType,
            MediaIden::Url,
            MediaIden::LocalPath,
            MediaIden::Status,
            MediaIden::RetryCount,
            MediaIden::LastError,
            MediaIden::CreatedAt,
            MediaIden::UpdatedAt,
        ])
        .values([
            media.owner_type.clone().into(),
            media.owner_id.into(),
            media.media_type.clone().into(),
            media.url.clone().into(),
            media.local_path.clone().into(),
            media.status.clone().into(),
            media.retry_count.into(),
            media.last_error.clone().into(),
            media.created_at.clone().into(),
            media.updated_at.clone().into(),
        ])?
        .on_conflict(
            OnConflict::column(MediaIden::Url)
                .update_columns([
                    MediaIden::OwnerType,
                    MediaIden::OwnerId,
                    MediaIden::MediaType,
                    MediaIden::LocalPath,
                    MediaIden::Status,
                    MediaIden::RetryCount,
                    MediaIden::LastError,
                    MediaIden::UpdatedAt,
                ])
                .to_owned(),
        )
        .build_sqlx(SqliteQueryBuilder);
    sqlx::query_with(&sql, values).execute(&mut *conn).await?;
    Ok(())
}

/// Saves a collected media reference without resetting downloader-owned state.
pub async fn save_media_reference<'c, A>(acquirer: A, media: &MediaDto) -> Result<()>
where
    A: Acquire<'c, Database = Sqlite>,
{
    let mut conn = acquirer.acquire().await?;
    let (sql, values) = Query::insert()
        .into_table(MediaIden::Table)
        .columns([
            MediaIden::OwnerType,
            MediaIden::OwnerId,
            MediaIden::MediaType,
            MediaIden::Url,
            MediaIden::LocalPath,
            MediaIden::Status,
            MediaIden::RetryCount,
            MediaIden::LastError,
            MediaIden::CreatedAt,
            MediaIden::UpdatedAt,
        ])
        .values([
            media.owner_type.clone().into(),
            media.owner_id.into(),
            media.media_type.clone().into(),
            media.url.clone().into(),
            media.local_path.clone().into(),
            media.status.clone().into(),
            media.retry_count.into(),
            media.last_error.clone().into(),
            media.created_at.clone().into(),
            media.updated_at.clone().into(),
        ])?
        .on_conflict(OnConflict::column(MediaIden::Url).do_nothing().to_owned())
        .build_sqlx(SqliteQueryBuilder);
    sqlx::query_with(&sql, values).execute(&mut *conn).await?;
    Ok(())
}

/// 按 url 读取媒体项。
pub async fn get_media_by_url<'e, E>(executor: E, url: &str) -> Result<Option<MediaDto>>
where
    E: Executor<'e, Database = Sqlite>,
{
    let (sql, values) = Query::select()
        .columns([
            MediaIden::Id,
            MediaIden::OwnerType,
            MediaIden::OwnerId,
            MediaIden::MediaType,
            MediaIden::Url,
            MediaIden::LocalPath,
            MediaIden::Status,
            MediaIden::RetryCount,
            MediaIden::LastError,
            MediaIden::CreatedAt,
            MediaIden::UpdatedAt,
        ])
        .from(MediaIden::Table)
        .and_where(Expr::col(MediaIden::Url).eq(url))
        .build_sqlx(SqliteQueryBuilder);
    Ok(sqlx::query_as_with::<Sqlite, MediaDto, _>(&sql, values)
        .fetch_optional(executor)
        .await?)
}

/// 保存监控用户（按 uid 幂等 upsert）。
pub async fn save_monitored_user<'c, A>(acquirer: A, user: &MonitoredUserDto) -> Result<()>
where
    A: Acquire<'c, Database = Sqlite>,
{
    let mut conn = acquirer.acquire().await?;
    let (sql, values) = Query::insert()
        .into_table(MonitoredUserIden::Table)
        .columns([
            MonitoredUserIden::AccountId,
            MonitoredUserIden::Uid,
            MonitoredUserIden::ScreenName,
            MonitoredUserIden::RefreshStrategy,
            MonitoredUserIden::Enabled,
            MonitoredUserIden::LastRefreshedAt,
            MonitoredUserIden::CreatedAt,
            MonitoredUserIden::UpdatedAt,
            MonitoredUserIden::Tier,
            MonitoredUserIden::IntervalSecs,
            MonitoredUserIden::JitterSecs,
            MonitoredUserIden::NextRefreshEpoch,
            MonitoredUserIden::LastRefreshEpoch,
        ])
        .values([
            user.account_id.into(),
            user.uid.into(),
            user.screen_name.clone().into(),
            user.refresh_strategy.clone().into(),
            user.enabled.into(),
            user.last_refreshed_at.clone().into(),
            user.created_at.clone().into(),
            user.updated_at.clone().into(),
            user.tier.as_str().into(),
            user.interval_secs.into(),
            user.jitter_secs.into(),
            user.next_refresh_epoch.into(),
            user.last_refresh_epoch.into(),
        ])?
        .on_conflict(
            OnConflict::columns([MonitoredUserIden::AccountId, MonitoredUserIden::Uid])
                .update_columns([
                    MonitoredUserIden::ScreenName,
                    MonitoredUserIden::RefreshStrategy,
                    MonitoredUserIden::Enabled,
                    MonitoredUserIden::UpdatedAt,
                    MonitoredUserIden::Tier,
                    MonitoredUserIden::IntervalSecs,
                    MonitoredUserIden::JitterSecs,
                ])
                .to_owned(),
        )
        .build_sqlx(SqliteQueryBuilder);
    sqlx::query_with(&sql, values).execute(&mut *conn).await?;
    Ok(())
}

/// 读取启用的监控用户。
pub async fn get_enabled_monitored_users<'e, E>(executor: E) -> Result<Vec<MonitoredUserDto>>
where
    E: Executor<'e, Database = Sqlite>,
{
    let (sql, values) = Query::select()
        .columns([
            MonitoredUserIden::AccountId,
            MonitoredUserIden::Uid,
            MonitoredUserIden::ScreenName,
            MonitoredUserIden::RefreshStrategy,
            MonitoredUserIden::Enabled,
            MonitoredUserIden::LastRefreshedAt,
            MonitoredUserIden::CreatedAt,
            MonitoredUserIden::UpdatedAt,
            MonitoredUserIden::Tier,
            MonitoredUserIden::IntervalSecs,
            MonitoredUserIden::JitterSecs,
            MonitoredUserIden::NextRefreshEpoch,
            MonitoredUserIden::LastRefreshEpoch,
        ])
        .from(MonitoredUserIden::Table)
        .and_where(Expr::col(MonitoredUserIden::Enabled).eq(true))
        .order_by(MonitoredUserIden::Uid, sea_query::Order::Asc)
        .build_sqlx(SqliteQueryBuilder);
    Ok(
        sqlx::query_as_with::<Sqlite, MonitoredUserDto, _>(&sql, values)
            .fetch_all(executor)
            .await?,
    )
}

/// 读取全部监控用户，包括暂停的配置项。
pub async fn get_monitored_users<'e, E>(executor: E) -> Result<Vec<MonitoredUserDto>>
where
    E: Executor<'e, Database = Sqlite>,
{
    Ok(sqlx::query_as(
        "SELECT account_id,uid,screen_name,refresh_strategy,enabled,last_refreshed_at,created_at,updated_at, \
         tier,interval_secs,jitter_secs,next_refresh_epoch,last_refresh_epoch FROM monitored_users \
         ORDER BY account_id,uid",
    )
    .fetch_all(executor)
    .await?)
}

/// 删除一个账号下的监控用户配置。
pub async fn delete_monitored_user<'c, A>(acquirer: A, account_id: i64, uid: i64) -> Result<bool>
where
    A: Acquire<'c, Database = Sqlite>,
{
    let mut conn = acquirer.acquire().await?;
    Ok(
        sqlx::query("DELETE FROM monitored_users WHERE account_id=? AND uid=?")
            .bind(account_id)
            .bind(uid)
            .execute(&mut *conn)
            .await?
            .rows_affected()
            == 1,
    )
}

fn sync_job_columns() -> [SyncJobIden; 25] {
    [
        SyncJobIden::Id,
        SyncJobIden::ResourceKey,
        SyncJobIden::Name,
        SyncJobIden::Kind,
        SyncJobIden::PayloadJson,
        SyncJobIden::Status,
        SyncJobIden::Priority,
        SyncJobIden::ScheduleConfig,
        SyncJobIden::Enabled,
        SyncJobIden::RecoveryCount,
        SyncJobIden::MaxRecoveryAttempts,
        SyncJobIden::AvailableAt,
        SyncJobIden::AvailableAtEpoch,
        SyncJobIden::ClaimedAt,
        SyncJobIden::OwnerToken,
        SyncJobIden::LeaseUntilEpoch,
        SyncJobIden::CurrentRunId,
        SyncJobIden::Generation,
        SyncJobIden::LastError,
        SyncJobIden::CreatedAt,
        SyncJobIden::UpdatedAt,
        SyncJobIden::AccountId,
        SyncJobIden::EndpointKey,
        SyncJobIden::EndpointGateRevision,
        SyncJobIden::AccountGateRevision,
    ]
}

/// 入队或更新同一资源的 active 任务，并返回事实行 id。
///
/// partial unique index 使并发 enqueue 也只能得到同一 active 行；终态不在
/// 索引范围内，因此资源之后可以再次入队。
async fn enqueue_sync_job_on_conn(
    conn: &mut sqlx::SqliteConnection,
    job: &SyncJobDto,
) -> Result<i64> {
    if job.resource_key.is_empty() || job.max_recovery_attempts < 0 || job.available_at_epoch < 0 {
        return Err(crate::error::Error::FormatError(
            "invalid sync job queue fields".to_string(),
        ));
    }
    let id = sqlx::query_scalar(
        "INSERT INTO sync_jobs \
         (resource_key,name,kind,payload_json,status,priority,schedule_config,enabled,recovery_count,pre_run_recovery_count, \
          max_recovery_attempts,available_at,available_at_epoch,created_at,updated_at,account_id,endpoint_key) \
         VALUES(?,?,?,?,'pending',?,?,?,0,0,?,?,?,?,?,?,?) \
         ON CONFLICT(resource_key) WHERE status IN ('pending','running','paused','interrupted') \
         DO UPDATE SET \
          name=CASE WHEN sync_jobs.status='pending' THEN excluded.name ELSE sync_jobs.name END, \
          kind=CASE WHEN sync_jobs.status='pending' THEN excluded.kind ELSE sync_jobs.kind END, \
          payload_json=CASE WHEN sync_jobs.status='pending' THEN excluded.payload_json ELSE sync_jobs.payload_json END, \
          priority=CASE WHEN sync_jobs.status='pending' THEN excluded.priority ELSE sync_jobs.priority END, \
          schedule_config=CASE WHEN sync_jobs.status='pending' THEN excluded.schedule_config ELSE sync_jobs.schedule_config END, \
          enabled=CASE WHEN sync_jobs.status='pending' THEN excluded.enabled ELSE sync_jobs.enabled END, \
          max_recovery_attempts=CASE WHEN sync_jobs.status='pending' THEN excluded.max_recovery_attempts ELSE sync_jobs.max_recovery_attempts END, \
          available_at=CASE WHEN sync_jobs.status='pending' THEN excluded.available_at ELSE sync_jobs.available_at END, \
          available_at_epoch=CASE WHEN sync_jobs.status='pending' THEN excluded.available_at_epoch ELSE sync_jobs.available_at_epoch END, \
          updated_at=CASE WHEN sync_jobs.status='pending' THEN excluded.updated_at ELSE sync_jobs.updated_at END \
         RETURNING id",
    )
    .bind(&job.resource_key)
    .bind(&job.name)
    .bind(&job.kind)
    .bind(&job.payload_json)
    .bind(job.priority)
    .bind(&job.schedule_config)
    .bind(job.enabled)
    .bind(job.max_recovery_attempts)
    .bind(&job.available_at)
    .bind(job.available_at_epoch)
    .bind(&job.created_at)
    .bind(&job.updated_at)
    .bind(job.account_id)
    .bind(&job.endpoint_key)
    .fetch_one(&mut *conn)
    .await?;
    Ok(id)
}

#[cfg(debug_assertions)]
#[doc(hidden)]
pub async fn enqueue_test_sync_job(pool: &sqlx::SqlitePool, job: &SyncJobDto) -> Result<i64> {
    let mut conn = pool.acquire().await?;
    sqlx::query(
        "INSERT INTO accounts(id,provider,uid,display_name,session_ref,enabled,created_at) \
         VALUES(1,'test-fixture','1','test fixture','sessions/test.json',1,?) \
         ON CONFLICT(id) DO UPDATE SET enabled=1",
    )
    .bind(&job.created_at)
    .execute(&mut *conn)
    .await?;
    enqueue_sync_job_on_conn(&mut conn, job).await
}

fn canonical_job(spec: &SyncJobSpec, available_at_epoch: i64, created_at: &str) -> SyncJobDto {
    let (account_id, kind, endpoint, resource, payload, priority) = match spec {
        SyncJobSpec::CollectUserPosts {
            account_id,
            uid,
            max_pages,
            priority,
        } => (
            *account_id,
            "collect_user_posts",
            "collect_user_posts",
            format!("account:{account_id}:user:{uid}:posts"),
            serde_json::json!({"uid":uid,"max_pages":max_pages}),
            *priority,
        ),
        SyncJobSpec::CollectComments {
            account_id,
            post_id,
            max_pages,
            priority,
        } => (
            *account_id,
            "collect_comments",
            "collect_comments",
            format!("account:{account_id}:post:{post_id}:comments"),
            serde_json::json!({"post_id":post_id,"max_pages":max_pages}),
            *priority,
        ),
        SyncJobSpec::CollectCommentReplies {
            account_id,
            post_id,
            root_comment_id,
            max_pages,
            priority,
        } => (
            *account_id,
            "collect_comment_replies",
            "collect_comment_replies",
            format!("account:{account_id}:post:{post_id}:comment:{root_comment_id}:replies"),
            serde_json::json!({
                "post_id": post_id,
                "root_comment_id": root_comment_id,
                "max_pages": max_pages
            }),
            *priority,
        ),
    };
    SyncJobDto {
        id: 0,
        resource_key: resource,
        name: kind.into(),
        kind: kind.into(),
        payload_json: Some(payload.to_string()),
        status: "pending".into(),
        priority,
        schedule_config: None,
        enabled: true,
        recovery_count: 0,
        max_recovery_attempts: 3,
        available_at: None,
        available_at_epoch,
        claimed_at: None,
        owner_token: None,
        lease_until_epoch: None,
        current_run_id: None,
        generation: 0,
        last_error: None,
        created_at: created_at.into(),
        updated_at: None,
        account_id,
        endpoint_key: endpoint.into(),
        endpoint_gate_revision: 0,
        account_gate_revision: 0,
    }
}

pub async fn enqueue_sync_job_spec<'c, A>(
    acquirer: A,
    spec: &SyncJobSpec,
    available_at_epoch: i64,
    created_at: &str,
) -> Result<i64>
where
    A: Acquire<'c, Database = Sqlite>,
{
    enqueue_validated_sync_job(
        acquirer,
        &canonical_job(spec, available_at_epoch, created_at),
    )
    .await
}

pub(crate) async fn enqueue_sync_job_spec_on_conn(
    conn: &mut sqlx::SqliteConnection,
    spec: &SyncJobSpec,
    available_at_epoch: i64,
    created_at: &str,
) -> Result<i64> {
    let job = canonical_job(spec, available_at_epoch, created_at);
    validate_sync_job(&job)?;
    ensure_enabled_account(conn, job.account_id).await?;
    enqueue_sync_job_on_conn(conn, &job).await
}

async fn enqueue_validated_sync_job<'c, A>(acquirer: A, job: &SyncJobDto) -> Result<i64>
where
    A: Acquire<'c, Database = Sqlite>,
{
    validate_sync_job(job)?;
    let mut conn = acquirer.acquire().await?;
    ensure_enabled_account(&mut conn, job.account_id).await?;
    enqueue_sync_job_on_conn(&mut conn, job).await
}

fn validate_sync_job(job: &SyncJobDto) -> Result<()> {
    let payload: serde_json::Value = serde_json::from_str(
        job.payload_json
            .as_deref()
            .ok_or_else(|| crate::error::Error::FormatError("missing payload".into()))?,
    )?;
    if payload
        .get("max_pages")
        .is_some_and(|value| !value.is_null() && value.as_u64().is_none())
    {
        return Err(crate::error::Error::FormatError(
            "max_pages must be an unsigned integer".into(),
        ));
    }
    let expected = match job.kind.as_str() {
        "collect_user_posts" => payload
            .get("uid")
            .and_then(serde_json::Value::as_i64)
            .map(|id| {
                (
                    "collect_user_posts",
                    format!("account:{}:user:{id}:posts", job.account_id),
                )
            }),
        "collect_comments" => payload
            .get("post_id")
            .and_then(serde_json::Value::as_i64)
            .map(|id| {
                (
                    "collect_comments",
                    format!("account:{}:post:{id}:comments", job.account_id),
                )
            }),
        "collect_comment_replies" => payload
            .get("post_id")
            .and_then(serde_json::Value::as_i64)
            .zip(
                payload
                    .get("root_comment_id")
                    .and_then(serde_json::Value::as_i64),
            )
            .map(|(post_id, root_comment_id)| {
                (
                    "collect_comment_replies",
                    format!(
                        "account:{}:post:{post_id}:comment:{root_comment_id}:replies",
                        job.account_id
                    ),
                )
            }),
        _ => None,
    }
    .ok_or_else(|| {
        crate::error::Error::FormatError(
            "unknown job kind or missing required payload field".into(),
        )
    })?;
    if job.account_id <= 0 || job.endpoint_key != expected.0 || job.resource_key != expected.1 {
        return Err(crate::error::Error::FormatError(
            "non-canonical sync job".into(),
        ));
    }
    Ok(())
}

async fn ensure_enabled_account(conn: &mut sqlx::SqliteConnection, account_id: i64) -> Result<()> {
    let enabled: bool = sqlx::query_scalar("SELECT enabled FROM accounts WHERE id=?")
        .bind(account_id)
        .fetch_optional(&mut *conn)
        .await?
        .ok_or_else(|| crate::error::Error::FormatError("account does not exist".into()))?;
    if !enabled {
        return Err(crate::error::Error::FormatError(
            "account is disabled".into(),
        ));
    }
    Ok(())
}

/// 兼容旧调用名；语义已升级为持久队列 enqueue/upsert。
#[cfg(debug_assertions)]
#[doc(hidden)]
pub async fn save_sync_job<'c, A>(acquirer: A, job: &SyncJobDto) -> Result<i64>
where
    A: Acquire<'c, Database = Sqlite>,
{
    enqueue_validated_sync_job(acquirer, job).await
}

/// 按 id 读取同步任务。
pub async fn get_sync_job<'e, E>(executor: E, id: i64) -> Result<Option<SyncJobDto>>
where
    E: Executor<'e, Database = Sqlite>,
{
    let (sql, values) = Query::select()
        .columns(sync_job_columns())
        .from(SyncJobIden::Table)
        .and_where(Expr::col(SyncJobIden::Id).eq(id))
        .build_sqlx(SqliteQueryBuilder);
    Ok(sqlx::query_as_with::<Sqlite, SyncJobDto, _>(&sql, values)
        .fetch_optional(executor)
        .await?)
}

/// 读取全部同步任务。
pub async fn get_sync_jobs<'e, E>(executor: E) -> Result<Vec<SyncJobDto>>
where
    E: Executor<'e, Database = Sqlite>,
{
    let (sql, values) = Query::select()
        .columns(sync_job_columns())
        .from(SyncJobIden::Table)
        .order_by(SyncJobIden::Priority, sea_query::Order::Desc)
        .build_sqlx(SqliteQueryBuilder);
    Ok(sqlx::query_as_with::<Sqlite, SyncJobDto, _>(&sql, values)
        .fetch_all(executor)
        .await?)
}

/// 原子 claim 一个当前可执行任务。条件 UPDATE + RETURNING 保证并发单赢家。
pub async fn claim_next_sync_job(
    pool: &sqlx::SqlitePool,
    claim: &ClaimRequest,
) -> Result<Option<SyncJobDto>> {
    claim_next_sync_job_with_gates(pool, claim, 0).await
}

/// Claim the highest-priority runnable job while honoring durable account and endpoint gates.
/// The selected endpoint is reserved in the same transaction.
pub async fn claim_next_sync_job_with_gates(
    pool: &sqlx::SqlitePool,
    claim: &ClaimRequest,
    minimum_interval_secs: i64,
) -> Result<Option<SyncJobDto>> {
    if claim.owner_token.is_empty()
        || claim.now_epoch < 0
        || claim.lease_until_epoch <= claim.now_epoch
    {
        return Err(crate::error::Error::FormatError(
            "invalid sync job claim".to_string(),
        ));
    }
    let claim = claim.clone();
    crate::sqlite_write::with_immediate_transaction(pool, |conn| Box::pin(async move {
    recover_interrupted_sync_jobs_on_conn(conn, claim.now_epoch, &claim.claimed_at).await?;
    let candidate: Option<i64> = sqlx::query_scalar(
        "SELECT j.id FROM sync_jobs j WHERE j.status='pending' AND j.enabled=1 \
         AND EXISTS(SELECT 1 FROM accounts a WHERE a.id=j.account_id AND a.enabled=1) \
         AND j.available_at_epoch<=? AND NOT EXISTS(SELECT 1 FROM rate_limit_gates g \
         WHERE g.account_id=j.account_id AND g.endpoint_key IN ('__account__',j.endpoint_key) \
         AND g.next_allowed_epoch>?) ORDER BY j.priority DESC,j.id ASC LIMIT 1",
    )
    .bind(claim.now_epoch)
    .bind(claim.now_epoch)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(candidate) = candidate else {
        return Ok(None);
    };
    let claimed = sqlx::query_as::<_, SyncJobDto>(
        "UPDATE sync_jobs SET status = 'running', claimed_at = ?, updated_at = ?, \
         owner_token = ?, lease_until_epoch = ?, generation = generation + 1, \
         endpoint_gate_revision=COALESCE((SELECT revision FROM rate_limit_gates \
         WHERE account_id=sync_jobs.account_id AND endpoint_key=sync_jobs.endpoint_key),0), \
         account_gate_revision=COALESCE((SELECT revision FROM rate_limit_gates \
         WHERE account_id=sync_jobs.account_id AND endpoint_key='__account__'),0) \
         WHERE id = ? \
         AND status = 'pending' RETURNING id, resource_key, name, kind, payload_json, status, priority, \
         schedule_config, enabled, recovery_count, max_recovery_attempts, available_at, available_at_epoch, \
         claimed_at, owner_token, lease_until_epoch, current_run_id, generation, last_error, created_at, updated_at, \
         account_id, endpoint_key, endpoint_gate_revision, account_gate_revision",
    )
    .bind(&claim.claimed_at)
    .bind(&claim.claimed_at)
    .bind(&claim.owner_token)
    .bind(claim.lease_until_epoch)
    .bind(candidate)
    .fetch_optional(&mut *conn)
    .await?;
    if minimum_interval_secs > 0
        && let Some(job) = &claimed
    {
        let reserved_until = claim.now_epoch.saturating_add(minimum_interval_secs.max(0));
        crate::rate_limit::apply_gate_on_conn(
            conn,
            &crate::rate_limit::GateUpdate {
                account_id: job.account_id,
                endpoint_key: &job.endpoint_key,
                next_allowed_epoch: reserved_until,
                backoff_level: 0,
                retry_after_epoch: None,
                updated_at: &claim.claimed_at,
                updated_at_epoch: claim.now_epoch,
            },
        )
        .await?;
    }
    Ok(claimed)
    })).await
}

/// 仅当当前状态等于 expected 时迁移，并返回是否由本调用完成。
pub async fn transition_sync_job<'c, A>(
    acquirer: A,
    job_id: i64,
    expected: SyncJobStatus,
    next: SyncJobStatus,
    updated_at: &str,
    error: Option<&str>,
) -> Result<bool>
where
    A: Acquire<'c, Database = Sqlite>,
{
    if expected == SyncJobStatus::Running || next == SyncJobStatus::Running {
        return Err(crate::error::Error::FormatError(
            "running transitions require queue ownership CAS".to_string(),
        ));
    }
    let mut conn = acquirer.acquire().await?;
    let (sql, values) = Query::update()
        .table(SyncJobIden::Table)
        .value(SyncJobIden::Status, next.as_str())
        .value(SyncJobIden::LastError, error)
        .value(SyncJobIden::UpdatedAt, updated_at)
        .and_where(Expr::col(SyncJobIden::Id).eq(job_id))
        .and_where(Expr::col(SyncJobIden::Status).eq(expected.as_str()))
        .build_sqlx(SqliteQueryBuilder);
    let result = sqlx::query_with(&sql, values).execute(&mut *conn).await?;
    Ok(result.rows_affected() == 1)
}

async fn current_sync_job_status(
    conn: &mut sqlx::SqliteConnection,
    job_id: i64,
) -> Result<SyncJobStatus> {
    let status: Option<String> = sqlx::query_scalar("SELECT status FROM sync_jobs WHERE id = ?")
        .bind(job_id)
        .fetch_optional(conn)
        .await?;
    let status = status.ok_or_else(|| {
        crate::error::Error::InconsistentTask(format!("sync job {job_id} not found"))
    })?;
    match status.as_str() {
        "pending" => Ok(SyncJobStatus::Pending),
        "running" => Ok(SyncJobStatus::Running),
        "paused" => Ok(SyncJobStatus::Paused),
        "interrupted" => Ok(SyncJobStatus::Interrupted),
        "completed" => Ok(SyncJobStatus::Completed),
        "failed" => Ok(SyncJobStatus::Failed),
        "cancelled" => Ok(SyncJobStatus::Cancelled),
        _ => Err(crate::error::Error::InconsistentTask(format!(
            "sync job {job_id} has invalid status {status}"
        ))),
    }
}

async fn finish_running_job_for_control(
    tx: &mut sqlx::Transaction<'_, Sqlite>,
    job_id: i64,
    target: SyncJobStatus,
    updated_at: &str,
) -> Result<bool> {
    let current_run_id: Option<i64> =
        sqlx::query_scalar("SELECT current_run_id FROM sync_jobs WHERE id=? AND status='running'")
            .bind(job_id)
            .fetch_optional(&mut **tx)
            .await?
            .flatten();
    if current_run_id.is_none() {
        let job = sqlx::query(
            "UPDATE sync_jobs SET status=?,generation=generation+1,owner_token=NULL, \
             current_run_id=NULL,lease_until_epoch=NULL,updated_at=? \
             WHERE id=? AND status='running' AND current_run_id IS NULL",
        )
        .bind(target.as_str())
        .bind(updated_at)
        .bind(job_id)
        .execute(&mut **tx)
        .await?;
        return Ok(job.rows_affected() == 1);
    }
    let run = sqlx::query(
        "UPDATE sync_runs SET status = ?, finished_at = ?, updated_at = ?, \
         error = COALESCE(error, 'stopped by user control') WHERE id = \
         (SELECT current_run_id FROM sync_jobs WHERE id = ? AND status = 'running') \
         AND status = 'running'",
    )
    .bind(target.as_str())
    .bind(updated_at)
    .bind(updated_at)
    .bind(job_id)
    .execute(&mut **tx)
    .await?;
    if run.rows_affected() != 1 {
        return Ok(false);
    }
    let job = sqlx::query(
        "UPDATE sync_jobs SET status = ?, generation = generation + 1, owner_token = NULL, \
         current_run_id = NULL, lease_until_epoch = NULL, updated_at = ? \
         WHERE id = ? AND status = 'running'",
    )
    .bind(target.as_str())
    .bind(updated_at)
    .bind(job_id)
    .execute(&mut **tx)
    .await?;
    Ok(job.rows_affected() == 1)
}

/// 暂停 pending/running 任务；running 控制同时 fence owner 并结束 run。
pub async fn pause_sync_job<'c, A>(
    acquirer: A,
    job_id: i64,
    updated_at: &str,
) -> Result<JobControlResult>
where
    A: Acquire<'c, Database = Sqlite>,
{
    let mut conn = acquirer.acquire().await?;
    let mut tx = conn.begin().await?;
    let status = current_sync_job_status(&mut tx, job_id).await?;
    let changed = match status {
        SyncJobStatus::Paused => {
            tx.commit().await?;
            return Ok(JobControlResult::AlreadyApplied);
        }
        SyncJobStatus::Pending | SyncJobStatus::Interrupted => {
            sqlx::query(
                "UPDATE sync_jobs SET status='paused', updated_at=? WHERE id=? AND status=?",
            )
            .bind(updated_at)
            .bind(job_id)
            .bind(status.as_str())
            .execute(&mut *tx)
            .await?
            .rows_affected()
                == 1
        }
        SyncJobStatus::Running => {
            finish_running_job_for_control(&mut tx, job_id, SyncJobStatus::Paused, updated_at)
                .await?
        }
        _ => {
            return Err(crate::error::Error::InconsistentTask(format!(
                "cannot pause sync job {job_id} from {}",
                status.as_str()
            )));
        }
    };
    if !changed {
        tx.rollback().await?;
        return Err(crate::error::Error::InconsistentTask(format!(
            "sync job {job_id} changed during pause"
        )));
    }
    tx.commit().await?;
    Ok(JobControlResult::Changed)
}

/// 恢复 paused/interrupted 任务，仅迁移回 pending，不直接执行。
pub async fn resume_sync_job<'c, A>(
    acquirer: A,
    job_id: i64,
    updated_at: &str,
) -> Result<JobControlResult>
where
    A: Acquire<'c, Database = Sqlite>,
{
    let mut conn = acquirer.acquire().await?;
    let status = current_sync_job_status(&mut conn, job_id).await?;
    if status == SyncJobStatus::Pending {
        let enabled: bool = sqlx::query_scalar("SELECT enabled FROM sync_jobs WHERE id=?")
            .bind(job_id)
            .fetch_one(&mut *conn)
            .await?;
        if enabled {
            return Ok(JobControlResult::AlreadyApplied);
        }
        let restored = sqlx::query(
            "UPDATE sync_jobs SET enabled=1,updated_at=? WHERE id=? AND status='pending' \
             AND enabled=0 AND EXISTS(SELECT 1 FROM accounts a \
             WHERE a.id=sync_jobs.account_id AND a.enabled=1)",
        )
        .bind(updated_at)
        .bind(job_id)
        .execute(&mut *conn)
        .await?;
        if restored.rows_affected() == 1 {
            return Ok(JobControlResult::Changed);
        }
        return Err(crate::error::Error::InconsistentTask(format!(
            "cannot resume sync job {job_id} while its account is disabled"
        )));
    }
    if !matches!(status, SyncJobStatus::Paused | SyncJobStatus::Interrupted) {
        return Err(crate::error::Error::InconsistentTask(format!(
            "cannot resume sync job {job_id} from {}",
            status.as_str()
        )));
    }
    let result = sqlx::query(
        "UPDATE sync_jobs SET status='pending',enabled=1,claimed_at=NULL,owner_token=NULL, \
          current_run_id=NULL,lease_until_epoch=NULL,last_error=NULL,updated_at=? \
          WHERE id=? AND status=? AND EXISTS(SELECT 1 FROM accounts a \
          WHERE a.id=sync_jobs.account_id AND a.enabled=1)",
    )
    .bind(updated_at)
    .bind(job_id)
    .bind(status.as_str())
    .execute(&mut *conn)
    .await?;
    if result.rows_affected() != 1 {
        return Err(crate::error::Error::InconsistentTask(format!(
            "sync job {job_id} changed during resume"
        )));
    }
    Ok(JobControlResult::Changed)
}

/// 取消 pending/running/paused/interrupted 任务。
pub async fn cancel_sync_job<'c, A>(
    acquirer: A,
    job_id: i64,
    updated_at: &str,
) -> Result<JobControlResult>
where
    A: Acquire<'c, Database = Sqlite>,
{
    let mut conn = acquirer.acquire().await?;
    let mut tx = conn.begin().await?;
    let status = current_sync_job_status(&mut tx, job_id).await?;
    if status == SyncJobStatus::Cancelled {
        tx.commit().await?;
        return Ok(JobControlResult::AlreadyApplied);
    }
    let changed = match status {
        SyncJobStatus::Running => {
            finish_running_job_for_control(&mut tx, job_id, SyncJobStatus::Cancelled, updated_at)
                .await?
        }
        SyncJobStatus::Pending | SyncJobStatus::Paused | SyncJobStatus::Interrupted => {
            sqlx::query(
                "UPDATE sync_jobs SET status='cancelled', generation=generation+1, \
                 owner_token=NULL, current_run_id=NULL, lease_until_epoch=NULL, updated_at=? \
                 WHERE id=? AND status=?",
            )
            .bind(updated_at)
            .bind(job_id)
            .bind(status.as_str())
            .execute(&mut *tx)
            .await?
            .rows_affected()
                == 1
        }
        _ => {
            return Err(crate::error::Error::InconsistentTask(format!(
                "cannot cancel sync job {job_id} from {}",
                status.as_str()
            )));
        }
    };
    if !changed {
        tx.rollback().await?;
        return Err(crate::error::Error::InconsistentTask(format!(
            "sync job {job_id} changed during cancel"
        )));
    }
    tx.commit().await?;
    Ok(JobControlResult::Changed)
}

/// 重试 failed 任务，仅迁移回 pending。
pub async fn retry_sync_job<'c, A>(
    acquirer: A,
    job_id: i64,
    updated_at: &str,
) -> Result<JobControlResult>
where
    A: Acquire<'c, Database = Sqlite>,
{
    let mut conn = acquirer.acquire().await?;
    let status = current_sync_job_status(&mut conn, job_id).await?;
    if status == SyncJobStatus::Pending {
        return Ok(JobControlResult::AlreadyApplied);
    }
    if status != SyncJobStatus::Failed {
        return Err(crate::error::Error::InconsistentTask(format!(
            "cannot retry sync job {job_id} from {}",
            status.as_str()
        )));
    }
    let result = sqlx::query(
        "UPDATE sync_jobs SET status='pending', recovery_count=0, pre_run_recovery_count=0, claimed_at=NULL, \
         owner_token=NULL, current_run_id=NULL, lease_until_epoch=NULL, last_error=NULL, \
         available_at_epoch=0, updated_at=? WHERE id=? AND status='failed'",
    )
    .bind(updated_at)
    .bind(job_id)
    .execute(&mut *conn)
    .await?;
    if result.rows_affected() != 1 {
        return Err(crate::error::Error::InconsistentTask(format!(
            "sync job {job_id} changed during retry"
        )));
    }
    Ok(JobControlResult::Changed)
}

/// 为 running job 创建一次 run；lease 只从 job 事实行复制。
pub async fn create_sync_run_at<'c, A>(
    acquirer: A,
    job_id: i64,
    owner_token: &str,
    generation: i64,
    now_epoch: i64,
    started_at: &str,
) -> Result<Option<i64>>
where
    A: Acquire<'c, Database = Sqlite>,
{
    let mut conn = acquirer.acquire().await?;
    let mut tx = conn.begin().await?;
    let run_id: Option<i64> = sqlx::query_scalar(
        "INSERT INTO sync_runs (job_id, status, started_at, attempt, updated_at, owner_token, generation, lease_until_epoch) \
         SELECT id, 'running', ?, recovery_count + 1, ?, owner_token, generation, lease_until_epoch FROM sync_jobs \
         WHERE id = ? AND status = 'running' AND owner_token = ? AND generation = ? \
          AND lease_until_epoch>? AND current_run_id IS NULL RETURNING id",
    )
    .bind(started_at)
    .bind(started_at)
    .bind(job_id)
    .bind(owner_token)
    .bind(generation)
    .bind(now_epoch)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(run_id) = run_id else {
        tx.commit().await?;
        return Ok(None);
    };
    let updated = sqlx::query(
        "UPDATE sync_jobs SET current_run_id = ? WHERE id = ? AND status = 'running' \
         AND owner_token = ? AND generation = ? AND current_run_id IS NULL",
    )
    .bind(run_id)
    .bind(job_id)
    .bind(owner_token)
    .bind(generation)
    .execute(&mut *tx)
    .await?;
    if updated.rows_affected() != 1 {
        tx.rollback().await?;
        return Ok(None);
    }
    tx.commit().await?;
    Ok(Some(run_id))
}

#[cfg(debug_assertions)]
pub async fn create_sync_run<'c, A>(
    acquirer: A,
    job_id: i64,
    owner_token: &str,
    generation: i64,
    started_at: &str,
) -> Result<Option<i64>>
where
    A: Acquire<'c, Database = Sqlite>,
{
    create_sync_run_at(acquirer, job_id, owner_token, generation, 0, started_at).await
}

/// 同一事务续租 job 与其 current run；旧 owner/generation 返回 false。
#[allow(clippy::too_many_arguments)]
pub async fn heartbeat_sync_run_at<'c, A>(
    acquirer: A,
    job_id: i64,
    run_id: i64,
    owner_token: &str,
    generation: i64,
    now_epoch: i64,
    lease_until_epoch: i64,
    updated_at: &str,
) -> Result<bool>
where
    A: Acquire<'c, Database = Sqlite>,
{
    if owner_token.is_empty() || now_epoch < 0 || lease_until_epoch <= now_epoch {
        return Err(crate::error::Error::FormatError(
            "invalid sync heartbeat".to_string(),
        ));
    }
    let mut conn = acquirer.acquire().await?;
    let mut tx = conn.begin().await?;
    let effective_lease: Option<i64> = sqlx::query_scalar(
        "UPDATE sync_jobs SET lease_until_epoch=MAX(lease_until_epoch,?), updated_at=? WHERE id=? AND status='running' \
           AND current_run_id=? AND owner_token=? AND generation=? AND enabled=1 \
           AND lease_until_epoch>? \
          AND EXISTS(SELECT 1 FROM accounts a WHERE a.id=sync_jobs.account_id AND a.enabled=1) \
          RETURNING lease_until_epoch",
    )
    .bind(lease_until_epoch)
    .bind(updated_at)
    .bind(job_id)
    .bind(run_id)
    .bind(owner_token)
    .bind(generation)
    .bind(now_epoch)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(effective_lease) = effective_lease else {
        tx.rollback().await?;
        return Ok(false);
    };
    let run = sqlx::query(
        "UPDATE sync_runs SET lease_until_epoch=?, updated_at=? WHERE id=? AND job_id=? \
         AND status='running' AND owner_token=? AND generation=?",
    )
    .bind(effective_lease)
    .bind(updated_at)
    .bind(run_id)
    .bind(job_id)
    .bind(owner_token)
    .bind(generation)
    .execute(&mut *tx)
    .await?;
    if run.rows_affected() != 1 {
        tx.rollback().await?;
        return Ok(false);
    }
    tx.commit().await?;
    Ok(true)
}

#[cfg(debug_assertions)]
pub async fn heartbeat_sync_run<'c, A>(
    acquirer: A,
    job_id: i64,
    run_id: i64,
    owner_token: &str,
    generation: i64,
    lease_until_epoch: i64,
    updated_at: &str,
) -> Result<bool>
where
    A: Acquire<'c, Database = Sqlite>,
{
    heartbeat_sync_run_at(
        acquirer,
        job_id,
        run_id,
        owner_token,
        generation,
        0,
        lease_until_epoch,
        updated_at,
    )
    .await
}

/// 兼容旧 DTO 写入；新代码应使用 create_sync_run/finish_sync_run。
pub async fn save_sync_run<'c, A>(acquirer: A, run: &SyncRunDto) -> Result<i64>
where
    A: Acquire<'c, Database = Sqlite>,
{
    if run.status == SyncJobStatus::Running.as_str() {
        return Err(crate::error::Error::FormatError(
            "running sync runs require queue ownership CAS".to_string(),
        ));
    }
    let mut conn = acquirer.acquire().await?;
    let (sql, values) = Query::insert()
        .into_table(SyncRunIden::Table)
        .columns([
            SyncRunIden::JobId,
            SyncRunIden::Status,
            SyncRunIden::StartedAt,
            SyncRunIden::FinishedAt,
            SyncRunIden::StatsJson,
            SyncRunIden::Error,
            SyncRunIden::Attempt,
            SyncRunIden::UpdatedAt,
            SyncRunIden::OwnerToken,
            SyncRunIden::Generation,
            SyncRunIden::LeaseUntilEpoch,
        ])
        .values([
            run.job_id.into(),
            run.status.clone().into(),
            run.started_at.clone().into(),
            run.finished_at.clone().into(),
            run.stats_json.clone().into(),
            run.error.clone().into(),
            run.attempt.into(),
            run.updated_at.clone().into(),
            run.owner_token.clone().into(),
            run.generation.into(),
            run.lease_until_epoch.into(),
        ])?
        .returning_col(SyncRunIden::Id)
        .build_sqlx(SqliteQueryBuilder);
    Ok(sqlx::query_scalar_with(&sql, values)
        .fetch_one(&mut *conn)
        .await?)
}

/// 仅当 run 仍由调用方持有有效 lease 时结束它。
pub async fn finish_sync_run_at(
    pool: &sqlx::SqlitePool,
    request: &FinishRunRequest,
    now_epoch: i64,
) -> Result<bool> {
    if !matches!(
        request.next_status,
        SyncJobStatus::Completed
            | SyncJobStatus::Failed
            | SyncJobStatus::Cancelled
            | SyncJobStatus::Interrupted
    ) {
        return Err(crate::error::Error::FormatError(
            "invalid terminal sync run status".to_string(),
        ));
    }
    let request = request.clone();
    crate::sqlite_write::with_immediate_transaction(pool, |conn| Box::pin(async move {
    let owned: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM sync_jobs j JOIN sync_runs r ON r.id=j.current_run_id \
         WHERE j.id=? AND j.status='running' AND j.enabled=1 AND j.current_run_id=? AND j.owner_token=? \
         AND j.generation=? AND j.lease_until_epoch>? AND r.job_id=j.id AND r.status='running' \
         AND r.owner_token=j.owner_token AND r.generation=j.generation \
         AND EXISTS(SELECT 1 FROM accounts a WHERE a.id=j.account_id AND a.enabled=1))",
    )
    .bind(request.job_id)
    .bind(request.run_id)
    .bind(&request.owner_token)
    .bind(request.generation)
    .bind(now_epoch)
    .fetch_one(&mut *conn)
    .await?;
    if !owned {
        return Ok(false);
    }
    let job = sqlx::query(
        "UPDATE sync_jobs SET status = ?, current_run_id = NULL, owner_token = NULL, \
         lease_until_epoch = NULL, last_error = ?, updated_at = ?, \
         rate_limit_backoff_level=CASE WHEN ?='completed' THEN 0 ELSE rate_limit_backoff_level END \
          WHERE id = ? AND status = 'running' \
          AND current_run_id = ? AND owner_token = ? AND generation = ? AND lease_until_epoch > ?",
    )
    .bind(request.next_status.as_str())
    .bind(&request.error)
    .bind(&request.finished_at)
    .bind(request.next_status.as_str())
    .bind(request.job_id)
    .bind(request.run_id)
    .bind(&request.owner_token)
    .bind(request.generation)
    .bind(now_epoch)
    .execute(&mut *conn)
    .await?;
    if job.rows_affected() != 1 {
        return Err(crate::error::Error::InconsistentTask(
            "sync job finish lost ownership after preflight".into(),
        ));
    }
    if request.next_status == SyncJobStatus::Completed {
        sqlx::query(
            "UPDATE rate_limit_gates SET backoff_level=0,retry_after_epoch=NULL,updated_at=? \
             WHERE account_id=(SELECT account_id FROM sync_jobs WHERE id=?) \
             AND endpoint_key=(SELECT endpoint_key FROM sync_jobs WHERE id=?) \
             AND revision=(SELECT endpoint_gate_revision FROM sync_jobs WHERE id=?)",
        )
        .bind(&request.finished_at)
        .bind(request.job_id)
        .bind(request.job_id)
        .bind(request.job_id)
        .execute(&mut *conn)
        .await?;
        sqlx::query(
            "UPDATE rate_limit_gates SET backoff_level=0,retry_after_epoch=NULL,updated_at=? \
             WHERE account_id=(SELECT account_id FROM sync_jobs WHERE id=?) \
             AND endpoint_key='__account__' \
             AND revision=(SELECT account_gate_revision FROM sync_jobs WHERE id=?)",
        )
        .bind(&request.finished_at)
        .bind(request.job_id)
        .bind(request.job_id)
        .execute(&mut *conn)
        .await?;
    }
    let run = sqlx::query(
        "UPDATE sync_runs SET status = ?, finished_at = ?, stats_json = ?, error = ?, updated_at = ? \
         WHERE id = ? AND job_id = ? AND status = 'running' AND owner_token = ? AND generation = ?",
    )
    .bind(request.next_status.as_str())
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
    if run.rows_affected() != 1 {
        return Err(crate::error::Error::InconsistentTask(
            "sync run finish lost ownership after preflight".into(),
        ));
    }
    Ok(true)
    })).await
}

#[cfg(debug_assertions)]
pub async fn finish_sync_run(pool: &sqlx::SqlitePool, request: &FinishRunRequest) -> Result<bool> {
    finish_sync_run_at(pool, request, -1).await
}

/// 按 job 读取最近的运行历史。
pub async fn get_sync_run_history<'e, E>(
    executor: E,
    job_id: i64,
    limit: u64,
) -> Result<Vec<SyncRunDto>>
where
    E: Executor<'e, Database = Sqlite>,
{
    let (sql, values) = Query::select()
        .columns([
            SyncRunIden::Id,
            SyncRunIden::JobId,
            SyncRunIden::Status,
            SyncRunIden::StartedAt,
            SyncRunIden::FinishedAt,
            SyncRunIden::StatsJson,
            SyncRunIden::Error,
            SyncRunIden::Attempt,
            SyncRunIden::UpdatedAt,
            SyncRunIden::OwnerToken,
            SyncRunIden::Generation,
            SyncRunIden::LeaseUntilEpoch,
        ])
        .from(SyncRunIden::Table)
        .and_where(Expr::col(SyncRunIden::JobId).eq(job_id))
        .order_by(SyncRunIden::Id, sea_query::Order::Desc)
        .limit(limit)
        .build_sqlx(SqliteQueryBuilder);
    Ok(sqlx::query_as_with::<Sqlite, SyncRunDto, _>(&sql, values)
        .fetch_all(executor)
        .await?)
}

/// 启动恢复统计。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoverySummary {
    pub requeued: u64,
    pub failed: u64,
}

/// 当前 owned run 真实中断后的单 job 恢复结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InterruptedRecoveryResult {
    pub status: SyncJobStatus,
    pub recovery_count: i64,
}

/// 原子结束真实中断的 owned run，并仅按该 job 自身恢复上限转 pending/failed。
pub async fn recover_interrupted_sync_run_at<'c, A>(
    acquirer: A,
    request: &FinishRunRequest,
    now_epoch: i64,
) -> Result<Option<InterruptedRecoveryResult>>
where
    A: Acquire<'c, Database = Sqlite>,
{
    let mut conn = acquirer.acquire().await?;
    let mut tx = conn.begin().await?;
    let recovered: Option<(String, i64)> = sqlx::query_as(
        "UPDATE sync_jobs SET recovery_count = recovery_count + 1, \
         status = CASE WHEN recovery_count + 1 >= max_recovery_attempts THEN 'failed' ELSE 'pending' END, \
         current_run_id = NULL, owner_token = NULL, lease_until_epoch = NULL, claimed_at = NULL, \
          last_error = ?, updated_at = ? WHERE id = ? AND status = 'running' \
          AND current_run_id = ? AND owner_token = ? AND generation = ? \
          AND enabled=1 AND lease_until_epoch>? \
          AND EXISTS(SELECT 1 FROM accounts a WHERE a.id=sync_jobs.account_id AND a.enabled=1) \
          RETURNING status, recovery_count",
    )
    .bind(&request.error)
    .bind(&request.finished_at)
    .bind(request.job_id)
    .bind(request.run_id)
    .bind(&request.owner_token)
    .bind(request.generation)
    .bind(now_epoch)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((status, recovery_count)) = recovered else {
        tx.rollback().await?;
        return Ok(None);
    };
    let run = sqlx::query(
        "UPDATE sync_runs SET status = 'interrupted', finished_at = ?, stats_json = ?, \
         error = ?, updated_at = ? WHERE id = ? AND job_id = ? AND status = 'running' \
         AND owner_token = ? AND generation = ?",
    )
    .bind(&request.finished_at)
    .bind(&request.stats_json)
    .bind(&request.error)
    .bind(&request.finished_at)
    .bind(request.run_id)
    .bind(request.job_id)
    .bind(&request.owner_token)
    .bind(request.generation)
    .execute(&mut *tx)
    .await?;
    if run.rows_affected() != 1 {
        tx.rollback().await?;
        return Ok(None);
    }
    let status = match status.as_str() {
        "pending" => SyncJobStatus::Pending,
        "failed" => SyncJobStatus::Failed,
        _ => {
            tx.rollback().await?;
            return Err(crate::error::Error::InconsistentTask(format!(
                "unexpected interrupted recovery status {status}"
            )));
        }
    };
    tx.commit().await?;
    Ok(Some(InterruptedRecoveryResult {
        status,
        recovery_count,
    }))
}

/// 正常退出时原子中断 owned run 并重排 job，不消耗故障恢复预算。
pub async fn interrupt_sync_run_for_shutdown_at<'c, A>(
    acquirer: A,
    request: &FinishRunRequest,
    now_epoch: i64,
) -> Result<bool>
where
    A: Acquire<'c, Database = Sqlite>,
{
    let mut conn = acquirer.acquire().await?;
    let mut tx = conn.begin().await?;
    let job = sqlx::query(
        "UPDATE sync_jobs SET status='pending', current_run_id=NULL, owner_token=NULL, \
         lease_until_epoch=NULL, claimed_at=NULL, last_error=?, updated_at=? \
         WHERE id=? AND status='running' AND current_run_id=? AND owner_token=? AND generation=? \
         AND enabled=1 AND lease_until_epoch>? \
         AND EXISTS(SELECT 1 FROM accounts a WHERE a.id=sync_jobs.account_id AND a.enabled=1)",
    )
    .bind(&request.error)
    .bind(&request.finished_at)
    .bind(request.job_id)
    .bind(request.run_id)
    .bind(&request.owner_token)
    .bind(request.generation)
    .bind(now_epoch)
    .execute(&mut *tx)
    .await?;
    if job.rows_affected() != 1 {
        tx.rollback().await?;
        return Ok(false);
    }
    let run = sqlx::query(
        "UPDATE sync_runs SET status='interrupted', finished_at=?, stats_json=?, error=?, updated_at=? \
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
    .execute(&mut *tx)
    .await?;
    if run.rows_affected() != 1 {
        tx.rollback().await?;
        return Ok(false);
    }
    tx.commit().await?;
    Ok(true)
}

#[cfg(debug_assertions)]
pub async fn recover_interrupted_sync_run<'c, A>(
    acquirer: A,
    request: &FinishRunRequest,
) -> Result<Option<InterruptedRecoveryResult>>
where
    A: Acquire<'c, Database = Sqlite>,
{
    recover_interrupted_sync_run_at(acquirer, request, -1).await
}

/// 原子恢复 lease 已过期的 running job 及其全部 running run。
pub async fn recover_interrupted_sync_jobs<'c, A>(
    acquirer: A,
    now_epoch: i64,
    recovered_at: &str,
) -> Result<RecoverySummary>
where
    A: Acquire<'c, Database = Sqlite>,
{
    let mut conn = acquirer.acquire().await?;
    let mut tx = conn.begin().await?;
    let summary = recover_interrupted_sync_jobs_on_conn(&mut tx, now_epoch, recovered_at).await?;
    tx.commit().await?;
    Ok(summary)
}

async fn recover_interrupted_sync_jobs_on_conn(
    conn: &mut sqlx::SqliteConnection,
    now_epoch: i64,
    recovered_at: &str,
) -> Result<RecoverySummary> {
    let expired: Vec<(i64, Option<i64>)> = sqlx::query_as(
        "SELECT id, current_run_id FROM sync_jobs WHERE status = 'running' \
         AND lease_until_epoch IS NOT NULL AND lease_until_epoch <= ?",
    )
    .bind(now_epoch)
    .fetch_all(&mut *conn)
    .await?;
    let mut requeued = 0;
    let mut failed = 0;
    for (job_id, run_id) in expired {
        if let Some(run_id) = run_id {
            sqlx::query(
                "UPDATE sync_runs SET status = 'interrupted', finished_at = ?, updated_at = ?, \
                 error = COALESCE(error, 'worker lease expired') WHERE id = ? AND job_id = ? \
                 AND status = 'running'",
            )
            .bind(recovered_at)
            .bind(recovered_at)
            .bind(run_id)
            .bind(job_id)
            .execute(&mut *conn)
            .await?;
        }
        let status: Option<String> = sqlx::query_scalar(
        "UPDATE sync_jobs SET recovery_count = recovery_count + CASE WHEN current_run_id IS NULL THEN 0 ELSE 1 END, \
             pre_run_recovery_count = pre_run_recovery_count + CASE WHEN current_run_id IS NULL THEN 1 ELSE 0 END, \
             status = CASE WHEN current_run_id IS NULL AND pre_run_recovery_count + 1 >= max_recovery_attempts THEN 'failed' \
             WHEN current_run_id IS NULL THEN 'pending' \
             WHEN recovery_count + 1 >= max_recovery_attempts THEN 'failed' ELSE 'pending' END, \
             claimed_at = NULL, current_run_id = NULL, owner_token = NULL, lease_until_epoch = NULL, \
             updated_at = ?, last_error = CASE WHEN current_run_id IS NULL AND pre_run_recovery_count + 1 >= max_recovery_attempts \
             THEN 'worker lease expired before run started: recovery limit reached' WHEN current_run_id IS NULL \
             THEN 'worker lease expired before run started' ELSE 'worker lease expired' END \
             WHERE id = ? AND status = 'running' \
              AND lease_until_epoch IS NOT NULL AND lease_until_epoch <= ? RETURNING status",
        )
        .bind(recovered_at)
        .bind(job_id)
        .bind(now_epoch)
        .fetch_optional(&mut *conn)
        .await?;
        match status.as_deref() {
            Some("pending") => requeued += 1,
            Some("failed") => failed += 1,
            _ => {}
        }
    }
    Ok(RecoverySummary { requeued, failed })
}

/// 保存同步 checkpoint。
///
/// Sidecar sequence 只在单次 request 内有意义。ad-hoc 请求使用累计
/// `fetched_count` 防倒退；persistent 请求还必须持有 resource 对应 job 的当前
/// run/generation。checkpoint 以 stream/resource 为稳定身份，允许后续 job 接管。
pub async fn save_sync_checkpoint_at<'c, A>(
    acquirer: A,
    checkpoint: &SyncCheckpointDto,
    now_epoch: i64,
) -> Result<bool>
where
    A: Acquire<'c, Database = Sqlite>,
{
    let mut conn = acquirer.acquire().await?;
    if checkpoint.fetched_count < 0 {
        return Err(crate::error::Error::FormatError(
            "checkpoint fetched_count cannot be negative".to_string(),
        ));
    }
    let result = match &checkpoint.owner {
        CheckpointOwner::AdHoc => {
            sqlx::query(
                "INSERT INTO sync_checkpoints \
                 (stream, cursor_json, fetched_count, last_sequence, updated_at, job_id, run_id, generation, owner_token) \
                 VALUES (?, ?, ?, ?, ?, NULL, NULL, NULL, NULL) ON CONFLICT(stream) DO UPDATE SET \
                 cursor_json=excluded.cursor_json, fetched_count=excluded.fetched_count, \
                 last_sequence=excluded.last_sequence, updated_at=excluded.updated_at, job_id=NULL, \
                 run_id=NULL, generation=NULL, owner_token=NULL \
                 WHERE sync_checkpoints.job_id IS NULL \
                 AND excluded.fetched_count > sync_checkpoints.fetched_count",
            )
            .bind(&checkpoint.stream)
            .bind(&checkpoint.cursor_json)
            .bind(checkpoint.fetched_count)
            .bind(checkpoint.last_sequence)
            .bind(&checkpoint.updated_at)
            .execute(&mut *conn)
            .await?
        }
        CheckpointOwner::Persistent {
            run_id,
            generation,
            owner_token,
        } => {
            let Some(job_id) = checkpoint.job_id else {
                return Err(crate::error::Error::FormatError(
                    "persistent checkpoint requires job_id".to_string(),
                ));
            };
            sqlx::query(
                "INSERT INTO sync_checkpoints \
                 (stream, cursor_json, fetched_count, last_sequence, updated_at, job_id, run_id, generation, owner_token) \
                 SELECT ?, ?, ?, ?, ?, j.id, r.id, j.generation, j.owner_token \
                 FROM sync_jobs j JOIN sync_runs r ON r.id = j.current_run_id \
                   WHERE j.id = ? AND j.status = 'running' AND j.enabled=1 AND j.current_run_id = ? \
                   AND j.resource_key = ? \
                   AND j.generation = ? AND j.owner_token = ? AND j.lease_until_epoch > ? \
                   AND r.status = 'running' \
                  AND r.generation = ? AND r.owner_token = ? \
                  AND EXISTS(SELECT 1 FROM accounts a WHERE a.id=j.account_id AND a.enabled=1) \
                 ON CONFLICT(stream) DO UPDATE SET cursor_json=excluded.cursor_json, \
                 fetched_count=excluded.fetched_count, last_sequence=excluded.last_sequence, \
                 updated_at=excluded.updated_at, job_id=excluded.job_id, run_id=excluded.run_id, \
                 generation=excluded.generation, owner_token=excluded.owner_token \
                 WHERE excluded.stream = ? AND ( \
                  excluded.fetched_count > sync_checkpoints.fetched_count OR ( \
                   excluded.fetched_count = sync_checkpoints.fetched_count \
                   AND excluded.cursor_json IS NOT sync_checkpoints.cursor_json \
                  ) \
                 )",
            )
            .bind(&checkpoint.stream)
            .bind(&checkpoint.cursor_json)
            .bind(checkpoint.fetched_count)
            .bind(checkpoint.last_sequence)
            .bind(&checkpoint.updated_at)
            .bind(job_id)
            .bind(run_id)
            .bind(&checkpoint.stream)
            .bind(generation)
            .bind(owner_token)
            .bind(now_epoch)
            .bind(generation)
            .bind(owner_token)
            .bind(&checkpoint.stream)
            .execute(&mut *conn)
            .await?
        }
    };
    Ok(result.rows_affected() == 1)
}

#[cfg(debug_assertions)]
pub async fn save_sync_checkpoint<'c, A>(
    acquirer: A,
    checkpoint: &SyncCheckpointDto,
) -> Result<bool>
where
    A: Acquire<'c, Database = Sqlite>,
{
    save_sync_checkpoint_at(acquirer, checkpoint, -1).await
}

/// 按 stream 读取同步 checkpoint。
pub async fn get_sync_checkpoint<'e, E>(
    executor: E,
    stream: &str,
) -> Result<Option<SyncCheckpointDto>>
where
    E: Executor<'e, Database = Sqlite>,
{
    let (sql, values) = Query::select()
        .columns([
            SyncCheckpointIden::Stream,
            SyncCheckpointIden::CursorJson,
            SyncCheckpointIden::FetchedCount,
            SyncCheckpointIden::LastSequence,
            SyncCheckpointIden::UpdatedAt,
            SyncCheckpointIden::JobId,
            SyncCheckpointIden::RunId,
            SyncCheckpointIden::Generation,
            SyncCheckpointIden::OwnerToken,
        ])
        .from(SyncCheckpointIden::Table)
        .and_where(Expr::col(SyncCheckpointIden::Stream).eq(stream))
        .build_sqlx(SqliteQueryBuilder);
    let mut checkpoint = sqlx::query_as_with::<Sqlite, SyncCheckpointDto, _>(&sql, values)
        .fetch_optional(executor)
        .await?;
    if let Some(checkpoint) = checkpoint.as_mut()
        && let (Some(run_id), Some(generation), Some(owner_token)) = (
            checkpoint.run_id,
            checkpoint.generation,
            checkpoint.owner_token.clone(),
        )
    {
        checkpoint.owner = CheckpointOwner::Persistent {
            run_id,
            generation,
            owner_token,
        };
    }
    Ok(checkpoint)
}

/// 记录已处理的幂等事件。返回 `true` 表示新插入，`false` 表示重复（已存在）。
pub async fn record_processed_event<'c, A>(acquirer: A, event: &ProcessedEventDto) -> Result<bool>
where
    A: Acquire<'c, Database = Sqlite>,
{
    let mut conn = acquirer.acquire().await?;
    let (sql, values) = Query::insert()
        .into_table(ProcessedEventIden::Table)
        .columns([
            ProcessedEventIden::EventId,
            ProcessedEventIden::Stream,
            ProcessedEventIden::Sequence,
            ProcessedEventIden::RequestId,
            ProcessedEventIden::ProcessedAt,
        ])
        .values([
            event.event_id.clone().into(),
            event.stream.clone().into(),
            event.sequence.into(),
            event.request_id.clone().into(),
            event.processed_at.clone().into(),
        ])?
        .on_conflict(
            OnConflict::column(ProcessedEventIden::EventId)
                .do_nothing()
                .to_owned(),
        )
        .build_sqlx(SqliteQueryBuilder);
    let result = sqlx::query_with(&sql, values).execute(&mut *conn).await?;
    Ok(result.rows_affected() > 0)
}

/// 幂等事件是否已存在。
pub async fn event_already_processed<'e, E>(executor: E, event_id: &str) -> Result<bool>
where
    E: Executor<'e, Database = Sqlite>,
{
    let (sql, values) = Query::select()
        .expr(Expr::col(ProcessedEventIden::EventId).count())
        .from(ProcessedEventIden::Table)
        .and_where(Expr::col(ProcessedEventIden::EventId).eq(event_id))
        .build_sqlx(SqliteQueryBuilder);
    let count: i64 = sqlx::query_scalar_with(&sql, values)
        .fetch_one(executor)
        .await?;
    Ok(count > 0)
}

/// 按保留策略清理过期幂等事件（保留最近 `keep` 条）。
///
/// `keep` 为 `u64`，直接内联进 SQL 无注入风险。
pub async fn prune_processed_events<'e, E>(executor: E, keep: u64) -> Result<u64>
where
    E: Executor<'e, Database = Sqlite>,
{
    let sql = format!(
        "DELETE FROM processed_events WHERE id NOT IN \
         (SELECT id FROM processed_events ORDER BY id DESC LIMIT {keep})"
    );
    let result = sqlx::query(&sql).execute(executor).await?;
    Ok(result.rows_affected())
}

/// 事务提交计划：业务数据 + checkpoint + 幂等事件同一事务提交。
///
/// - 调用方先把要写入的评论/媒体放入 `comments`/`media`。
/// - [`transactional::CommitPlan::execute`] 会先检查事件是否已处理；
///   重复事件返回 `CommitOutcome::Duplicate` 且不重复写入业务数据。
/// - 成功后更新 checkpoint；任何一步失败整体回滚。
pub mod transactional {
    use super::*;

    fn is_placeholder_url(url: &url::Url) -> bool {
        url.host_str() == Some("example.invalid")
    }

    fn merge_collected_user(
        existing: crate::models::User,
        incoming: &crate::models::User,
    ) -> crate::models::User {
        let sparse = incoming.screen_name.is_empty()
            || incoming.domain.is_empty()
            || is_placeholder_url(&incoming.avatar_hd)
            || is_placeholder_url(&incoming.avatar_large)
            || is_placeholder_url(&incoming.profile_image_url);
        crate::models::User {
            id: incoming.id,
            screen_name: if incoming.screen_name.is_empty() {
                existing.screen_name
            } else {
                incoming.screen_name.clone()
            },
            domain: if incoming.domain.is_empty() {
                existing.domain
            } else {
                incoming.domain.clone()
            },
            avatar_hd: if is_placeholder_url(&incoming.avatar_hd) {
                existing.avatar_hd
            } else {
                incoming.avatar_hd.clone()
            },
            avatar_large: if is_placeholder_url(&incoming.avatar_large) {
                existing.avatar_large
            } else {
                incoming.avatar_large.clone()
            },
            profile_image_url: if is_placeholder_url(&incoming.profile_image_url) {
                existing.profile_image_url
            } else {
                incoming.profile_image_url.clone()
            },
            following: if sparse {
                incoming.following || existing.following
            } else {
                incoming.following
            },
            follow_me: if sparse {
                incoming.follow_me || existing.follow_me
            } else {
                incoming.follow_me
            },
        }
    }

    /// 单事务提交的输入。
    #[derive(Debug)]
    pub struct CommitPlan {
        /// 关联的命令 request_id。
        pub request_id: Option<String>,
        /// 事件流标识。
        pub stream: String,
        /// 事件序号（从 1 单调递增）。
        pub sequence: i64,
        /// 事件全局幂等键。
        pub event_id: String,
        /// 本事件携带的用户。
        pub users: Vec<crate::models::User>,
        /// 本事件携带的帖子（P1-B 起支持同事务写入）。
        pub posts: Vec<super::super::post::PostInternal>,
        /// 本事件携带的评论。
        pub comments: Vec<CommentDto>,
        /// 本事件携带的媒体引用。
        pub media: Vec<MediaDto>,
        /// 提交后要写入的 checkpoint。
        pub checkpoint: SyncCheckpointDto,
        /// 已处理时间（RFC3339）。
        pub processed_at: String,
    }

    /// 提交结果。
    #[derive(Debug, PartialEq, Eq)]
    pub enum CommitOutcome {
        /// 事件是新的，已写入业务数据并更新 checkpoint。
        Applied,
        /// 事件已处理过，跳过，未产生重复数据。
        Duplicate,
    }

    impl CommitPlan {
        /// 在单个事务中执行：幂等去重 → 写评论/媒体 → 更新 checkpoint。
        ///
        /// 重复 `event_id` 不产生重复数据；业务主键冲突由 upsert 吸收。
        pub async fn execute_at<'c, A>(&self, acquirer: A, now_epoch: i64) -> Result<CommitOutcome>
        where
            A: Acquire<'c, Database = Sqlite>,
        {
            let mut conn = acquirer.acquire().await?;
            let mut tx = conn.begin().await?;

            // 幂等去重（同一事务内，先查后插防重复）。
            if event_already_processed(&mut *tx, &self.event_id).await? {
                warn!("duplicate event_id {} skipped", self.event_id);
                tx.commit().await?;
                return Ok(CommitOutcome::Duplicate);
            }

            for user in &self.users {
                let merged = match super::super::user::get_user(&mut *tx, user.id).await? {
                    Some(existing) => merge_collected_user(existing, user),
                    None => user.clone(),
                };
                super::super::user::save_user(&mut *tx, &merged).await?;
            }
            for post in &self.posts {
                let preserved;
                let post = if post.content_status == "partial" {
                    preserved = super::super::post::get_post(&mut *tx, post.id).await?;
                    match preserved.as_ref() {
                        Some(existing) if existing.content_status == "complete" => existing,
                        _ => post,
                    }
                } else {
                    post
                };
                super::super::post::save_post(&mut *tx, post).await?;
            }
            for comment in &self.comments {
                save_comment(&mut *tx, comment).await?;
            }
            for media in &self.media {
                save_media_reference(&mut *tx, media).await?;
            }
            if !save_sync_checkpoint_at(&mut *tx, &self.checkpoint, now_epoch).await? {
                return Err(crate::error::Error::InconsistentTask(
                    "checkpoint ownership or progress rejected".to_string(),
                ));
            }

            record_processed_event(
                &mut *tx,
                &ProcessedEventDto {
                    id: 0,
                    event_id: self.event_id.clone(),
                    stream: Some(self.stream.clone()),
                    sequence: Some(self.sequence),
                    request_id: self.request_id.clone(),
                    processed_at: self.processed_at.clone(),
                },
            )
            .await?;

            tx.commit().await?;
            Ok(CommitOutcome::Applied)
        }

        #[cfg(debug_assertions)]
        pub async fn execute<'c, A>(&self, acquirer: A) -> Result<CommitOutcome>
        where
            A: Acquire<'c, Database = Sqlite>,
        {
            self.execute_at(acquirer, -1).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::SqlitePool;

    async fn setup_db() -> SqlitePool {
        let pool = SqlitePool::connect(":memory:").await.unwrap();
        sqlx::migrate!().run(&pool).await.unwrap();
        pool
    }

    fn now() -> String {
        chrono::Utc::now().to_rfc3339()
    }

    fn sample_comment(id: i64) -> CommentDto {
        CommentDto {
            id,
            post_id: 9001,
            root_id: None,
            parent_id: None,
            user_id: Some(10001),
            text: format!("comment {id}"),
            created_at: now(),
            depth: 0,
            child_count: 0,
            like_count: 0,
            source: Some("iPhone".into()),
            media_json: None,
            raw_data: None,
            content_status: "complete".into(),
            deleted: false,
            first_fetched_at: None,
            last_refreshed_at: None,
        }
    }

    fn sample_post(id: i64) -> crate::storage::internal::post::PostInternal {
        crate::storage::internal::post::PostInternal {
            attitudes_count: None,
            attitudes_status: 0,
            comments_count: None,
            created_at: now(),
            deleted: false,
            edit_count: None,
            favorited: false,
            geo: None,
            id,
            mblogid: format!("Mb{id}"),
            mix_media_ids: None,
            mix_media_info: None,
            page_info: None,
            pic_ids: None,
            pic_infos: None,
            pic_num: None,
            region_name: None,
            reposts_count: None,
            repost_type: None,
            retweeted_id: None,
            source: None,
            tag_struct: None,
            text: format!("post {id}"),
            uid: Some(10001),
            url_struct: None,
            bid: None,
            location: None,
            topic_ids: None,
            at_users: None,
            is_long_text: false,
            video_url: None,
            raw_data: None,
            content_status: "complete".into(),
            fetch_error: None,
            first_fetched_at: None,
            last_refreshed_at: None,
        }
    }

    #[tokio::test]
    async fn test_comment_roundtrip() {
        let db = setup_db().await;
        let comment = sample_comment(42);
        save_comment(&db, &comment).await.unwrap();
        let fetched = get_comment(&db, 42).await.unwrap().unwrap();
        assert_eq!(fetched, comment);

        // 幂等 upsert：重复保存不产生重复行。
        save_comment(&db, &comment).await.unwrap();
        let all = get_comments_by_post(&db, 9001).await.unwrap();
        assert_eq!(all.len(), 1);
    }

    #[tokio::test]
    async fn test_media_roundtrip_and_url_unique() {
        let db = setup_db().await;
        let media = MediaDto {
            id: 0,
            owner_type: MediaOwnerType::Post.as_str().into(),
            owner_id: Some(9001),
            media_type: MediaType::Picture.as_str().into(),
            url: "https://wx1.sinaimg.cn/abc.jpg".into(),
            local_path: Some("pictures/abc.jpg".into()),
            status: MediaStatus::Pending.as_str().into(),
            retry_count: 0,
            last_error: None,
            created_at: now(),
            updated_at: None,
        };
        save_media(&db, &media).await.unwrap();
        let fetched = get_media_by_url(&db, &media.url).await.unwrap().unwrap();
        assert_eq!(fetched.media_type, "picture");
        assert_eq!(fetched.owner_id, Some(9001));

        // 相同 url 幂等，本地路径被更新。
        let mut updated = media.clone();
        updated.local_path = Some("pictures/abc_v2.jpg".into());
        updated.status = MediaStatus::Downloaded.as_str().into();
        save_media(&db, &updated).await.unwrap();
        let fetched2 = get_media_by_url(&db, &media.url).await.unwrap().unwrap();
        assert_eq!(fetched2.local_path.as_deref(), Some("pictures/abc_v2.jpg"));
        assert_eq!(fetched2.status, "downloaded");
    }

    #[tokio::test]
    async fn test_media_reference_preserves_download_state() {
        let db = setup_db().await;
        let downloaded = MediaDto {
            id: 0,
            owner_type: "post".into(),
            owner_id: Some(9001),
            media_type: "picture".into(),
            url: "https://example.com/preserved.jpg".into(),
            local_path: Some("pictures/preserved.jpg".into()),
            status: MediaStatus::Downloaded.as_str().into(),
            retry_count: 2,
            last_error: Some("old error".into()),
            created_at: now(),
            updated_at: Some(now()),
        };
        save_media(&db, &downloaded).await.unwrap();

        let pending_reference = MediaDto {
            owner_id: Some(9002),
            local_path: None,
            status: MediaStatus::Pending.as_str().into(),
            retry_count: 0,
            last_error: None,
            ..downloaded.clone()
        };
        save_media_reference(&db, &pending_reference).await.unwrap();

        let fetched = get_media_by_url(&db, &downloaded.url)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fetched.local_path, downloaded.local_path);
        assert_eq!(fetched.status, "downloaded");
        assert_eq!(fetched.retry_count, 2);
        assert_eq!(fetched.last_error, downloaded.last_error);
        assert_eq!(fetched.owner_id, downloaded.owner_id);
    }

    #[tokio::test]
    async fn test_monitored_user_roundtrip() {
        let db = setup_db().await;
        let account_id = save_account(
            &db,
            &AccountDto {
                id: 0,
                provider: "test".into(),
                uid: "monitor".into(),
                display_name: None,
                session_ref: "sessions/monitor.json".into(),
                enabled: true,
                created_at: now(),
                updated_at: None,
            },
        )
        .await
        .unwrap();
        let user = MonitoredUserDto {
            account_id,
            uid: 10001,
            screen_name: Some("小明".into()),
            refresh_strategy: "daily".into(),
            enabled: true,
            last_refreshed_at: None,
            created_at: now(),
            updated_at: None,
            tier: RefreshTier::Cold,
            interval_secs: 0,
            jitter_secs: 0,
            next_refresh_epoch: 0,
            last_refresh_epoch: None,
        };
        save_monitored_user(&db, &user).await.unwrap();
        let enabled = get_enabled_monitored_users(&db).await.unwrap();
        assert_eq!(enabled.len(), 1);
        assert_eq!(enabled[0].screen_name.as_deref(), Some("小明"));
    }

    #[tokio::test]
    async fn test_sync_job_and_run_roundtrip() {
        let db = setup_db().await;
        let account_id = save_account(
            &db,
            &AccountDto {
                id: 0,
                provider: "test".into(),
                uid: "job".into(),
                display_name: None,
                session_ref: "sessions/job.json".into(),
                enabled: true,
                created_at: now(),
                updated_at: None,
            },
        )
        .await
        .unwrap();
        let job_id = enqueue_sync_job_spec(
            &db,
            &SyncJobSpec::CollectUserPosts {
                account_id,
                uid: 123,
                max_pages: None,
                priority: 1,
            },
            0,
            &now(),
        )
        .await
        .unwrap();
        assert!(job_id > 0);

        let run = SyncRunDto {
            id: 0,
            job_id,
            status: "completed".into(),
            started_at: now(),
            finished_at: None,
            stats_json: None,
            error: None,
            attempt: 1,
            updated_at: None,
            owner_token: None,
            generation: 0,
            lease_until_epoch: None,
        };
        let run_id = save_sync_run(&db, &run).await.unwrap();
        assert!(run_id > 0);

        let jobs = get_sync_jobs(&db).await.unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].kind, "collect_user_posts");
    }

    #[tokio::test]
    async fn test_checkpoint_roundtrip_and_upsert() {
        let db = setup_db().await;
        let cp = SyncCheckpointDto {
            stream: "user:123:posts".into(),
            cursor_json: Some("{\"cursor\":{\"max_id\":\"p1_after\"}}".into()),
            fetched_count: 20,
            last_sequence: Some(20),
            updated_at: now(),
            job_id: None,
            run_id: None,
            generation: None,
            owner_token: None,
            owner: CheckpointOwner::AdHoc,
        };
        save_sync_checkpoint(&db, &cp).await.unwrap();
        let fetched = get_sync_checkpoint(&db, "user:123:posts")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fetched.fetched_count, 20);

        // upsert 更新游标，不产生第二行。
        let mut cp2 = cp.clone();
        cp2.fetched_count = 40;
        cp2.last_sequence = Some(40);
        cp2.cursor_json = Some("{\"cursor\":{\"max_id\":\"p2_after\"}}".into());
        save_sync_checkpoint(&db, &cp2).await.unwrap();
        let all: Vec<SyncCheckpointDto> = sqlx::query_as("SELECT stream, cursor_json, fetched_count, last_sequence, updated_at, job_id, run_id, generation, owner_token FROM sync_checkpoints")
            .fetch_all(&db)
            .await
            .unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].fetched_count, 40);
    }

    #[tokio::test]
    async fn test_processed_event_idempotency() {
        let db = setup_db().await;
        let event = ProcessedEventDto {
            id: 0,
            event_id: "019fbbd7-ea26-7b7c-b113-c89ac2788773".into(),
            stream: Some("post:9001:comments".into()),
            sequence: Some(1),
            request_id: Some("req-1".into()),
            processed_at: now(),
        };
        assert!(record_processed_event(&db, &event).await.unwrap());
        // 重复插入被忽略。
        assert!(!record_processed_event(&db, &event).await.unwrap());
        assert!(event_already_processed(&db, &event.event_id).await.unwrap());
        assert!(!event_already_processed(&db, "unknown-event").await.unwrap());
    }

    #[tokio::test]
    async fn test_transactional_commit_applies_and_deduplicates() {
        let db = setup_db().await;
        let checkpoint = SyncCheckpointDto {
            stream: "user:123:posts".into(),
            cursor_json: Some("{\"cursor\":{\"max_id\":\"p1_after\"}}".into()),
            fetched_count: 1,
            last_sequence: Some(1),
            updated_at: now(),
            job_id: None,
            run_id: None,
            generation: None,
            owner_token: None,
            owner: CheckpointOwner::AdHoc,
        };
        let plan = transactional::CommitPlan {
            request_id: Some("req-1".into()),
            stream: "user:123:posts".into(),
            sequence: 1,
            event_id: "019fbbd7-ea26-7b7c-b113-c89ac2788773".into(),
            users: vec![],
            posts: vec![],
            comments: vec![sample_comment(42)],
            media: vec![],
            checkpoint,
            processed_at: now(),
        };

        // 首次提交：Applied，评论 + checkpoint + 事件表全部落库。
        let outcome = plan.execute(&db).await.unwrap();
        assert_eq!(outcome, transactional::CommitOutcome::Applied);
        assert!(get_comment(&db, 42).await.unwrap().is_some());
        let cp = get_sync_checkpoint(&db, "user:123:posts")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cp.fetched_count, 1);

        // 重复提交：Duplicate，不产生重复数据。
        let outcome2 = plan.execute(&db).await.unwrap();
        assert_eq!(outcome2, transactional::CommitOutcome::Duplicate);
        assert_eq!(get_comments_by_post(&db, 9001).await.unwrap().len(), 1);
        assert!(
            event_already_processed(&db, "019fbbd7-ea26-7b7c-b113-c89ac2788773")
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn test_prune_processed_events() {
        let db = setup_db().await;
        for i in 0..5 {
            record_processed_event(
                &db,
                &ProcessedEventDto {
                    id: 0,
                    event_id: format!("event-{i}"),
                    stream: None,
                    sequence: None,
                    request_id: None,
                    processed_at: now(),
                },
            )
            .await
            .unwrap();
        }
        let removed = prune_processed_events(&db, 2).await.unwrap();
        assert_eq!(removed, 3);
        assert!(!event_already_processed(&db, "event-0").await.unwrap());
        assert!(event_already_processed(&db, "event-3").await.unwrap());
        assert!(!event_already_processed(&db, "event-2").await.unwrap());
    }

    #[tokio::test]
    async fn test_transactional_commit_writes_post_and_comment_atomically() {
        use crate::storage::internal::post::PostInternal;

        let db = setup_db().await;
        let post = PostInternal {
            attitudes_count: None,
            attitudes_status: 0,
            comments_count: None,
            created_at: now(),
            deleted: false,
            edit_count: None,
            favorited: false,
            geo: None,
            id: 9001,
            mblogid: "Mb123".into(),
            mix_media_ids: None,
            mix_media_info: None,
            page_info: None,
            pic_ids: None,
            pic_infos: None,
            pic_num: None,
            region_name: None,
            reposts_count: None,
            repost_type: None,
            retweeted_id: None,
            source: None,
            tag_struct: None,
            text: "post text".into(),
            uid: Some(10001),
            url_struct: None,
            bid: None,
            location: None,
            topic_ids: None,
            at_users: None,
            is_long_text: false,
            video_url: None,
            raw_data: None,
            content_status: "complete".into(),
            fetch_error: None,
            first_fetched_at: None,
            last_refreshed_at: None,
        };
        let checkpoint = SyncCheckpointDto {
            stream: "user:123:posts".into(),
            cursor_json: Some("{\"cursor\":{\"max_id\":\"p1_after\"}}".into()),
            fetched_count: 1,
            last_sequence: Some(1),
            updated_at: now(),
            job_id: None,
            run_id: None,
            generation: None,
            owner_token: None,
            owner: CheckpointOwner::AdHoc,
        };
        let plan = transactional::CommitPlan {
            request_id: Some("req-p1".into()),
            stream: "user:123:posts".into(),
            sequence: 1,
            event_id: "019fbbd7-ea26-7b7c-b113-c89ac2788999".into(),
            users: vec![],
            posts: vec![post.clone()],
            comments: vec![sample_comment(4242)],
            media: vec![],
            checkpoint,
            processed_at: now(),
        };
        let outcome = plan.execute(&db).await.unwrap();
        assert_eq!(outcome, transactional::CommitOutcome::Applied);

        // 帖子 + 评论 + checkpoint 全部落库。
        let fetched: String = sqlx::query_scalar("SELECT text FROM posts WHERE id = 9001")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(fetched, "post text");
        assert!(get_comment(&db, 4242).await.unwrap().is_some());
        let cp = get_sync_checkpoint(&db, "user:123:posts")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cp.fetched_count, 1);
        let _ = post;
    }

    #[tokio::test]
    async fn test_transactional_collection_preserves_richer_existing_data() {
        use crate::models::User;
        use crate::storage::internal::{post, user};
        use url::Url;

        let db = setup_db().await;
        let existing_user = User {
            id: 10001,
            screen_name: "完整用户名".into(),
            domain: "complete-domain".into(),
            avatar_hd: Url::parse("https://example.com/hd.jpg").unwrap(),
            avatar_large: Url::parse("https://example.com/large.jpg").unwrap(),
            profile_image_url: Url::parse("https://example.com/profile.jpg").unwrap(),
            following: true,
            follow_me: true,
        };
        user::save_user(&db, &existing_user).await.unwrap();

        let mut existing_post = sample_post(9001);
        existing_post.text = "完整长文".into();
        existing_post.created_at = "2026-08-01T00:00:00Z".into();
        existing_post.mblogid = "complete-id".into();
        existing_post.pic_infos = Some(serde_json::json!({"p1": {"url": "x"}}));
        existing_post.content_status = "complete".into();
        post::save_post(&db, &existing_post).await.unwrap();

        let sparse_user = User {
            id: 10001,
            screen_name: String::new(),
            domain: String::new(),
            avatar_hd: Url::parse("https://example.invalid/").unwrap(),
            avatar_large: Url::parse("https://example.invalid/").unwrap(),
            profile_image_url: Url::parse("https://example.invalid/").unwrap(),
            following: false,
            follow_me: false,
        };
        let mut sparse_post = sample_post(9001);
        sparse_post.text = String::new();
        sparse_post.created_at = String::new();
        sparse_post.mblogid = String::new();
        sparse_post.pic_infos = None;
        sparse_post.content_status = "partial".into();

        let plan = transactional::CommitPlan {
            request_id: Some("req-preserve".into()),
            stream: "user:10001:posts".into(),
            sequence: 1,
            event_id: "019fbbd7-ea26-7b7c-b113-c89ac2788111".into(),
            users: vec![sparse_user],
            posts: vec![sparse_post],
            comments: vec![],
            media: vec![],
            checkpoint: SyncCheckpointDto {
                stream: "user:10001:posts".into(),
                cursor_json: Some("{\"cursor\":{\"max_id\":\"0\"}}".into()),
                fetched_count: 1,
                last_sequence: Some(1),
                updated_at: now(),
                job_id: None,
                run_id: None,
                generation: None,
                owner_token: None,
                owner: CheckpointOwner::AdHoc,
            },
            processed_at: now(),
        };
        plan.execute(&db).await.unwrap();

        let restored_user = user::get_user(&db, 10001).await.unwrap().unwrap();
        assert_eq!(restored_user, existing_user);
        let restored_post = post::get_post(&db, 9001).await.unwrap().unwrap();
        assert_eq!(restored_post.text, "完整长文");
        assert_eq!(restored_post.created_at, "2026-08-01T00:00:00Z");
        assert_eq!(restored_post.mblogid, "complete-id");
        assert_eq!(restored_post.pic_infos, existing_post.pic_infos);
        assert_eq!(restored_post.content_status, "complete");
    }
}
