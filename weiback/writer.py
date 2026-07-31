import sqlite3
import logging
import hashlib
from datetime import datetime, timezone
from pathlib import Path

from . import models

logger = logging.getLogger(__name__)


def connect(db_path: str) -> sqlite3.Connection:
    conn = sqlite3.connect(db_path, timeout=5.0, check_same_thread=False)
    conn.execute("PRAGMA journal_mode=WAL")
    conn.execute("PRAGMA synchronous=NORMAL")
    conn.execute("PRAGMA busy_timeout=5000")
    conn.execute("PRAGMA foreign_keys=ON")
    conn.row_factory = sqlite3.Row
    _backup_before_migration(conn, db_path)
    conn.executescript(models.SCHEMA_SQL)
    _migrate_schema(conn)
    conn.commit()
    return conn


def _backup_before_migration(conn: sqlite3.Connection, db_path: str) -> None:
    if db_path == ":memory:":
        return
    version = conn.execute("PRAGMA user_version").fetchone()[0]
    has_tables = conn.execute(
        "SELECT 1 FROM sqlite_master WHERE type='table' LIMIT 1"
    ).fetchone()
    if version >= models.SCHEMA_VERSION or has_tables is None:
        return

    backup_path = Path(f"{db_path}.pre-v{models.SCHEMA_VERSION}.bak")
    if backup_path.exists():
        return
    backup = sqlite3.connect(backup_path)
    try:
        conn.backup(backup)
    except Exception:
        backup.close()
        backup_path.unlink(missing_ok=True)
        raise
    else:
        backup.close()
        logger.info("数据库升级前备份已创建: %s", backup_path)


def _migrate_schema(conn: sqlite3.Connection) -> None:
    version = conn.execute("PRAGMA user_version").fetchone()[0]
    if version >= models.SCHEMA_VERSION:
        return

    _add_missing_columns(conn, "posts", models.POST_MIGRATION_COLUMNS)
    _add_missing_columns(conn, "comments", models.COMMENT_MIGRATION_COLUMNS)
    _add_missing_columns(
        conn,
        "comments_sync_progress",
        models.COMMENT_PROGRESS_MIGRATION_COLUMNS,
    )
    _deduplicate_legacy_pictures(conn)
    conn.execute(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_picture_post_url ON picture(post_id, url)"
    )
    _migrate_legacy_media(conn)
    conn.execute(f"PRAGMA user_version={models.SCHEMA_VERSION}")


def _add_missing_columns(
    conn: sqlite3.Connection, table: str, columns: dict[str, str]
) -> None:
    existing = {row[1] for row in conn.execute(f"PRAGMA table_info({table})")}
    for name, definition in columns.items():
        if name not in existing:
            conn.execute(f"ALTER TABLE {table} ADD COLUMN {name} {definition}")


def _deduplicate_legacy_pictures(conn: sqlite3.Connection) -> None:
    duplicates = conn.execute(
        """SELECT post_id, url, MAX(CASE WHEN path IS NOT NULL AND path != '' THEN path END) AS path
           FROM picture GROUP BY post_id, url HAVING COUNT(*) > 1"""
    ).fetchall()
    for row in duplicates:
        keep = conn.execute(
            "SELECT MIN(rowid) FROM picture WHERE post_id IS ? AND url IS ?",
            (row["post_id"], row["url"]),
        ).fetchone()[0]
        conn.execute(
            "UPDATE picture SET path=COALESCE(?, path) WHERE rowid=?",
            (row["path"], keep),
        )
        conn.execute(
            "DELETE FROM picture WHERE post_id IS ? AND url IS ? AND rowid != ?",
            (row["post_id"], row["url"], keep),
        )


