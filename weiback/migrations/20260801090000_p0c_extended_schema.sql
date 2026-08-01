-- P0-C: 帖子扩展字段 + 新实体表
-- 迁移前备份在 Rust 侧 create_db_pool_with_url 中执行（VACUUM INTO）。

-- 6.2 帖子扩展字段（SQLite 逐列 ALTER）
ALTER TABLE posts ADD COLUMN bid TEXT;
ALTER TABLE posts ADD COLUMN location TEXT;
ALTER TABLE posts ADD COLUMN topic_ids TEXT;
ALTER TABLE posts ADD COLUMN at_users TEXT;
ALTER TABLE posts ADD COLUMN is_long_text INTEGER NOT NULL DEFAULT 0;
ALTER TABLE posts ADD COLUMN video_url TEXT;
ALTER TABLE posts ADD COLUMN raw_data TEXT;
ALTER TABLE posts ADD COLUMN content_status TEXT NOT NULL DEFAULT 'complete';
ALTER TABLE posts ADD COLUMN fetch_error TEXT;
ALTER TABLE posts ADD COLUMN first_fetched_at TEXT;
ALTER TABLE posts ADD COLUMN last_refreshed_at TEXT;

-- 6.3 新实体
CREATE TABLE comments (
    id INTEGER PRIMARY KEY,
    post_id INTEGER NOT NULL,
    root_id INTEGER,
    parent_id INTEGER,
    user_id INTEGER,
    text TEXT NOT NULL,
    created_at TEXT NOT NULL,
    depth INTEGER NOT NULL DEFAULT 0,
    child_count INTEGER NOT NULL DEFAULT 0,
    like_count INTEGER NOT NULL DEFAULT 0,
    source TEXT,
    media_json TEXT,
    raw_data TEXT,
    content_status TEXT NOT NULL DEFAULT 'complete',
    deleted INTEGER NOT NULL DEFAULT 0,
    first_fetched_at TEXT,
    last_refreshed_at TEXT
);
CREATE INDEX idx_comments_post_id ON comments(post_id);
CREATE INDEX idx_comments_parent_id ON comments(parent_id);
CREATE INDEX idx_comments_root_id ON comments(root_id);

CREATE TABLE media (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    owner_type TEXT NOT NULL,
    owner_id INTEGER,
    media_type TEXT NOT NULL,
    url TEXT NOT NULL UNIQUE,
    local_path TEXT,
    status TEXT NOT NULL DEFAULT 'pending',
    retry_count INTEGER NOT NULL DEFAULT 0,
    last_error TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT
);
CREATE INDEX idx_media_owner ON media(owner_type, owner_id);
CREATE INDEX idx_media_status ON media(status);

CREATE TABLE monitored_users (
    uid INTEGER PRIMARY KEY,
    screen_name TEXT,
    refresh_strategy TEXT NOT NULL DEFAULT 'manual',
    enabled INTEGER NOT NULL DEFAULT 1,
    last_refreshed_at TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT
);

CREATE TABLE sync_jobs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    kind TEXT NOT NULL,
    priority INTEGER NOT NULL DEFAULT 0,
    schedule_config TEXT,
    enabled INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL,
    updated_at TEXT
);

CREATE TABLE sync_runs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    job_id INTEGER NOT NULL,
    status TEXT NOT NULL,
    started_at TEXT NOT NULL,
    finished_at TEXT,
    stats_json TEXT,
    error TEXT
);
CREATE INDEX idx_sync_runs_job_id ON sync_runs(job_id);

CREATE TABLE sync_checkpoints (
    stream TEXT PRIMARY KEY,
    cursor_json TEXT,
    fetched_count INTEGER NOT NULL DEFAULT 0,
    last_sequence INTEGER,
    updated_at TEXT NOT NULL
);

CREATE TABLE processed_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    event_id TEXT NOT NULL UNIQUE,
    stream TEXT,
    sequence INTEGER,
    request_id TEXT,
    processed_at TEXT NOT NULL
);
CREATE INDEX idx_processed_events_stream ON processed_events(stream);
