"""协议 v1 契约校验：校验事件信封与 payload DTO 是否符合协议 schema。

P1-A 交付物：
- "现有采集 fixture 全部通过协议契约测试"：本模块从
  `docs/protocol/v1/event.schema.json` 与 `dtos.schema.json` 提取核心约束，
  以纯 Python（零依赖）实现可执行校验。Rust 侧有等价校验（sidecar/protocol.rs）。

`validate_event(envelope)` 返回错误消息列表；空列表表示通过。
"""

from __future__ import annotations

import re

from . import extract
from .protocol import PROTOCOL_VERSION, is_uuid_v7

# 事件类型枚举（event.schema.json）
EVENT_TYPES = {
    "ready",
    "capabilities",
    "started",
    "progress",
    "user",
    "post",
    "comment",
    "media_reference",
    "checkpoint",
    "rate_limited",
    "auth_required",
    "warning",
    "error",
    "done",
    "cancelled",
}

# stream 格式（event.schema.json pattern）
STREAM_RE = re.compile(r"^(user:[^:]+:posts|post:[^:]+:comments|post:[^:]+:replies)$")

# 各事件的必填 payload 字段（dtos.schema.json）
_REQUIRED_PAYLOAD = {
    "ready": ["sidecar_name", "sidecar_version", "protocol_version"],
    "capabilities": ["protocol_versions", "commands"],
    "started": ["stream"],
    "progress": ["phase"],
    "user": ["id"],
    "post": ["id", "uid"],
    "comment": ["id", "post_id"],
    "media_reference": ["owner_type", "owner_id", "media_type", "url"],
    "checkpoint": ["cursor", "fetched_count", "has_more"],
    "rate_limited": ["retryable"],
    "auth_required": ["code"],
    "warning": ["message"],
    "error": ["code", "retryable"],
    "done": ["status", "fetched_count", "has_more"],
    "cancelled": ["request_id"],
}

# 枚举约束（dtos.schema.json）
_CONTENT_STATUS = {"partial", "complete"}
_MEDIA_OWNER_TYPES = {"post", "user", "comment"}
_MEDIA_TYPES = {"picture", "video", "avatar", "emoji"}
_DONE_STATUS = {"completed", "stopped"}
_ERROR_SCOPE = {"request", "stream", "sidecar"}
_RATE_LIMITED_SCOPE = {"account", "endpoint", "request"}
_AUTH_STATE = {"unknown", "authenticated", "guest"}

_COMMANDS = {
    "hello",
    "health",
    "collect_user_posts",
    "collect_comments",
    "collect_comment_replies",
    "cancel",
    "shutdown",
}


def validate_payload(event_type: str, payload: dict) -> list[str]:
    """只校验一个 payload（不校验信封字段），供抽取器测试复用。"""
    errors: list[str] = []
    if not isinstance(payload, dict):
        return ["payload must be an object"]
    _validate_payload(event_type, payload, errors)
    return errors


def validate_event(envelope: dict) -> list[str]:
    """校验一个事件信封（含 payload），返回错误消息列表。"""
    errors: list[str] = []

    if not isinstance(envelope, dict):
        return ["event must be a JSON object"]

    if envelope.get("protocol_version") != PROTOCOL_VERSION:
        errors.append(
            f"protocol_version must be {PROTOCOL_VERSION}, got {envelope.get('protocol_version')!r}"
        )

    event_id = envelope.get("event_id")
    if not isinstance(event_id, str) or not is_uuid_v7(event_id):
        errors.append(f"event_id must be a UUID v7 string, got {event_id!r}")

    request_id = envelope.get("request_id")
    if request_id is not None and (not isinstance(request_id, str) or not is_uuid_v7(request_id)):
        errors.append(f"request_id must be null or UUID v7, got {request_id!r}")

    event_type = envelope.get("type")
    if event_type not in EVENT_TYPES:
        errors.append(f"type must be one of {sorted(EVENT_TYPES)}, got {event_type!r}")
        return errors

    if not isinstance(envelope.get("occurred_at"), str) or not envelope.get("occurred_at"):
        errors.append("occurred_at must be a non-empty string")

    stream = envelope.get("stream")
    if stream is not None and not STREAM_RE.match(stream):
        errors.append(f"stream must match {STREAM_RE.pattern}, got {stream!r}")

    sequence = envelope.get("sequence")
    if sequence is not None and (not isinstance(sequence, int) or sequence < 1):
        errors.append(f"sequence must be an integer >= 1, got {sequence!r}")

    total_expected = envelope.get("total_expected")
    if total_expected is not None and (not isinstance(total_expected, int) or total_expected < 0):
        errors.append(f"total_expected must be a non-negative integer, got {total_expected!r}")

    payload = envelope.get("payload")
    if not isinstance(payload, dict):
        errors.append("payload must be an object")
        return errors

    _validate_payload(event_type, payload, errors)
    return errors


