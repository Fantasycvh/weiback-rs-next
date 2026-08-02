"""事件抽取：从微博上游原始响应提取规范化 DTO。

P1-A 交付物：
- collector 与 Python writer 解耦：本模块只从上游 JSON 响应提取规范化
  DTO（user/post/comment/media_reference），不写任何数据库。
- 用户、帖子、评论、媒体引用规范化事件：输出字段对齐
  `docs/protocol/v1/dtos.schema.json`。

全部为纯函数，零第三方依赖。时间字段统一转 RFC3339。
"""

from __future__ import annotations

import html
import json
import re
from datetime import datetime, timedelta, timezone
from urllib.parse import parse_qsl, urlsplit

_HTML_TAG_RE = re.compile(r"<[^>]+>")

# 微博常见 created_at 格式："Sat Aug 01 12:00:00 +0800 2026"
_WEIBO_CREATED_AT_FORMATS = (
    "%a %b %d %H:%M:%S %z %Y",
    "%a %b %d %H:%M:%S %z",
    "%Y-%m-%d %H:%M:%S",
)

TZ_CST = timezone(timedelta(hours=8))

_I64_MAX = 9_223_372_036_854_775_807
_SENSITIVE_URL_KEYS = {
    "access_token",
    "auth",
    "cookie",
    "gsid",
    "passport",
    "session",
    "sub",
    "token",
    "xsrf",
}
_SENSITIVE_URL_KEY_PARTS = (
    "auth", "cookie", "credential", "gsid", "passport", "password", "secret",
    "session", "signature", "token", "xsrf",
)


def clean_html_text(value) -> str | None:
    """去除微博文本中的 HTML 标签并反转义实体。"""
    if not isinstance(value, str):
        return None
    return html.unescape(_HTML_TAG_RE.sub("", value))


def _to_rfc3339(value) -> str | None:
    """把微博 created_at 转 RFC3339；无法解析时原样返回字符串。"""
    if value is None:
        return None
    if isinstance(value, (int, float)):
        dt = datetime.fromtimestamp(value, tz=timezone.utc)
        return _format_rfc3339(dt)
    if not isinstance(value, str) or not value:
        return None
    text = value.strip()
    for fmt in _WEIBO_CREATED_AT_FORMATS:
        try:
            dt = datetime.strptime(text, fmt)
        except ValueError:
            continue
        if dt.tzinfo is None:
            dt = dt.replace(tzinfo=TZ_CST)
        return _format_rfc3339(dt)
    # 已经是 RFC3339 / ISO 格式（带时区）则直接规范化
    if re.match(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}", text):
        return text
    return text


def _format_rfc3339(dt: datetime) -> str:
    return dt.astimezone(timezone.utc).isoformat(timespec="milliseconds").replace("+00:00", "Z")


def _json_value(value):
    """把任意值序列化为 JSON 字符串（对齐旧 collector 的 _json_value）。"""
    if value is None:
        return None
    if isinstance(value, str):
        return value
    try:
        return json.dumps(value, ensure_ascii=False, default=str)
    except (TypeError, ValueError):
        return str(value)


def _as_int(value, default: int | None = 0) -> int | None:
    try:
        return int(value)
    except (TypeError, ValueError):
        return default


def _as_str(value) -> str | None:
    if value is None:
        return None
    return str(value)


def _owner_id(value) -> str | None:
    """返回 Rust i64 可安全解析的规范十进制所有者 ID。"""
    if isinstance(value, bool):
        return None
    text = str(value) if isinstance(value, (int, str)) else ""
    if not re.fullmatch(r"0|[1-9][0-9]*", text):
        return None
    return text if int(text) <= _I64_MAX else None


