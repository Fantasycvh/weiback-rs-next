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
    Uid,
    ScreenName,
    RefreshStrategy,
    Enabled,
    LastRefreshedAt,
    CreatedAt,
    UpdatedAt,
}

#[derive(Iden)]
#[iden = "sync_jobs"]
pub enum SyncJobIden {
    Table,
    Id,
    Name,
    Kind,
    Priority,
    ScheduleConfig,
    Enabled,
    CreatedAt,
    UpdatedAt,
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
    pub uid: i64,
    pub screen_name: Option<String>,
    pub refresh_strategy: String,
    pub enabled: bool,
    pub last_refreshed_at: Option<String>,
    pub created_at: String,
    pub updated_at: Option<String>,
}

/// 同步任务 DTO。
#[derive(Debug, Clone, PartialEq, FromRow, Serialize, Deserialize)]
pub struct SyncJobDto {
    pub id: i64,
    pub name: String,
    pub kind: String,
    pub priority: i64,
    pub schedule_config: Option<String>,
    pub enabled: bool,
    pub created_at: String,
    pub updated_at: Option<String>,
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
}

/// 同步 checkpoint DTO。
#[derive(Debug, Clone, PartialEq, FromRow, Serialize, Deserialize)]
pub struct SyncCheckpointDto {
    pub stream: String,
    pub cursor_json: Option<String>,
    pub fetched_count: i64,
    pub last_sequence: Option<i64>,
    pub updated_at: String,
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
            MonitoredUserIden::Uid,
            MonitoredUserIden::ScreenName,
            MonitoredUserIden::RefreshStrategy,
            MonitoredUserIden::Enabled,
            MonitoredUserIden::LastRefreshedAt,
            MonitoredUserIden::CreatedAt,
            MonitoredUserIden::UpdatedAt,
        ])
        .values([
            user.uid.into(),
            user.screen_name.clone().into(),
            user.refresh_strategy.clone().into(),
            user.enabled.into(),
            user.last_refreshed_at.clone().into(),
            user.created_at.clone().into(),
            user.updated_at.clone().into(),
        ])?
        .on_conflict(
            OnConflict::column(MonitoredUserIden::Uid)
                .update_columns([
                    MonitoredUserIden::ScreenName,
                    MonitoredUserIden::RefreshStrategy,
                    MonitoredUserIden::Enabled,
                    MonitoredUserIden::LastRefreshedAt,
                    MonitoredUserIden::UpdatedAt,
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
            MonitoredUserIden::Uid,
            MonitoredUserIden::ScreenName,
            MonitoredUserIden::RefreshStrategy,
            MonitoredUserIden::Enabled,
            MonitoredUserIden::LastRefreshedAt,
            MonitoredUserIden::CreatedAt,
            MonitoredUserIden::UpdatedAt,
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

/// 保存同步任务（id=0 时按自增插入）。
pub async fn save_sync_job<'c, A>(acquirer: A, job: &SyncJobDto) -> Result<i64>
where
    A: Acquire<'c, Database = Sqlite>,
{
    let mut conn = acquirer.acquire().await?;
    let (sql, values) = Query::insert()
        .into_table(SyncJobIden::Table)
        .columns([
            SyncJobIden::Name,
            SyncJobIden::Kind,
            SyncJobIden::Priority,
            SyncJobIden::ScheduleConfig,
            SyncJobIden::Enabled,
            SyncJobIden::CreatedAt,
            SyncJobIden::UpdatedAt,
        ])
        .values([
            job.name.clone().into(),
            job.kind.clone().into(),
            job.priority.into(),
            job.schedule_config.clone().into(),
            job.enabled.into(),
            job.created_at.clone().into(),
            job.updated_at.clone().into(),
        ])?
        .build_sqlx(SqliteQueryBuilder);
    let result = sqlx::query_with(&sql, values).execute(&mut *conn).await?;
    Ok(result.last_insert_rowid())
}

/// 读取全部同步任务。
pub async fn get_sync_jobs<'e, E>(executor: E) -> Result<Vec<SyncJobDto>>
where
    E: Executor<'e, Database = Sqlite>,
{
    let (sql, values) = Query::select()
        .columns([
            SyncJobIden::Id,
            SyncJobIden::Name,
            SyncJobIden::Kind,
            SyncJobIden::Priority,
            SyncJobIden::ScheduleConfig,
            SyncJobIden::Enabled,
            SyncJobIden::CreatedAt,
            SyncJobIden::UpdatedAt,
        ])
        .from(SyncJobIden::Table)
        .order_by(SyncJobIden::Priority, sea_query::Order::Desc)
        .build_sqlx(SqliteQueryBuilder);
    Ok(sqlx::query_as_with::<Sqlite, SyncJobDto, _>(&sql, values)
        .fetch_all(executor)
        .await?)
}

/// 保存同步运行记录。
pub async fn save_sync_run<'c, A>(acquirer: A, run: &SyncRunDto) -> Result<i64>
where
    A: Acquire<'c, Database = Sqlite>,
{
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
        ])
        .values([
            run.job_id.into(),
            run.status.clone().into(),
            run.started_at.clone().into(),
            run.finished_at.clone().into(),
            run.stats_json.clone().into(),
            run.error.clone().into(),
        ])?
        .build_sqlx(SqliteQueryBuilder);
    let result = sqlx::query_with(&sql, values).execute(&mut *conn).await?;
    Ok(result.last_insert_rowid())
}

/// 保存同步 checkpoint（按 stream 幂等 upsert）。
pub async fn save_sync_checkpoint<'c, A>(acquirer: A, checkpoint: &SyncCheckpointDto) -> Result<()>
where
    A: Acquire<'c, Database = Sqlite>,
{
    let mut conn = acquirer.acquire().await?;
    let (sql, values) = Query::insert()
        .into_table(SyncCheckpointIden::Table)
        .columns([
            SyncCheckpointIden::Stream,
            SyncCheckpointIden::CursorJson,
            SyncCheckpointIden::FetchedCount,
            SyncCheckpointIden::LastSequence,
            SyncCheckpointIden::UpdatedAt,
        ])
        .values([
            checkpoint.stream.clone().into(),
            checkpoint.cursor_json.clone().into(),
            checkpoint.fetched_count.into(),
            checkpoint.last_sequence.into(),
            checkpoint.updated_at.clone().into(),
        ])?
        .on_conflict(
            OnConflict::column(SyncCheckpointIden::Stream)
                .update_columns([
                    SyncCheckpointIden::CursorJson,
                    SyncCheckpointIden::FetchedCount,
                    SyncCheckpointIden::LastSequence,
                    SyncCheckpointIden::UpdatedAt,
                ])
                .to_owned(),
        )
        .build_sqlx(SqliteQueryBuilder);
    sqlx::query_with(&sql, values).execute(&mut *conn).await?;
    Ok(())
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
        ])
        .from(SyncCheckpointIden::Table)
        .and_where(Expr::col(SyncCheckpointIden::Stream).eq(stream))
        .build_sqlx(SqliteQueryBuilder);
    Ok(
        sqlx::query_as_with::<Sqlite, SyncCheckpointDto, _>(&sql, values)
            .fetch_optional(executor)
            .await?,
    )
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
        pub async fn execute<'c, A>(&self, acquirer: A) -> Result<CommitOutcome>
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
            save_sync_checkpoint(&mut *tx, &self.checkpoint).await?;

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
        let user = MonitoredUserDto {
            uid: 10001,
            screen_name: Some("小明".into()),
            refresh_strategy: "daily".into(),
            enabled: true,
            last_refreshed_at: None,
            created_at: now(),
            updated_at: None,
        };
        save_monitored_user(&db, &user).await.unwrap();
        let enabled = get_enabled_monitored_users(&db).await.unwrap();
        assert_eq!(enabled.len(), 1);
        assert_eq!(enabled[0].screen_name.as_deref(), Some("小明"));
    }

    #[tokio::test]
    async fn test_sync_job_and_run_roundtrip() {
        let db = setup_db().await;
        let job = SyncJobDto {
            id: 0,
            name: "用户帖子增量".into(),
            kind: "collect_user_posts".into(),
            priority: 1,
            schedule_config: Some("{\"interval\":\"6h\"}".into()),
            enabled: true,
            created_at: now(),
            updated_at: None,
        };
        let job_id = save_sync_job(&db, &job).await.unwrap();
        assert!(job_id > 0);

        let run = SyncRunDto {
            id: 0,
            job_id,
            status: "running".into(),
            started_at: now(),
            finished_at: None,
            stats_json: None,
            error: None,
        };
        let run_id = save_sync_run(&db, &run).await.unwrap();
        assert!(run_id > 0);

        let jobs = get_sync_jobs(&db).await.unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].name, "用户帖子增量");
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
        cp2.cursor_json = Some("{\"cursor\":{\"max_id\":\"p2_after\"}}".into());
        save_sync_checkpoint(&db, &cp2).await.unwrap();
        let all: Vec<SyncCheckpointDto> = sqlx::query_as("SELECT stream, cursor_json, fetched_count, last_sequence, updated_at FROM sync_checkpoints")
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
