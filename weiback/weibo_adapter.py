import html
import re
from types import SimpleNamespace


_HTML_TAG_RE = re.compile(r"<[^>]+>")


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
        created_at=None,
        source=raw.get("source"),
        like_counts=raw.get("like_counts") or raw.get("like_count") or 0,
        liked=bool(raw.get("liked", False)),
        reply_id=raw.get("reply_id"),
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
