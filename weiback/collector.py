import logging
import time
import random
import json
from datetime import datetime, timezone, timedelta

from . import writer
from .task_manager import TaskManager, TaskType
from .weibo_adapter import (
    parse_child_comment,
    parse_weibo_datetime,
    unpack_build_comments_page,
    unpack_child_comment_page,
)

logger = logging.getLogger(__name__)

TZ_CST = timezone(timedelta(hours=8))

DEFAULT_PAGE_DELAY = 3.0

_COMMENTS_URL = "https://weibo.com/ajax/statuses/buildComments"
_COMMENTS_REFERER = "https://weibo.com/"


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
        "mblogid": getattr(post, "mblogid", None) or getattr(post, "bid", None),
        "mix_media_ids": None,
        "mix_media_info": None,
        "page_info": None,
        "region_name": getattr(post, "region_name", None),
        "source": getattr(post, "source", None),
        "tag_struct": None,
        "url_struct": None,
        "bid": getattr(post, "bid", None),
        "location": getattr(post, "location", None),
        "topic_ids": _json_value(getattr(post, "topic_ids", None)),
        "at_users": _json_value(getattr(post, "at_users", None)),
        "is_long_text": bool(getattr(post, "is_long_text", False)),
        "video_url": getattr(post, "video_url", None),
        "raw_data": _json_value(getattr(post, "raw_data", None)),
        "content_status": "complete",
    }


def _json_value(value) -> str | None:
    if value is None:
        return None
    return json.dumps(value, ensure_ascii=False, default=str)


def _post_uid(post, fallback_uid: int) -> int:
    value = getattr(post, "user_id", None)
    try:
        return int(value) if value is not None else fallback_uid
    except (TypeError, ValueError):
        return fallback_uid


def _post_tree(post, fallback_uid: int):
    yield post, _post_uid(post, fallback_uid)
    retweeted = getattr(post, "retweeted_status", None)
    if retweeted is not None:
        yield from _post_tree(retweeted, fallback_uid)


def _comment_to_dict(
    comment,
    post_id: int,
    *,
    root_id: str | None = None,
    parent_id: str | None = None,
    depth: int = 0,
) -> dict:
    comment_id = str(comment.id) if comment.id else f"{post_id}_{hash(comment)}"
    reply_id = getattr(comment, "reply_id", None)
    created = getattr(comment, "created_at", None)
    if not isinstance(created, datetime):
        created = parse_weibo_datetime(created)
    return {
        "id": comment_id,
        "post_id": post_id,
        "user_id": getattr(comment, "user_id", None),
        "user_screen_name": getattr(comment, "user_screen_name", None),
        "text": getattr(comment, "text", None),
        "created_at": _to_rfc3339(created) if created else None,
        "like_count": getattr(comment, "like_counts", 0) or 0,
        "reply_id": reply_id,
        "root_id": root_id or comment_id,
        "parent_id": parent_id,
        "depth": depth,
        "source": getattr(comment, "source", None),
        "user_avatar_url": getattr(comment, "user_avatar_url", None),
        "user_verified": bool(getattr(comment, "user_verified", False)),
        "liked": bool(getattr(comment, "liked", False)),
        "reply_text": getattr(comment, "reply_text", None),
        "pic_url": getattr(comment, "pic_url", None),
        "child_count": _comment_child_count(comment),
        "raw_data": _json_value(getattr(comment, "raw_data", None)),
    }


def _comment_child_count(comment) -> int:
    """Determine whether a comment has child replies worth fetching.

    Prefers an explicit ``child_count``, then falls back to a
    ``total_number`` nested in the comment's raw payload (both are how
    Weibo reports the number of replies under a comment). Accepts both
    objects and the row dicts produced by :func:`_comment_to_dict`.
    """
    if isinstance(comment, dict):
        value = comment.get("child_count") or 0
        if value:
            return int(value)
        raw = comment.get("raw_data")
        if isinstance(raw, dict):
            value = raw.get("total_number") or raw.get("child_count") or 0
            try:
                return int(value)
            except (TypeError, ValueError):
                return 0
        return 0
    value = getattr(comment, "child_count", 0) or 0
    if value:
        return int(value)
    raw = getattr(comment, "raw_data", None)
    if isinstance(raw, dict):
        value = raw.get("total_number") or raw.get("child_count") or 0
        try:
            return int(value)
        except (TypeError, ValueError):
            return 0
    return 0