def _safe_media_url(value) -> str | None:
    """只接受不携带认证信息或敏感查询参数的 HTTPS URL。"""
    if not isinstance(value, str) or not value:
        return None
    try:
        parsed = urlsplit(value)
        query_keys = {
            key.lower() for key, _value in parse_qsl(parsed.query, keep_blank_values=True)
        }
    except ValueError:
        return None
    if parsed.scheme != "https" or not parsed.netloc:
        return None
    if parsed.username is not None or parsed.password is not None:
        return None
    if parsed.fragment or any(_is_sensitive_url_key(key) for key in query_keys):
        return None
    return value


def _is_sensitive_url_key(key: str) -> bool:
    normalized = re.sub(r"[^a-z0-9]", "", key.lower())
    return key.lower() in _SENSITIVE_URL_KEYS or any(
        part in normalized for part in _SENSITIVE_URL_KEY_PARTS
    )


# -- 用户 ----------------------------------------------------------------

def extract_user(raw: dict | None) -> dict:
    """从原始 user 对象提取规范化 user_dto（对齐 dtos.user_dto）。"""
    if not isinstance(raw, dict):
        raw = {}
    avatar_hd = raw.get("avatar_hd") or raw.get("avatar_large")
    return {
        "id": raw.get("id") if raw.get("id") is not None else raw.get("idstr"),
        "screen_name": raw.get("screen_name"),
        "avatar_hd": avatar_hd,
        "avatar_large": raw.get("avatar_large"),
        "profile_image_url": raw.get("profile_image_url"),
        "domain": raw.get("domain"),
        "following": bool(raw.get("following", False)),
        "follow_me": bool(raw.get("follow_me", False)),
    }


# -- 帖子 ----------------------------------------------------------------

def extract_post(raw: dict, uid) -> dict:
    """从原始 status 对象提取规范化 post_dto（对齐 dtos.post_dto）。

    `uid` 为帖子作者 uid；原始响应通常嵌在 status.user.id，取不到时用入参兜底。
    """
    if not isinstance(raw, dict):
        raw = {}
    author_uid = _as_int(raw.get("user_id"), None)
    if author_uid is None:
        user = raw.get("user")
        if isinstance(user, dict) and user.get("id") is not None:
            author_uid = _as_int(user.get("id"), None)
    if author_uid is None:
        author_uid = _as_int(uid, None)

    post_id = raw.get("id") if raw.get("id") is not None else raw.get("idstr")
    retweeted = raw.get("retweeted_status")
    retweeted_id = None
    if isinstance(retweeted, dict):
        rid = retweeted.get("id") if retweeted.get("id") is not None else retweeted.get("idstr")
        retweeted_id = rid

    pic_ids = raw.get("pic_ids")
    if pic_ids is None:
        pics = raw.get("pics")
        if isinstance(pics, list):
            pic_ids = [p.get("pid") or p.get("url") for p in pics if isinstance(p, dict)]
        else:
            pic_ids = []

    content_status = raw.get("content_status", "complete")
    if content_status not in ("partial", "complete"):
        content_status = "complete"

    return {
        "id": post_id,
        "uid": author_uid,
        "text": clean_html_text(raw.get("text")),
        "created_at": _to_rfc3339(raw.get("created_at")),
        "attitudes_count": _as_int(raw.get("attitudes_count")),
        "attitudes_status": _as_int(raw.get("attitudes_status")),
        "comments_count": _as_int(raw.get("comments_count")),
        "reposts_count": _as_int(raw.get("reposts_count")),
        "repost_type": _as_int(raw.get("repost_type")),
        "retweeted_id": retweeted_id,
        "pic_ids": pic_ids or None,
        "pic_infos": raw.get("pic_infos"),
        "pic_num": _as_int(raw.get("pic_num"), len(pic_ids or [])),
        "geo": raw.get("geo"),
        "mblogid": raw.get("mblogid") or raw.get("bid"),
        "mix_media_ids": raw.get("mix_media_ids"),
        "mix_media_info": raw.get("mix_media_info"),
        "page_info": raw.get("page_info"),
        "region_name": raw.get("region_name"),
        "source": raw.get("source"),
        "tag_struct": raw.get("tag_struct"),
        "url_struct": raw.get("url_struct"),
        "deleted": bool(raw.get("deleted", False)),
        "edit_count": _as_int(raw.get("edit_count")),
        "favorited": bool(raw.get("favorited", False)),
        "bid": raw.get("bid"),
        "location": raw.get("location"),
        "topic_ids": raw.get("topic_ids"),
        "at_users": raw.get("at_users"),
        "is_long_text": bool(raw.get("isLongText", raw.get("is_long_text", False))),
        "video_url": raw.get("video_url") or raw.get("page_info", {}).get("media_info", {}).get("stream_url") if isinstance(raw.get("page_info"), dict) else raw.get("video_url"),
        "raw_data": _json_value(_redacted_diagnostics(raw)),
        "content_status": content_status,
        "fetch_error": raw.get("fetch_error"),
        "first_fetched_at": raw.get("first_fetched_at"),
        "last_refreshed_at": _to_rfc3339(raw.get("last_refreshed_at")),
    }