def _migrate_legacy_media(conn: sqlite3.Connection) -> None:
    for row in conn.execute(
        "SELECT id, url, path, post_id, user_id FROM picture WHERE url IS NOT NULL"
    ).fetchall():
        save_media(
            conn,
            owner_type="post",
            owner_id=str(row["post_id"]),
            media_type="image",
            url=row["url"],
            post_id=row["post_id"],
            user_id=str(row["user_id"]) if row["user_id"] is not None else None,
            path=row["path"] or "",
        )
    for row in conn.execute(
        "SELECT url, path, post_id FROM video WHERE url IS NOT NULL"
    ).fetchall():
        save_media(
            conn,
            owner_type="post",
            owner_id=str(row["post_id"]),
            media_type="video",
            url=row["url"],
            post_id=row["post_id"],
            path=row["path"] or "",
        )


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
            """INSERT INTO posts (
            id, uid, text, created_at, attitudes_count, attitudes_status,
            comments_count, reposts_count, repost_type, retweeted_id,
            pic_ids, pic_infos, pic_num, geo, mblogid, mix_media_ids,
            mix_media_info, page_info, region_name, source, tag_struct,
            url_struct, deleted, edit_count, favorited, bid, location,
            topic_ids, at_users, is_long_text, video_url, raw_data,
            content_status, fetch_error, first_fetched_at, last_refreshed_at
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(id) DO UPDATE SET
                uid=excluded.uid, text=excluded.text, created_at=excluded.created_at,
                attitudes_count=excluded.attitudes_count,
                attitudes_status=excluded.attitudes_status,
                comments_count=excluded.comments_count,
                reposts_count=excluded.reposts_count,
                repost_type=excluded.repost_type,
                retweeted_id=excluded.retweeted_id,
                pic_ids=excluded.pic_ids, pic_infos=excluded.pic_infos,
                pic_num=excluded.pic_num, geo=excluded.geo,
                mblogid=excluded.mblogid, mix_media_ids=excluded.mix_media_ids,
                mix_media_info=excluded.mix_media_info,
                page_info=excluded.page_info, region_name=excluded.region_name,
                source=excluded.source, tag_struct=excluded.tag_struct,
                url_struct=excluded.url_struct, bid=excluded.bid,
                location=excluded.location, topic_ids=excluded.topic_ids,
                at_users=excluded.at_users, is_long_text=excluded.is_long_text,
                video_url=excluded.video_url, raw_data=excluded.raw_data,
                content_status=excluded.content_status,
                fetch_error=excluded.fetch_error,
                last_refreshed_at=excluded.last_refreshed_at""",
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
                p.get("bid"),
                p.get("location"),
                p.get("topic_ids"),
                p.get("at_users"),
                p.get("is_long_text", False),
                p.get("video_url"),
                p.get("raw_data"),
                p.get("content_status", "complete"),
                p.get("fetch_error"),
                p.get("first_fetched_at") or _utc_now(),
                p.get("last_refreshed_at") or _utc_now(),
            ),
        )


def save_comments(conn: sqlite3.Connection, comments: list[dict]):
    for c in comments:
        conn.execute(
            """INSERT INTO comments (
            id, post_id, user_id, user_screen_name, text, created_at,
            like_count, reply_id, root_id, parent_id, depth, source,
            user_avatar_url, user_verified, liked, reply_text, pic_url,
            child_count, raw_data, last_refreshed_at
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(id) DO UPDATE SET
                post_id=excluded.post_id, user_id=excluded.user_id,
                user_screen_name=excluded.user_screen_name, text=excluded.text,
                created_at=excluded.created_at, like_count=excluded.like_count,
                reply_id=excluded.reply_id, root_id=excluded.root_id,
                parent_id=excluded.parent_id, depth=excluded.depth,
                source=excluded.source,
                user_avatar_url=excluded.user_avatar_url,
                user_verified=excluded.user_verified, liked=excluded.liked,
                reply_text=excluded.reply_text, pic_url=excluded.pic_url,
                child_count=excluded.child_count, raw_data=excluded.raw_data,
                last_refreshed_at=excluded.last_refreshed_at""",
            (
                c["id"],
                c["post_id"],
                c.get("user_id"),
                c.get("user_screen_name"),
                c.get("text"),
                c.get("created_at"),
                c.get("like_count", 0),
                c.get("reply_id"),
                c.get("root_id") or c["id"],
                c.get("parent_id"),
                c.get("depth", 0),
                c.get("source"),
                c.get("user_avatar_url"),
                c.get("user_verified", False),
                c.get("liked", False),
                c.get("reply_text"),
                c.get("pic_url"),
                c.get("child_count", 0),
                c.get("raw_data"),
                c.get("last_refreshed_at") or _utc_now(),
            ),
        )
        user_id = c.get("user_id")
        avatar_url = c.get("user_avatar_url")
        if user_id is not None and avatar_url:
            save_media(
                conn,
                owner_type="user",
                owner_id=str(user_id),
                media_type="avatar",
                url=avatar_url,
                user_id=str(user_id),
            )
        if c.get("pic_url"):
            save_media(
                conn,
                owner_type="comment",
                owner_id=str(c["id"]),
                media_type="image",
                url=c["pic_url"],
                post_id=c["post_id"],
                user_id=str(user_id) if user_id is not None else None,
            )