def _comment_tree_rows(comments, post_id: int) -> list[dict]:
    """Build comment dicts while assigning root_id/parent_id/depth.

    A comment whose ``reply_id`` points at another comment in the same
    page becomes its child (depth + 1); otherwise it is treated as a
    top-level root comment. ``reply_id`` pointing at a not-yet-seen id is
    resolved in a second pass so chains built within one page work.
    """
    seen: dict[str, dict] = {}
    ordered: list[dict] = []
    unresolved: list[tuple[str, dict]] = []

    for comment in comments:
        if not comment:
            continue
        row = _comment_to_dict(comment, post_id)
        reply_id = row["reply_id"]
        if not reply_id:
            row["root_id"] = row["id"]
            row["parent_id"] = None
            row["depth"] = 0
            seen[row["id"]] = row
            ordered.append(row)
        else:
            unresolved.append((str(reply_id), row))

    changed = True
    while unresolved and changed:
        changed = False
        remaining = []
        for reply_id, row in unresolved:
            parent = seen.get(reply_id)
            if parent is None:
                remaining.append((reply_id, row))
                continue
            row["root_id"] = parent["root_id"] or parent["id"]
            row["parent_id"] = parent["id"]
            row["depth"] = parent["depth"] + 1
            seen[row["id"]] = row
            ordered.append(row)
            changed = True
        unresolved = remaining

    for reply_id, row in unresolved:
        row["root_id"] = row["id"]
        row["parent_id"] = None
        row["depth"] = 0
        seen.setdefault(row["id"], row)
        ordered.append(row)

    return ordered


def _to_rfc3339(dt: datetime) -> str:
    if dt.tzinfo is None:
        dt = dt.replace(tzinfo=TZ_CST)
    return dt.isoformat()


SAFETY_MAX_PAGES = 1000


def _drop_synced_posts(posts: list, last_id: int) -> list:
    """截断已同步过的旧帖（微博按 id 倒序返回，遇到 <= last_id 即停）。"""
    kept: list = []
    for post in posts:
        try:
            if int(post.id) <= last_id:
                break
        except (TypeError, ValueError):
            pass
        kept.append(post)
    return kept


def _match_content_type(post, content_type: str) -> bool:
    """本地判断微博是否属于指定内容类型。

    - all: 全部
    - original: 原创（非转发）
    - picture: 含图片
    - video: 含视频
    - article: 长文
    """
    if content_type in ("all", None, ""):
        return True
    if content_type == "original":
        if getattr(post, "retweeted_status", None) is not None:
            return False
        return bool(getattr(post, "is_original", True))
    if content_type == "picture":
        return bool(_pic_urls_to_list(post))
    if content_type == "video":
        return bool(getattr(post, "video_url", None))
    if content_type == "article":
        return bool(getattr(post, "is_long_text", False))
    return True


def sync_user(conn, client, uid: str, max_pages: int | None = None, with_comments: bool = False,
              task_manager: TaskManager | None = None, page_delay: float | None = None,
              content_type: str = "all", comment_limit: int = 10) -> int:
    self_uid = int(uid)
    last_id = writer.get_last_post_id(conn, self_uid)
    logger.info("同步 uid=%s (最后ID=%s, max_pages=%s)", uid, last_id, max_pages or "不限")

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
    avatar_url = (
        getattr(user_obj, "avatar_hd", None)
        or getattr(user_obj, "avatar_large", None)
        or getattr(user_obj, "profile_image_url", None)
    )
    if avatar_url:
        writer.save_media(
            conn,
            owner_type="user",
            owner_id=str(self_uid),
            media_type="avatar",
            url=avatar_url,
            user_id=str(self_uid),
        )

    new_count = 0
    if max_pages is not None and max_pages > 0:
        manual = True
        page_limit: int = max_pages
    else:
        manual = False
        page_limit = SAFETY_MAX_PAGES
    for page in range(1, page_limit + 1):
        try:
            posts = client.get_user_posts(
                uid=uid, page=page, expand=True,
                with_comments=with_comments, comment_limit=comment_limit,
            )
        except Exception as e:
            logger.warning("第 %d 页获取失败: %s", page, e)
            if task_manager:
                task_manager.report_error("fetch_page", str(e), f"page_{page}")

            if page_delay:
                time.sleep(page_delay)
            continue

        if not posts:
            if not manual:
                logger.info("翻页结束: uid=%s, 第 %d 页无数据", uid, page)
            break

        # 增量断点：仅定时/全量同步（未指定页数）启用
        if not manual and last_id:
            posts = _drop_synced_posts(posts, last_id)
            if not posts:
                logger.info("已达已同步断点: uid=%s 第 %d 页无新帖", uid, page)
                break

        if content_type and content_type != "all":
            posts = [p for p in posts if _match_content_type(p, content_type)]

        if not posts:
            # 该页有数据但全部被类型过滤掉，继续翻页
            if page_delay:
                time.sleep(page_delay)
            continue

        batch = []
        batch_ids: set[int] = set()
        for post in posts:
            post_id = int(post.id)
            was_present = writer.post_exists(conn, post_id)
            if not was_present:
                new_count += 1

            for tree_post, tree_uid in _post_tree(post, self_uid):
                tree_post_id = int(tree_post.id)
                if tree_post_id not in batch_ids:
                    batch.append(_post_to_dict(tree_post, tree_uid))
                    batch_ids.add(tree_post_id)

                pic_urls = _pic_urls_to_list(tree_post)
                if pic_urls:
                    writer.save_pictures(conn, tree_post_id, pic_urls, tree_uid)
                video_url = getattr(tree_post, "video_url", None)
                if video_url:
                    writer.save_video(conn, video_url, "", tree_post_id)

            if with_comments and getattr(post, "comments", None):
                comments = _comment_tree_rows(post.comments, post_id)
                writer.save_comments(conn, comments)
                writer.mark_comments_synced(conn, post_id)

        if batch:
            writer.save_posts(conn, batch)
            conn.commit()

        if task_manager:
            task_manager.update_progress(page, page_limit)

        if new_count > 0:
            logger.info("同步进度: uid=%s, 已获取 %d 条 (第 %d/%d 页)", uid, new_count, page, page_limit)

        if page_delay and page < page_limit:
            time.sleep(page_delay)

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
        try:
            if _backfill_one_post(conn, client, row, max_pages=max_pages):
                fetched += 1
        except Exception as e:
            logger.warning(
                "[%d/%d] 帖子 %s 评论回补失败: %s",
                i,
                len(posts),
                row["id"],
                e,
            )

        if post_delay and i < len(posts):
            time.sleep(random.uniform(*post_delay))

    logger.info("评论回补完成: 处理 %d 帖, 有评论 %d 帖", len(posts), fetched)
    return fetched


