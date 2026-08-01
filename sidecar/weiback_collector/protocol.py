"""协议 v1 常量与命令信封解析。"""

from __future__ import annotations

import json
import re

PROTOCOL_VERSION = 1
MAX_MESSAGE_BYTES = 128 * 1024

_COMMANDS = {
    "hello",
    "health",
    "collect_user_posts",
    "collect_comments",
    "collect_comment_replies",
    "cancel",
    "shutdown",
}

_UUID_V7_RE = re.compile(
    r"^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$"
)


class ProtocolError(Exception):
    """协议层错误，message 可用于事件或诊断。"""


class InvalidCommandError(ProtocolError):
    """命令信封或 payload 非法。"""


def is_uuid_v7(value: str) -> bool:
    return bool(_UUID_V7_RE.match(value))


def serialize_command(command: dict) -> str:
    """把命令对象序列化为单行 JSON。"""
    return json.dumps(command, ensure_ascii=False, separators=(",", ":"))


def parse_command(line: str) -> dict:
    """解析 stdin 的一行命令信封。

    返回命令 dict（含 protocol_version/request_id/type/payload）。
    非法 JSON 或信封字段缺失/类型错误时抛 `InvalidCommandError`。
    """
    try:
        cmd = json.loads(line)
    except json.JSONDecodeError as e:
        raise InvalidCommandError(f"invalid json: {e}") from e

    if not isinstance(cmd, dict):
        raise InvalidCommandError("command must be a JSON object")

    version = cmd.get("protocol_version")
    if not isinstance(version, int) or version != PROTOCOL_VERSION:
        raise InvalidCommandError(
            f"unsupported protocol_version: {version!r}"
        )

    request_id = cmd.get("request_id")
    if not isinstance(request_id, str) or not is_uuid_v7(request_id):
        raise InvalidCommandError("request_id must be a UUID v7 string")

    command_type = cmd.get("type")
    if not isinstance(command_type, str) or command_type not in _COMMANDS:
        raise InvalidCommandError(f"unknown command type: {command_type!r}")

    payload = cmd.get("payload")
    if payload is not None and not isinstance(payload, dict):
        raise InvalidCommandError("payload must be an object or null")

    return cmd
