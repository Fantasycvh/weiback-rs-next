import sqlite3
from pathlib import Path


def test_schema_executes_without_error():
    from weiback.models import SCHEMA_SQL
    conn = sqlite3.connect(":memory:")
    conn.executescript(SCHEMA_SQL)
    conn.commit()

    tables = conn.execute(
        "SELECT name FROM sqlite_master WHERE type='table' ORDER BY name"
    ).fetchall()
    names = [r[0] for r in tables]
    assert "users" in names
    assert "posts" in names
    assert "comments" in names
    assert "picture" in names
    assert "video" in names
    assert "monitored_users" in names
    assert "sync_history" in names
    assert "comments_sync_progress" in names
    conn.close()


def test_schema_posts_has_expected_columns():
    from weiback.models import SCHEMA_SQL
    conn = sqlite3.connect(":memory:")
    conn.executescript(SCHEMA_SQL)

    cols = conn.execute("PRAGMA table_info(posts)").fetchall()
    col_names = {r[1] for r in cols}
    for expected in ("id", "uid", "text", "created_at", "retweeted_id",
                     "pic_ids", "pic_infos", "region_name", "source"):
        assert expected in col_names, f"缺少列: {expected}"
    conn.close()


def test_schema_users_has_expected_columns():
    from weiback.models import SCHEMA_SQL
    conn = sqlite3.connect(":memory:")
    conn.executescript(SCHEMA_SQL)

    cols = conn.execute("PRAGMA table_info(users)").fetchall()
    col_names = {r[1] for r in cols}
    for expected in ("id", "screen_name", "avatar_hd", "domain", "following", "follow_me"):
        assert expected in col_names, f"缺少列: {expected}"
    conn.close()


def test_schema_monitored_users_columns():
    from weiback.models import SCHEMA_SQL
    conn = sqlite3.connect(":memory:")
    conn.executescript(SCHEMA_SQL)

    cols = conn.execute("PRAGMA table_info(monitored_users)").fetchall()
    col_names = {r[1] for r in cols}
    for expected in ("uid", "screen_name", "is_active", "added_at", "last_sync_at"):
        assert expected in col_names
    conn.close()


def test_connect_migrates_legacy_schema_idempotently(db_path):
    from weiback.models import SCHEMA_VERSION
    from weiback.writer import connect

    legacy = sqlite3.connect(db_path)
    legacy.executescript("""
        CREATE TABLE posts (
            id INTEGER PRIMARY KEY,
            uid INTEGER NOT NULL,
            text TEXT NOT NULL DEFAULT '',
            created_at TEXT,
            deleted INTEGER DEFAULT 0
        );
        CREATE TABLE comments (
            id TEXT PRIMARY KEY,
            post_id INTEGER NOT NULL,
            text TEXT
        );
        CREATE TABLE picture (
            id TEXT,
            url TEXT,
            definition TEXT,
            path TEXT DEFAULT '',
            post_id INTEGER,
            user_id INTEGER
        );
        INSERT INTO posts (id, uid, text, deleted) VALUES (1, 2, 'legacy', 1);
        INSERT INTO picture VALUES ('p1', 'https://example.com/p.jpg', 'large', '1/p.jpg', 1, 2);
        INSERT INTO picture VALUES ('p1', 'https://example.com/p.jpg', 'large', '', 1, 2);
    """)
    legacy.commit()
    legacy.close()

    conn = connect(db_path)
    backup_path = Path(f"{db_path}.pre-v{SCHEMA_VERSION}.bak")
    assert backup_path.exists()
    backup = sqlite3.connect(backup_path)
    assert backup.execute("SELECT text FROM posts WHERE id=1").fetchone()[0] == "legacy"
    assert "bid" not in {
        row[1] for row in backup.execute("PRAGMA table_info(posts)").fetchall()
    }
    backup.close()
    assert conn.execute("PRAGMA user_version").fetchone()[0] == SCHEMA_VERSION
    post_columns = {row[1] for row in conn.execute("PRAGMA table_info(posts)")}
    assert {"bid", "video_url", "raw_data", "content_status", "last_refreshed_at"} <= post_columns
    comment_columns = {row[1] for row in conn.execute("PRAGMA table_info(comments)")}
    assert {"root_id", "parent_id", "depth", "pic_url", "user_avatar_url"} <= comment_columns
    media = conn.execute(
        "SELECT url, path, media_type FROM media WHERE owner_type='post' AND owner_id='1'"
    ).fetchall()
    assert len(media) == 1
    assert media[0]["path"] == "1/p.jpg"
    assert media[0]["media_type"] == "image"
    assert conn.execute("SELECT deleted FROM posts WHERE id=1").fetchone()[0] == 1
    conn.close()

    second = connect(db_path)
    assert second.execute("PRAGMA user_version").fetchone()[0] == SCHEMA_VERSION
    assert second.execute("SELECT COUNT(*) FROM media").fetchone()[0] == 1
    second.close()
    backup_path.unlink()