def save_pictures(conn: sqlite3.Connection, post_id: int, urls: list[str], user_id: int | None = None):
    for url in urls:
        pic_id = url.rsplit("/", 1)[-1].rsplit(".", 1)[0]
        db_key = url.split("?")[0]
        conn.execute(
            """INSERT INTO picture (id, url, definition, path, post_id, user_id)
            VALUES (?, ?, ?, ?, ?, ?)
            ON CONFLICT(post_id, url) DO UPDATE SET
                id=excluded.id,
                definition=excluded.definition,
                user_id=COALESCE(excluded.user_id, picture.user_id)""",
            (pic_id, db_key, "large", "", post_id, user_id),
        )
        save_media(
            conn,
            owner_type="post",
            owner_id=str(post_id),
            media_type="image",
            url=db_key,
            post_id=post_id,
            user_id=str(user_id) if user_id is not None else None,
        )


def save_video(conn: sqlite3.Connection, url: str, path: str, post_id: int):
    cursor = conn.execute(
        """UPDATE video
           SET path=CASE WHEN ? != '' THEN ? ELSE path END
           WHERE post_id=? AND url=?""",
        (path, path, post_id, url),
    )
    if cursor.rowcount == 0:
        conn.execute(
            "INSERT INTO video (url, path, post_id) VALUES (?, ?, ?)",
            (url, path, post_id),
        )
    save_media(
        conn,
        owner_type="post",
        owner_id=str(post_id),
        media_type="video",
        url=url,
        post_id=post_id,
        path=path,
    )


def save_media(
    conn: sqlite3.Connection,
    *,
    owner_type: str,
    owner_id: str,
    media_type: str,
    url: str,
    post_id: int | None = None,
    user_id: str | None = None,
    path: str = "",
) -> None:
    media_id = hashlib.sha256(
        f"{owner_type}\0{owner_id}\0{media_type}\0{url}".encode("utf-8")
    ).hexdigest()
    conn.execute(
        """INSERT INTO media
           (id, owner_type, owner_id, post_id, user_id, media_type, url, path, status)
           VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
           ON CONFLICT(owner_type, owner_id, media_type, url) DO UPDATE SET
               post_id=COALESCE(excluded.post_id, media.post_id),
               user_id=COALESCE(excluded.user_id, media.user_id),
               path=CASE WHEN media.path != '' THEN media.path ELSE excluded.path END,
               status=CASE WHEN media.path != '' OR excluded.path != '' THEN 'complete' ELSE media.status END,
               updated_at=datetime('now')""",
        (
            media_id,
            owner_type,
            owner_id,
            post_id,
            user_id,
            media_type,
            url,
            path,
            "complete" if path else "pending",
        ),
    )


def _utc_now() -> str:
    return datetime.now(timezone.utc).isoformat()


def get_pictures_without_path(conn: sqlite3.Connection, limit: int | None = None) -> list[dict]:
    if limit is None:
        return [dict(r) for r in conn.execute(models.SELECT_PICTURES_WITHOUT_PATH_LIMITLESS).fetchall()]
    return [dict(r) for r in conn.execute(models.SELECT_PICTURES_WITHOUT_PATH, (limit,)).fetchall()]


def update_picture_path(conn: sqlite3.Connection, url: str, path: str):
    conn.execute(models.UPDATE_PICTURE_PATH, (path, url))


