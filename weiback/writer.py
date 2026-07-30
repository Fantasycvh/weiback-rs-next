import sqlite3
import logging

from . import models

logger = logging.getLogger(__name__)


def connect(db_path: str) -> sqlite3.Connection:
    conn = sqlite3.connect(db_path, timeout=5.0, check_same_thread=False)
    conn.execute("PRAGMA journal_mode=WAL")
    conn.execute("PRAGMA synchronous=NORMAL")
    conn.execute("PRAGMA busy_timeout=5000")
    conn.execute("PRAGMA foreign_keys=ON")
    conn.row_factory = sqlite3.Row
    conn.executescript(models.SCHEMA_SQL)
    conn.commit()
    return conn


def save_user(conn: sqlite3.Connection, user_data: dict):
    conn.execute(
        """INSERT OR REPLACE INTO users
        (id, screen_name, avatar_hd, avatar_large, profile_image_url, domain, following, follow_me)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?)""",
        (
            user_data["id"],
            user_data["screen_name"],
            user_data.get("avatar_hd"),
            user_data.get("avatar_large"),
            user_data.get("profile_image_url"),
            user_data.get("domain"),
            user_data.get("following", False),
            user_data.get("follow_me", False),
        ),
    )


def save_posts(conn: sqlite3.Connection, posts: list[dict]):
    for p in posts:
        conn.execute(
            """INSERT OR REPLACE INTO posts (
            id, uid, text, created_at, attitudes_count, attitudes_status,
            comments_count, reposts_count, repost_type, retweeted_id,
            pic_ids, pic_infos, pic_num, geo, mblogid, mix_media_ids,
            mix_media_info, page_info, region_name, source, tag_struct,
            url_struct, deleted, edit_count, favorited
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)""",
            (
                p["id"],
                p["uid"],
                p["text"],
                p["created_at"],
                p.get("attitudes_count", 0),
                p.get("attitudes_status", 0),
                p.get("comments_count", 0),
                p.get("reposts_count", 0),
                p.get("repost_type", 0),
                p.get("retweeted_id"),
                p.get("pic_ids"),
                p.get("pic_infos"),
                p.get("pic_num", 0),
                p.get("geo"),
                p.get("mblogid"),
                p.get("mix_media_ids"),
                p.get("mix_media_info"),
                p.get("page_info"),
                p.get("region_name"),
                p.get("source"),
                p.get("tag_struct"),
                p.get("url_struct"),
                False,
                0,
                False,
            ),
        )


def save_comments(conn: sqlite3.Connection, comments: list[dict]):
    for c in comments:
        conn.execute(
            """INSERT OR REPLACE INTO comments
            (id, post_id, user_id, user_screen_name, text, created_at, like_count, reply_id)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?)""",
            (
                c["id"],
                c["post_id"],
                c.get("user_id"),
                c.get("user_screen_name"),
                c.get("text"),
                c.get("created_at"),
                c.get("like_count", 0),
                c.get("reply_id"),
            ),
        )


def save_pictures(conn: sqlite3.Connection, post_id: int, urls: list[str], user_id: int | None = None):
    for url in urls:
        pic_id = url.rsplit("/", 1)[-1].rsplit(".", 1)[0]
        db_key = url.split("?")[0]
        conn.execute(
            """INSERT OR REPLACE INTO picture (id, url, definition, path, post_id, user_id)
            VALUES (?, ?, ?, ?, ?, ?)""",
            (pic_id, db_key, "large", "", post_id, user_id),
        )


def save_video(conn: sqlite3.Connection, url: str, path: str, post_id: int):
    conn.execute(
        "INSERT OR REPLACE INTO video (url, path, post_id) VALUES (?, ?, ?)",
        (url, path, post_id),
    )


def get_last_post_id(conn: sqlite3.Connection, uid: int) -> int:
    row = conn.execute(
        "SELECT MAX(id) FROM posts WHERE uid=?", (uid,)
    ).fetchone()
    return row[0] or 0


def add_monitored_user(conn: sqlite3.Connection, uid: str, screen_name: str = ""):
    conn.execute(models.INSERT_MONITORED_USER, (uid, screen_name))
    conn.commit()


def remove_monitored_user(conn: sqlite3.Connection, uid: str):
    conn.execute(models.DELETE_MONITORED_USER, (uid,))
    conn.commit()


def get_monitored_users(conn: sqlite3.Connection) -> list[dict]:
    return [dict(r) for r in conn.execute(models.SELECT_MONITORED_USERS).fetchall()]


def get_all_monitored_users(conn: sqlite3.Connection) -> list[dict]:
    return [dict(r) for r in conn.execute(models.SELECT_ALL_MONITORED_USERS).fetchall()]


def write_sync_history(conn: sqlite3.Connection, uid: str, sync_time: str, new_count: int, status: str = "success", error: str | None = None):
    conn.execute(models.INSERT_SYNC_HISTORY, (uid, sync_time, new_count, status, error))
    conn.commit()


def get_sync_status(conn: sqlite3.Connection) -> dict:
    latest = conn.execute(models.SELECT_LATEST_SYNC).fetchone()
    summary = conn.execute(models.SELECT_SYNC_SUMMARY).fetchone()
    stats_row = conn.execute("SELECT COUNT(*) as total FROM posts").fetchone()
    user_row = conn.execute("SELECT COUNT(*) as total FROM monitored_users WHERE is_active=1").fetchone()
    return {
        "latest": dict(latest) if latest else None,
        "total_syncs": summary[0] if summary else 0,
        "total_posts": summary[1] if summary else 0,
        "posts_count": stats_row[0] if stats_row else 0,
        "watched_users": user_row[0] if user_row else 0,
    }


def get_uncommented_posts(conn: sqlite3.Connection, limit: int = 50) -> list[dict]:
    return [dict(r) for r in conn.execute(models.SELECT_UNCOMMENTED_POSTS, (limit,)).fetchall()]


def mark_comments_synced(conn: sqlite3.Connection, post_id: int):
    conn.execute(models.INSERT_COMMENTS_PROGRESS, (post_id,))
