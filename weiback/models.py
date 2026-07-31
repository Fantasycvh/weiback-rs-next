SCHEMA_VERSION = 2


SCHEMA_SQL = """
CREATE TABLE IF NOT EXISTS users (
    id INTEGER PRIMARY KEY,
    screen_name TEXT,
    avatar_hd TEXT,
    avatar_large TEXT,
    profile_image_url TEXT,
    domain TEXT,
    following INTEGER DEFAULT 0,
    follow_me INTEGER DEFAULT 0
);

CREATE TABLE IF NOT EXISTS posts (
    id INTEGER PRIMARY KEY,
    uid INTEGER NOT NULL,
    text TEXT NOT NULL DEFAULT '',
    created_at TEXT,
    attitudes_count INTEGER DEFAULT 0,
    attitudes_status INTEGER DEFAULT 0,
    comments_count INTEGER DEFAULT 0,
    reposts_count INTEGER DEFAULT 0,
    repost_type INTEGER DEFAULT 0,
    retweeted_id INTEGER,
    pic_ids TEXT,
    pic_infos TEXT,
    pic_num INTEGER DEFAULT 0,
    geo TEXT,
    mblogid TEXT,
    mix_media_ids TEXT,
    mix_media_info TEXT,
    page_info TEXT,
    region_name TEXT,
    source TEXT,
    tag_struct TEXT,
    url_struct TEXT,
    deleted INTEGER DEFAULT 0,
    edit_count INTEGER DEFAULT 0,
    favorited INTEGER DEFAULT 0,
    bid TEXT,
    location TEXT,
    topic_ids TEXT,
    at_users TEXT,
    is_long_text INTEGER DEFAULT 0,
    video_url TEXT,
    raw_data TEXT,
    content_status TEXT NOT NULL DEFAULT 'partial',
    fetch_error TEXT,
    first_fetched_at TEXT,
    last_refreshed_at TEXT
);

CREATE TABLE IF NOT EXISTS comments (
    id TEXT PRIMARY KEY,
    post_id INTEGER NOT NULL,
    user_id TEXT,
    user_screen_name TEXT,
    text TEXT,
    created_at TEXT,
    like_count INTEGER DEFAULT 0,
    reply_id TEXT,
    root_id TEXT,
    parent_id TEXT,
    depth INTEGER NOT NULL DEFAULT 0,
    source TEXT,
    user_avatar_url TEXT,
    user_verified INTEGER DEFAULT 0,
    liked INTEGER DEFAULT 0,
    reply_text TEXT,
    pic_url TEXT,
    child_count INTEGER DEFAULT 0,
    raw_data TEXT,
    last_refreshed_at TEXT
);

CREATE INDEX IF NOT EXISTS idx_comments_post_id ON comments(post_id);

CREATE TABLE IF NOT EXISTS picture (
    id TEXT,
    url TEXT,
    definition TEXT,
    path TEXT DEFAULT '',
    post_id INTEGER,
    user_id INTEGER
);

CREATE TABLE IF NOT EXISTS video (
    url TEXT,
    path TEXT,
    post_id INTEGER
);

CREATE TABLE IF NOT EXISTS monitored_users (
    uid TEXT PRIMARY KEY,
    screen_name TEXT,
    is_active INTEGER DEFAULT 1,
    added_at TEXT DEFAULT (datetime('now')),
    last_sync_at TEXT
);

CREATE TABLE IF NOT EXISTS sync_history (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    uid TEXT NOT NULL,
    sync_time TEXT NOT NULL,
    new_posts_count INTEGER DEFAULT 0,
    status TEXT NOT NULL DEFAULT 'success',
    error_message TEXT
);

CREATE INDEX IF NOT EXISTS idx_sync_history_uid ON sync_history(uid);

CREATE TABLE IF NOT EXISTS comments_sync_progress (
    post_id INTEGER PRIMARY KEY,
    status TEXT NOT NULL DEFAULT 'pending',
    cursor TEXT,
    fetched_count INTEGER DEFAULT 0,
    error_message TEXT,
    synced_at TEXT,
    updated_at TEXT DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS comment_reply_progress (
    root_comment_id TEXT PRIMARY KEY,
    post_id INTEGER NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',
    max_id TEXT DEFAULT '0',
    max_id_type INTEGER DEFAULT 0,
    fetched_count INTEGER DEFAULT 0,
    error_message TEXT,
    updated_at TEXT DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_reply_progress_post_id
ON comment_reply_progress(post_id);

CREATE TABLE IF NOT EXISTS media (
    id TEXT PRIMARY KEY,
    owner_type TEXT NOT NULL,
    owner_id TEXT NOT NULL,
    post_id INTEGER,
    user_id TEXT,
    media_type TEXT NOT NULL,
    url TEXT NOT NULL,
    path TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL DEFAULT 'pending',
    error_message TEXT,
    retry_count INTEGER NOT NULL DEFAULT 0,
    created_at TEXT DEFAULT (datetime('now')),
    updated_at TEXT DEFAULT (datetime('now')),
    UNIQUE(owner_type, owner_id, media_type, url)
);

CREATE INDEX IF NOT EXISTS idx_media_post_id ON media(post_id);
CREATE INDEX IF NOT EXISTS idx_media_owner ON media(owner_type, owner_id);
"""


