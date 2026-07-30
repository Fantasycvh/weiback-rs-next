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
    favorited INTEGER DEFAULT 0
);

CREATE TABLE IF NOT EXISTS comments (
    id TEXT PRIMARY KEY,
    post_id INTEGER NOT NULL,
    user_id TEXT,
    user_screen_name TEXT,
    text TEXT,
    created_at TEXT,
    like_count INTEGER DEFAULT 0,
    reply_id TEXT
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
    synced_at TEXT DEFAULT (datetime('now'))
);
"""

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
WHERE csp.post_id IS NULL
ORDER BY p.id DESC
LIMIT ?
"""

INSERT_COMMENTS_PROGRESS = """
INSERT OR IGNORE INTO comments_sync_progress (post_id) VALUES (?)
"""