def _redacted_diagnostics(raw: dict) -> dict:
    """构造脱敏诊断摘要：保留状态码/存在性，不保留认证秘密。"""
    result = {}
    for key in ("status", "truncated", "isLongText", "mblogid", "created_at"):
        if key in raw:
            result[key] = raw[key]
    if isinstance(raw.get("user"), dict):
        result["user_exists"] = True
    return result


# -- 评论 ----------------------------------------------------------------

def extract_comment(raw: dict | None, post_id, *, root_id=None, parent_id=None, depth: int = 0) -> dict:
    """从原始评论对象提取规范化 comment_dto（对齐 dtos.comment_dto）。

    兼容 hotFlowChild 两种条目形态：直接字段或嵌套 `user`/`pic` 对象。
    """
    if not isinstance(raw, dict):
        raw = {}
    user = raw.get("user")
    if not isinstance(user, dict):
        user = {}
    pic = raw.get("pic")
    if not isinstance(pic, dict):
        pic = {}
    large_pic = pic.get("large")
    if not isinstance(large_pic, dict):
        large_pic = {}

    comment_id = raw.get("id") if raw.get("id") is not None else raw.get("idstr")
    if comment_id is None:
        comment_id = str(raw.get("rootidstr", f"{post_id}_{depth}"))

    reply_id = raw.get("reply_id") or raw.get("reply_comment_id")
    child_count = raw.get("total_number")
    if child_count is None:
        child_count = raw.get("child_count", 0)

    return {
        "id": str(comment_id),
        "post_id": post_id,
        "user_id": user.get("id") if user.get("id") is not None else user.get("idstr"),
        "user_screen_name": user.get("screen_name"),
        "text": clean_html_text(raw.get("text")),
        "created_at": _to_rfc3339(raw.get("created_at")),
        "like_count": _as_int(raw.get("like_counts", raw.get("like_count"))),
        "reply_id": _as_str(reply_id),
        "root_id": _as_str(root_id) if root_id else _as_str(comment_id),
        "parent_id": _as_str(parent_id),
        "depth": max(0, _as_int(depth, 0) or 0),
        "source": raw.get("source"),
        "user_avatar_url": (
            user.get("avatar_hd")
            or user.get("avatar_large")
            or user.get("profile_image_url")
        ),
        "user_verified": bool(user.get("verified", False)),
        "liked": bool(raw.get("liked", False)),
        "reply_text": clean_html_text(raw.get("reply_text")),
        "pic_url": large_pic.get("url") or pic.get("url"),
        "child_count": _as_int(child_count),
        "raw_data": _json_value(_redacted_comment_diagnostics(raw)),
        "last_refreshed_at": _to_rfc3339(raw.get("last_refreshed_at")),
    }


def _redacted_comment_diagnostics(raw: dict) -> dict:
    result = {}
    for key in ("status", "created_at", "reply_id"):
        if key in raw:
            result[key] = raw[key]
    if isinstance(raw.get("user"), dict):
        result["user_exists"] = True
    return result