def _validate_payload(event_type: str, payload: dict, errors: list[str]) -> None:
    for field in _REQUIRED_PAYLOAD.get(event_type, ()):
        if field not in payload:
            errors.append(f"payload missing required field {field!r} for type {event_type!r}")

    if event_type == "ready":
        if payload.get("sidecar_name") != "weiback-collector":
            errors.append("ready.sidecar_name must be 'weiback-collector'")
        if payload.get("protocol_version") != PROTOCOL_VERSION:
            errors.append("ready.protocol_version must be 1")

    elif event_type == "capabilities":
        versions = payload.get("protocol_versions")
        if not isinstance(versions, list) or PROTOCOL_VERSION not in versions:
            errors.append("capabilities.protocol_versions must include 1")
        commands = payload.get("commands")
        if isinstance(commands, list):
            for cmd in commands:
                if cmd not in _COMMANDS:
                    errors.append(f"capabilities.commands contains unknown command {cmd!r}")
        else:
            errors.append("capabilities.commands must be an array")
        auth_state = payload.get("auth_state")
        if auth_state is not None and auth_state not in _AUTH_STATE:
            errors.append(f"capabilities.auth_state must be one of {sorted(_AUTH_STATE)}")

    elif event_type == "post":
        content_status = payload.get("content_status")
        if content_status is not None and content_status not in _CONTENT_STATUS:
            errors.append(
                f"post.content_status must be one of {sorted(_CONTENT_STATUS)}, got {content_status!r}"
            )

    elif event_type == "comment":
        depth = payload.get("depth")
        if not isinstance(depth, int) or depth < 0:
            errors.append(f"comment.depth must be an integer >= 0, got {depth!r}")

    elif event_type == "media_reference":
        owner_type = payload.get("owner_type")
        if owner_type not in _MEDIA_OWNER_TYPES:
            errors.append(f"media_reference.owner_type must be one of {sorted(_MEDIA_OWNER_TYPES)}")
        media_type = payload.get("media_type")
        if media_type not in _MEDIA_TYPES:
            errors.append(f"media_reference.media_type must be one of {sorted(_MEDIA_TYPES)}")
        if not isinstance(payload.get("url"), str) or not payload.get("url"):
            errors.append("media_reference.url must be a non-empty string")

    elif event_type == "checkpoint":
        cursor = payload.get("cursor")
        if not isinstance(cursor, dict):
            errors.append("checkpoint.cursor must be an object")
        else:
            if "max_id" not in cursor:
                errors.append("checkpoint.cursor.max_id is required")
            if not isinstance(cursor.get("max_id_type"), int):
                errors.append("checkpoint.cursor.max_id_type must be an integer")
        if not isinstance(payload.get("has_more"), bool):
            errors.append("checkpoint.has_more must be a boolean")

    elif event_type == "rate_limited":
        if payload.get("retryable") is not True:
            errors.append("rate_limited.retryable must be true")
        scope = payload.get("scope")
        if scope is not None and scope not in _RATE_LIMITED_SCOPE:
            errors.append(f"rate_limited.scope must be one of {sorted(_RATE_LIMITED_SCOPE)}")

    elif event_type == "auth_required":
        if payload.get("code") != "AUTH_REQUIRED":
            errors.append("auth_required.code must be 'AUTH_REQUIRED'")

    elif event_type == "error":
        if not isinstance(payload.get("code"), str) or not payload.get("code"):
            errors.append("error.code must be a non-empty string")
        if not isinstance(payload.get("retryable"), bool):
            errors.append("error.retryable must be a boolean")
        scope = payload.get("scope")
        if scope is not None and scope not in _ERROR_SCOPE:
            errors.append(f"error.scope must be one of {sorted(_ERROR_SCOPE)}")

    elif event_type == "done":
        if payload.get("status") not in _DONE_STATUS:
            errors.append(f"done.status must be one of {sorted(_DONE_STATUS)}")
        if not isinstance(payload.get("has_more"), bool):
            errors.append("done.has_more must be a boolean")
        fetched_count = payload.get("fetched_count")
        if not isinstance(fetched_count, int) or isinstance(fetched_count, bool):
            errors.append("done.fetched_count must be a non-negative integer")
        elif fetched_count < 0:
            errors.append("done.fetched_count must be a non-negative integer")

    elif event_type == "cancelled":
        if not isinstance(payload.get("request_id"), str):
            errors.append("cancelled.request_id must be a string")


def validate_fixture_events(events: list[dict]) -> list[str]:
    """校验一串事件（按 stream 分组检查 sequence 单调递增）。"""
    errors: list[str] = []
    seq_tracker: dict[str, int] = {}
    for envelope in events:
        errors.extend(validate_event(envelope))
        stream = envelope.get("stream")
        sequence = envelope.get("sequence")
        if stream and sequence is not None:
            previous = seq_tracker.get(stream)
            if previous is not None and sequence <= previous:
                errors.append(f"sequence not monotonic in stream {stream!r}: {sequence} after {previous}")
            seq_tracker[stream] = sequence
    return errors


def fixture_comments_consistency(raw_comments: list[dict], post_id) -> list[str]:
    """辅助：校验从原始评论提取的 comment DTO 是否与 raw 内容一致（供契约 fixture 使用）。"""
    errors: list[str] = []
    for raw in raw_comments:
        dto = extract.extract_comment(raw, post_id, depth=1)
        errors.extend(validate_event({"type": "comment", "payload": dto, "stream": "x"}))
    return errors