POST_MIGRATION_COLUMNS = {
    "bid": "TEXT",
    "location": "TEXT",
    "topic_ids": "TEXT",
    "at_users": "TEXT",
    "is_long_text": "INTEGER DEFAULT 0",
    "video_url": "TEXT",
    "raw_data": "TEXT",
    "content_status": "TEXT NOT NULL DEFAULT 'partial'",
    "fetch_error": "TEXT",
    "first_fetched_at": "TEXT",
    "last_refreshed_at": "TEXT",
}

COMMENT_MIGRATION_COLUMNS = {
    "root_id": "TEXT",
    "parent_id": "TEXT",
    "depth": "INTEGER NOT NULL DEFAULT 0",
    "source": "TEXT",
    "user_avatar_url": "TEXT",
    "user_verified": "INTEGER DEFAULT 0",
    "liked": "INTEGER DEFAULT 0",
    "reply_text": "TEXT",
    "pic_url": "TEXT",
    "child_count": "INTEGER DEFAULT 0",
    "raw_data": "TEXT",
    "last_refreshed_at": "TEXT",
}

COMMENT_PROGRESS_MIGRATION_COLUMNS = {
    "status": "TEXT NOT NULL DEFAULT 'pending'",
    "cursor": "TEXT",
    "fetched_count": "INTEGER DEFAULT 0",
    "error_message": "TEXT",
    "updated_at": "TEXT",
}

INSERT_MONITORED_USER = """
INSERT OR IGNORE INTO monitored_users (uid, screen_name)
VALUES (?, ?)
"""

UPDATE_MONITORED_USER = """
UPDATE monitored_users SET screen_name=?, last_sync_at=? WHERE uid=?
"""

SELECT_MONITORED_USERS = """
SELECT uid, screen_name, is_active, added_at, last_sync_at
FROM monitored_users WHERE is_active = 1
ORDER BY added_at
"""

SELECT_ALL_MONITORED_USERS = """
SELECT uid, screen_name, is_active, added_at, last_sync_at
FROM monitored_users ORDER BY added_at
"""

DELETE_MONITORED_USER = "DELETE FROM monitored_users WHERE uid=?"


INSERT_SYNC_HISTORY = """
INSERT INTO sync_history (uid, sync_time, new_posts_count, status, error_message)
VALUES (?, ?, ?, ?, ?)
"""

SELECT_LATEST_SYNC = """
SELECT uid, sync_time, new_posts_count, status, error_message
FROM sync_history ORDER BY id DESC LIMIT 1
"""

SELECT_SYNC_SUMMARY = """
SELECT COUNT(*) as total_syncs, COALESCE(SUM(new_posts_count), 0) as total_posts
FROM sync_history
"""

SELECT_HISTORY_BY_USER = """
SELECT sync_time, new_posts_count, status, error_message
FROM sync_history WHERE uid=? ORDER BY id DESC LIMIT 20
"""


SELECT_UNCOMMENTED_POSTS = """
SELECT p.id, p.uid
FROM posts p
LEFT JOIN comments_sync_progress csp ON p.id = csp.post_id
WHERE csp.post_id IS NULL OR csp.status != 'complete'
ORDER BY p.id DESC
LIMIT ?
"""

INSERT_COMMENTS_PROGRESS = """
INSERT INTO comments_sync_progress (post_id, status, synced_at, updated_at)
VALUES (?, 'complete', datetime('now'), datetime('now'))
ON CONFLICT(post_id) DO UPDATE SET
    status='complete', error_message=NULL,
    synced_at=datetime('now'), updated_at=datetime('now')
"""


SELECT_PICTURES_WITHOUT_PATH = """
SELECT id, url, post_id, user_id
FROM picture
WHERE path IS NULL OR path = ''
ORDER BY post_id DESC
LIMIT ?
"""

SELECT_PICTURES_WITHOUT_PATH_LIMITLESS = """
SELECT id, url, post_id, user_id
FROM picture
WHERE path IS NULL OR path = ''
ORDER BY post_id DESC
"""

UPDATE_PICTURE_PATH = """
UPDATE picture SET path=? WHERE url=?
"""