def unpack_child_comment_page(response: dict) -> tuple[list[dict], str, int]:
    """解析 hotFlowChild 响应，兼容两种已知信封格式。

    格式 A：`data` 直接是评论数组，游标在响应顶层。
    格式 B：`data` 是 dict，评论在 `data.data` 或 `data.comments`，游标在 data 层。

    返回 `(评论原始条目列表, next_max_id, next_max_id_type)`。
    """
    payload = response.get("data", {})
    if isinstance(payload, list):
        items = payload
        cursor_source = response
    elif isinstance(payload, dict):
        nested = payload.get("data", payload.get("comments", []))
        items = nested if isinstance(nested, list) else []
        cursor_source = payload
    else:
        items = []
        cursor_source = response

    max_id = cursor_source.get("max_id", response.get("max_id", 0))
    max_id_type = _as_int(
        cursor_source.get("max_id_type", response.get("max_id_type", 0)), 0
    ) or 0
    return items, str(max_id or 0), max_id_type


# -- 媒体引用 ------------------------------------------------------------

def media_reference_from_user(user_dto: dict) -> dict | None:
    """为用户头像选择一个最佳安全 URL：HD → large → profile。"""
    owner_id = _owner_id(user_dto.get("id"))
    if owner_id is None:
        return None
    for field, definition in (
        ("avatar_hd", "original"),
        ("avatar_large", "large"),
        ("profile_image_url", "bmiddle"),
    ):
        url = _safe_media_url(user_dto.get(field))
        if url is not None:
            return {
                "owner_type": "user",
                "owner_id": owner_id,
                "post_id": None,
                "user_id": owner_id,
                "media_type": "avatar",
                "url": url,
                "definition": definition,
            }
    return None

def media_references_from_post(post_dto: dict, raw: dict) -> list[dict]:
    """从帖子提取图片/视频 media_reference DTO（对齐 dtos.media_reference_dto）。

    从 `pic_infos` 遍历图片 URL；有 `video_url` 时额外产出 video 引用。
    """
    refs: list[dict] = []
    owner_id = _owner_id(post_dto.get("id"))
    if owner_id is None:
        return refs

    user_id = _owner_id(post_dto.get("uid"))

    pic_infos = post_dto.get("pic_infos")
    if isinstance(pic_infos, dict):
        for url, definition in _best_pic_urls(pic_infos):
            refs.append({
                "owner_type": "post",
                "owner_id": owner_id,
                "post_id": owner_id,
                "user_id": user_id,
                "media_type": "picture",
                "url": url,
                "definition": definition,
            })
    video_url = _safe_media_url(post_dto.get("video_url"))
    if video_url is not None:
        refs.append({
            "owner_type": "post",
            "owner_id": owner_id,
            "post_id": owner_id,
            "user_id": user_id,
            "media_type": "video",
            "url": video_url,
            "definition": None,
        })
    return refs


def media_reference_from_comment(comment_dto: dict) -> dict | None:
    """从评论提取图片 media_reference DTO；无图时返回 None。"""
    owner_id = _owner_id(comment_dto.get("id"))
    url = _safe_media_url(comment_dto.get("pic_url"))
    if owner_id is None or url is None:
        return None
    return {
        "owner_type": "comment",
        "owner_id": owner_id,
        "post_id": _owner_id(comment_dto.get("post_id")),
        "user_id": _owner_id(comment_dto.get("user_id")),
        "media_type": "picture",
        "url": url,
        "definition": None,
    }


def _best_pic_urls(pic_infos: dict) -> list[tuple[str, str]]:
    refs: list[tuple[str, str]] = []
    for info in pic_infos.values():
        if not isinstance(info, dict):
            continue
        for field, definition in (
            ("original_pic", "original"),
            ("large_pic", "large"),
            ("bmiddle_pic", "bmiddle"),
        ):
            pic = info.get(field)
            url = _safe_media_url(pic.get("url") if isinstance(pic, dict) else None)
            if url is not None:
                refs.append((url, definition))
                break
    return refs
