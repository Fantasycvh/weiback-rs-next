import html
import re
from datetime import datetime
from types import SimpleNamespace


_HTML_TAG_RE = re.compile(r"<[^>]+>")

_WEIBO_TIME_FORMATS = (
    "%a %b %d %H:%M:%S %z %Y",  # "Sat Aug 01 03:10:00 +0800 2026"
    "%a %b %d %H:%M:%S %Y",
    "%Y-%m-%d %H:%M:%S",
)


def parse_weibo_datetime(value) -> datetime | None:
    """Convert Weibo time strings to a timezone-aware datetime.

    Accepts the classic ``Sat Aug 01 03:10:00 +0800 2026`` format,
    a few common ISO-8601 variants, and passes ``datetime`` through.
    """
    if isinstance(value, datetime):
        return value
    if not isinstance(value, str) or not value:
        return None
    text = value.strip()
    if not text:
        return None
    for fmt in _WEIBO_TIME_FORMATS:
        try:
            return datetime.strptime(text, fmt)
        except ValueError:
            continue
    try:
        return datetime.fromisoformat(text)
    except ValueError:
        return None


def parse_child_comment(raw: dict) -> SimpleNamespace:
    """Convert a hotFlowChild response item to the comment shape we store."""
    raw_user = raw.get("user")
    user: dict = raw_user if isinstance(raw_user, dict) else {}
    raw_picture = raw.get("pic")
    picture: dict = raw_picture if isinstance(raw_picture, dict) else {}
    raw_large_picture = picture.get("large")
    large_picture: dict = (
        raw_large_picture if isinstance(raw_large_picture, dict) else {}
    )
    text = raw.get("text")
    clean_text = html.unescape(_HTML_TAG_RE.sub("", text)) if isinstance(text, str) else text

    return SimpleNamespace(
        id=raw.get("id") or raw.get("idstr"),
        user_id=user.get("id") or user.get("idstr"),
        user_screen_name=user.get("screen_name"),
        user_avatar_url=(
            user.get("avatar_hd")
            or user.get("avatar_large")
            or user.get("profile_image_url")
        ),
        user_verified=bool(user.get("verified", False)),
        text=clean_text,
        created_at=parse_weibo_datetime(raw.get("created_at")),
        source=raw.get("source"),
        like_counts=raw.get("like_counts") or raw.get("like_count") or 0,
        liked=bool(raw.get("liked", False)),
        reply_id=raw.get("reply_id") or raw.get("reply_comment_id"),
        reply_text=raw.get("reply_text"),
        pic_url=large_picture.get("url") or picture.get("url"),
        child_count=raw.get("total_number") or raw.get("child_count") or 0,
        raw_data=raw,
    )


def unpack_child_comment_page(response: dict) -> tuple[list[dict], str, int]:
    """Read both known hotFlowChild response envelopes."""
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
    max_id_type = cursor_source.get(
        "max_id_type", response.get("max_id_type", 0)
    )
    return items, str(max_id or 0), int(max_id_type or 0)


class WeiboBusinessError(Exception):
    """Raised when the Weibo API reports a business-level failure."""

    def __init__(self, message: str, code: int | None = None):
        super().__init__(message)
        self.code = code


def unpack_build_comments_page(response: dict) -> tuple[list[dict], str]:
    """Read the weibo.com ``/ajax/statuses/buildComments`` envelope.

    Returns ``(comment_raw_items, next_max_id)``. Raises
    :class:`WeiboBusinessError` when the API reports a business failure
    (``ok`` negative / falsy), mapping login expiry to ``401`` and rate
    limiting to ``429``.
    """
    if not isinstance(response, dict):
        return [], "0"

    ok = response.get("ok", 1)
    if isinstance(ok, int) and ok < 0:
        raise WeiboBusinessError(_business_message(response) or "登录状态失效", code=401)
    if ok in (0, False):
        message = _business_message(response) or "评论接口业务失败"
        lowered = message.lower()
        if any(keyword in message for keyword in ("登录", "授权", "过期", "未登录")):
            raise WeiboBusinessError(message, code=401)
        if any(keyword in message for keyword in ("频繁", "频次", "太频繁", "rate limit")):
            raise WeiboBusinessError(message, code=429)
        raise WeiboBusinessError(message, code=500)

    data = response.get("data")
    items = data if isinstance(data, list) else []
    max_id = response.get("max_id")
    next_max_id = str(max_id) if max_id not in (None, "", 0) else "0"
    return items, next_max_id


def _business_message(response: dict) -> str:
    for key in ("msg", "message", "errmsg"):
        value = response.get(key)
        if isinstance(value, str) and value:
            return value
    return ""