def get_pending_media(conn: sqlite3.Connection, limit: int | None = None) -> list[dict]:
    sql = """SELECT * FROM media
             WHERE media_type IN ('image', 'video', 'avatar')
               AND (path IS NULL OR path = '')
             ORDER BY created_at, id"""
    if limit is None:
        rows = conn.execute(sql).fetchall()
    else:
        rows = conn.execute(f"{sql} LIMIT ?", (limit,)).fetchall()
    return [dict(row) for row in rows]


def mark_media_complete(conn: sqlite3.Connection, media: dict, path: str) -> None:
    conn.execute(
        """UPDATE media
           SET path=?, status='complete', error_message=NULL, updated_at=datetime('now')
           WHERE id=?""",
        (path, media["id"]),
    )
    if media["media_type"] == "image" and media["post_id"] is not None:
        conn.execute(
            "UPDATE picture SET path=? WHERE post_id=? AND url=?",
            (path, media["post_id"], media["url"]),
        )


def mark_media_failed(conn: sqlite3.Connection, media_id: str, error: str) -> None:
    conn.execute(
        """UPDATE media
           SET status='failed', error_message=?, retry_count=retry_count + 1,
               updated_at=datetime('now')
           WHERE id=?""",
        (error, media_id),
    )


def get_last_post_id(conn: sqlite3.Connection, uid: int) -> int:
    row = conn.execute(
        "SELECT MAX(id) FROM posts WHERE uid=?", (uid,)
    ).fetchone()
    return row[0] or 0


def post_exists(conn: sqlite3.Connection, post_id: int) -> bool:
    row = conn.execute("SELECT 1 FROM posts WHERE id=?", (post_id,)).fetchone()
    return row is not None


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


def get_comments_progress(conn: sqlite3.Connection, post_id: int) -> dict | None:
    row = conn.execute(
        "SELECT * FROM comments_sync_progress WHERE post_id=?", (post_id,)
    ).fetchone()
    return dict(row) if row else None


def save_comments_progress(
    conn: sqlite3.Connection,
    *,
    post_id: int,
    status: str,
    cursor: str,
    fetched_count: int,
    error_message: str | None = None,
) -> None:
    synced_at = _utc_now() if status == "complete" else None
    conn.execute(
        """INSERT INTO comments_sync_progress
           (post_id, status, cursor, fetched_count, error_message,
            synced_at, updated_at)
           VALUES (?, ?, ?, ?, ?, ?, ?)
           ON CONFLICT(post_id) DO UPDATE SET
               status=excluded.status, cursor=excluded.cursor,
               fetched_count=excluded.fetched_count,
               error_message=excluded.error_message,
               synced_at=excluded.synced_at,
               updated_at=excluded.updated_at""",
        (
            post_id,
            status,
            str(cursor),
            fetched_count,
            error_message,
            synced_at,
            _utc_now(),
        ),
    )


def get_comment_reply_progress(
    conn: sqlite3.Connection, root_comment_id: str
) -> dict | None:
    row = conn.execute(
        "SELECT * FROM comment_reply_progress WHERE root_comment_id=?",
        (str(root_comment_id),),
    ).fetchone()
    return dict(row) if row else None


def save_comment_reply_progress(
    conn: sqlite3.Connection,
    *,
    root_comment_id: str,
    post_id: int,
    status: str,
    max_id: str,
    max_id_type: int,
    fetched_count: int,
    error_message: str | None = None,
) -> None:
    conn.execute(
        """INSERT INTO comment_reply_progress
           (root_comment_id, post_id, status, max_id, max_id_type,
            fetched_count, error_message, updated_at)
           VALUES (?, ?, ?, ?, ?, ?, ?, datetime('now'))
           ON CONFLICT(root_comment_id) DO UPDATE SET
               post_id=excluded.post_id, status=excluded.status,
               max_id=excluded.max_id, max_id_type=excluded.max_id_type,
               fetched_count=excluded.fetched_count,
               error_message=excluded.error_message,
               updated_at=datetime('now')""",
        (
            str(root_comment_id),
            post_id,
            status,
            str(max_id),
            max_id_type,
            fetched_count,
            error_message,
        ),
    )