def _fetch_comments_page(client, post_id: str, max_id: str) -> tuple[list[dict], str]:
    """Fetch one buildComments page; returns (raw_items, next_max_id)."""
    params = {
        "is_reload": "1",
        "id": post_id,
        "is_show_bulletin": "2",
        "is_mix": "0",
        "count": "20",
        "flow": "0",
    }
    if max_id not in ("", "0", None):
        params["is_reload"] = "0"
        params["max_id"] = max_id
    response = client._request(
        _COMMENTS_URL,
        params=params,
        use_proxy=True,
        headers={"Referer": _COMMENTS_REFERER},
    )
    return unpack_build_comments_page(response)


def _backfill_one_post(conn, client, row: dict, *, max_pages: int | None = None) -> bool:
    """回补单个帖子的全部评论页与二级回复。返回是否抓到评论。"""
    post_id = str(row["id"])
    progress = writer.get_comments_progress(conn, row["id"])
    cursor = progress["cursor"] if progress else "0"
    total_fetched = int(progress["fetched_count"] or 0) if progress else 0
    pages_this_run = 0
    got_comments = False
    root_comments: list[tuple[str, object]] = []

    while max_pages is None or pages_this_run < max_pages:
        try:
            raw_comments, next_max_id = _fetch_comments_page(client, post_id, cursor)
            pages_this_run += 1
            if raw_comments:
                parsed = [c for c in (parse_child_comment(r) for r in raw_comments) if c]
                rows = _comment_tree_rows(parsed, row["id"])
                writer.save_comments(conn, rows)
                got_comments = True
                total_fetched += len(rows)
                root_comments.extend(
                    (str(r["id"]), r) for r in rows if r["depth"] == 0
                )

            complete = not raw_comments or next_max_id in ("", "0", None)
            writer.save_comments_progress(
                conn,
                post_id=row["id"],
                status="complete" if complete else "running",
                cursor=next_max_id or "0",
                fetched_count=total_fetched,
            )
            conn.commit()
            cursor = next_max_id or "0"
            if complete:
                break
        except Exception as e:
            writer.save_comments_progress(
                conn,
                post_id=row["id"],
                status="failed",
                cursor=cursor,
                fetched_count=total_fetched,
                error_message=str(e),
            )
            conn.commit()
            logger.warning("帖子 %s 评论第 %d 页抓取失败: %s", post_id, pages_this_run, e)
            break

    if got_comments:
        try:
            _backfill_replies_for_post(
                conn, client, int(post_id), root_comments, page_delay=0.0
            )
        except Exception as e:
            logger.warning("帖子 %s 二级回复自动回补失败: %s", post_id, e)
    return got_comments


