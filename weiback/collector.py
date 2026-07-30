import logging
import time
import random
from datetime import datetime, timezone, timedelta

from . import writer

logger = logging.getLogger(__name__)

TZ_CST = timezone(timedelta(hours=8))


def _pic_urls_to_list(post) -> list[str]:
    if hasattr(post, "pic_urls") and post.pic_urls:
        return list(post.pic_urls)
    return []


def _post_to_dict(post, uid: int) -> dict:
    return {
        "id": int(post.id),
        "uid": uid,
        "text": post.text or "",
        "created_at": _to_rfc3339(post.created_at) if hasattr(post, "created_at") and post.created_at else "",
        "attitudes_count": getattr(post, "attitudes_count", 0) or 0,
        "attitudes_status": getattr(post, "attitudes_status", 0) or 0,
        "comments_count": getattr(post, "comments_count", 0) or 0,
        "reposts_count": getattr(post, "reposts_count", 0) or 0,
        "repost_type": getattr(post, "repost_type", 0) or 0,
        "retweeted_id": int(post.retweeted_status.id) if getattr(post, "retweeted_status", None) and post.retweeted_status.id else None,
        "pic_ids": None,
        "pic_infos": None,
        "pic_num": len(_pic_urls_to_list(post)),
        "geo": None,
        "mblogid": getattr(post, "mblogid", None),
        "mix_media_ids": None,
        "mix_media_info": None,
        "page_info": None,
        "region_name": getattr(post, "region_name", None),
        "source": getattr(post, "source", None),
        "tag_struct": None,
        "url_struct": None,
    }


def _comment_to_dict(comment, post_id: int) -> dict:
    return {
        "id": str(comment.id) if comment.id else f"{post_id}_{hash(comment)}",
        "post_id": post_id,
        "user_id": getattr(comment, "user_id", None),
        "user_screen_name": getattr(comment, "user_screen_name", None),
        "text": getattr(comment, "text", None),
        "created_at": _to_rfc3339(comment.created_at) if hasattr(comment, "created_at") and comment.created_at else None,
        "like_count": getattr(comment, "like_counts", 0) or 0,
        "reply_id": getattr(comment, "reply_id", None),
    }


def _to_rfc3339(dt: datetime) -> str:
    if dt.tzinfo is None:
        dt = dt.replace(tzinfo=TZ_CST)
    return dt.isoformat()


def sync_user(conn, client, uid: str, max_pages: int = 5, with_comments: bool = False) -> int:
    self_uid = int(uid)
    last_id = writer.get_last_post_id(conn, self_uid)
    logger.info("同步 uid=%s (最后ID=%d)", uid, last_id)

    user_obj = client.get_user_by_uid(uid=uid)
    writer.save_user(conn, {
        "id": self_uid,
        "screen_name": getattr(user_obj, "screen_name", uid),
        "avatar_hd": getattr(user_obj, "avatar_hd", None),
        "avatar_large": getattr(user_obj, "avatar_large", None),
        "profile_image_url": getattr(user_obj, "profile_image_url", None),
        "domain": getattr(user_obj, "domain", None),
        "following": getattr(user_obj, "following", False),
        "follow_me": getattr(user_obj, "follow_me", False),
    })

    new_count = 0
    for page in range(1, max_pages + 1):
        posts = client.get_user_posts(
            uid=uid, page=page, expand="all",
            with_comments=with_comments, comment_limit=10,
        )
        if not posts:
            break

        batch = []
        for post in posts:
            post_id = int(post.id)
            if post_id <= last_id:
                conn.commit()
                return new_count

            post_data = _post_to_dict(post, self_uid)
            batch.append(post_data)

            if post.pic_urls:
                writer.save_pictures(conn, post_id, post.pic_urls, self_uid)

            if with_comments and getattr(post, "comments", None):
                comments = [_comment_to_dict(c, post_id) for c in post.comments if c]
                writer.save_comments(conn, comments)
                writer.mark_comments_synced(conn, post_id)

        writer.save_posts(conn, batch)
        conn.commit()
        new_count += len(batch)

    conn.commit()
    logger.info("同步完成: uid=%s, 新增 %d 条", uid, new_count)
    return new_count


def backfill_comments(conn, client, limit: int = 50, post_delay: tuple[float, float] = (3.0, 6.0), max_pages: int | None = None):
    posts = writer.get_uncommented_posts(conn, limit)
    if not posts:
        logger.info("没有需要回补评论的帖子")
        return 0

    fetched = 0
    for i, row in enumerate(posts, 1):
        post_id = str(row["id"])
        try:
            comments = client.get_all_comments(post_id, max_pages=max_pages, use_proxy=True)
        except Exception as e:
            logger.warning("[%d/%d] 帖子 %s 评论抓取失败: %s", i, len(posts), post_id, e)
            continue

        if comments:
            writer.save_comments(conn, [_comment_to_dict(c, row["id"]) for c in comments])
            fetched += 1

        writer.mark_comments_synced(conn, row["id"])
        conn.commit()

        if post_delay and i < len(posts):
            time.sleep(random.uniform(*post_delay))

    logger.info("评论回补完成: 处理 %d 帖, 有评论 %d 帖", len(posts), fetched)
    return fetched