def backfill_post_comments(
    conn, client, post_id: int, max_pages: int | None = None
) -> bool:
    """回补单个帖子的评论（手动触发）。返回是否抓到评论。"""
    row = conn.execute(
        "SELECT id FROM posts WHERE id=?", (post_id,)
    ).fetchone()
    if row is None:
        return False
    return _backfill_one_post(conn, client, dict(row), max_pages=max_pages)


def _backfill_replies_for_post(
    conn,
    client,
    post_id: int,
    root_comments: list[tuple[str, object]],
    *,
    page_delay: float = DEFAULT_PAGE_DELAY,
) -> int:
    """Fetch replies for root comments that report child replies.

    ``root_comments`` carries ``(comment_id, row_dict)`` pairs collected
    during the page walk so we do not need to re-query the DB; the row
    dict is used only for ``child_count``.
    """
    if not root_comments:
        return 0
    fetched = 0
    for comment_id, row in root_comments:
        child_count = _comment_child_count(row)
        if child_count <= 0:
            continue
        try:
            fetched += fetch_comment_replies(
                conn,
                client,
                post_id,
                str(comment_id),
                page_delay=page_delay,
            )
        except Exception as e:
            logger.warning("评论 %s 二级回复抓取失败: %s", comment_id, e)
    return fetched


def _existing_comment_depth(
    conn, parent_id: str, post_id: int, fallback_root_id: str
) -> int:
    """Return depth of an already-stored comment, so nested replies nest.

    If ``parent_id`` points at an existing comment of this post we return
    ``parent.depth + 1``; otherwise the reply is a direct child of the
    root comment (depth 1).
    """
    try:
        row = conn.execute(
            "SELECT depth FROM comments WHERE id=? AND post_id=?",
            (parent_id, post_id),
        ).fetchone()
    except Exception:
        return 1
    if row is None:
        return 1
    try:
        return int(row[0]) + 1
    except (TypeError, ValueError):
        return 1


def fetch_comment_replies(
    conn,
    client,
    post_id: int,
    root_comment_id: str,
    *,
    page_delay: float = DEFAULT_PAGE_DELAY,
) -> int:
    """Fetch all replies for one root comment, resuming from its last cursor."""
    root_comment_id = str(root_comment_id)
    progress = writer.get_comment_reply_progress(conn, root_comment_id)
    max_id = str(progress["max_id"]) if progress else "0"
    max_id_type = int(progress["max_id_type"]) if progress else 0
    total_fetched = int(progress["fetched_count"]) if progress else 0
    fetched_this_run = 0

    while True:
        try:
            response = client._request(
                "https://m.weibo.cn/comments/hotFlowChild",
                params={
                    "cid": root_comment_id,
                    "max_id": int(max_id) if max_id.isdigit() else max_id,
                    "max_id_type": max_id_type,
                },
                use_proxy=True,
            )
            raw_comments, next_max_id, next_max_id_type = (
                unpack_child_comment_page(response)
            )
            comments = []
            for raw in raw_comments:
                if not isinstance(raw, dict):
                    continue
                parsed = parse_child_comment(raw)
                reply_id = getattr(parsed, "reply_id", None)
                parent_id = str(reply_id) if reply_id else root_comment_id
                depth = _existing_comment_depth(
                    conn, parent_id, post_id, root_comment_id
                )
                comments.append(
                    _comment_to_dict(
                        parsed,
                        post_id,
                        root_id=root_comment_id,
                        parent_id=parent_id,
                        depth=depth,
                    )
                )

            writer.save_comments(conn, comments)
            fetched_this_run += len(comments)
            total_fetched += len(comments)
            max_id = next_max_id
            max_id_type = next_max_id_type
            status = "complete" if max_id == "0" else "running"
            writer.save_comment_reply_progress(
                conn,
                root_comment_id=root_comment_id,
                post_id=post_id,
                status=status,
                max_id=max_id,
                max_id_type=max_id_type,
                fetched_count=total_fetched,
            )
            conn.commit()

            if max_id == "0":
                return fetched_this_run
            if page_delay:
                time.sleep(page_delay)
        except Exception as exc:
            writer.save_comment_reply_progress(
                conn,
                root_comment_id=root_comment_id,
                post_id=post_id,
                status="failed",
                max_id=max_id,
                max_id_type=max_id_type,
                fetched_count=total_fetched,
                error_message=str(exc),
            )
            conn.commit()
            logger.warning(
                "评论 %s 的二级回复抓取失败，保留游标 %s: %s",
                root_comment_id,
                max_id,
                exc,
            )
            return fetched_this_run
