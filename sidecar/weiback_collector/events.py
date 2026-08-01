"""事件构造与输出。

stdout 只允许输出协议事件（每行一个 JSON 对象）。stderr 用于结构化诊断日志。
"""

from __future__ import annotations

import datetime
import json
import sys
from typing import Any

from .protocol import MAX_MESSAGE_BYTES, PROTOCOL_VERSION
from .uuid7 import uuid7


def utc_now() -> str:
    """RFC3339 UTC 时间，毫秒精度，供 occurred_at 使用（仅诊断）。"""
    now = datetime.datetime.now(datetime.timezone.utc)
    return now.isoformat(timespec="milliseconds").replace("+00:00", "Z")


class EventEmitter:
    """按当前请求维护 sequence 并输出事件。"""

    def __init__(self, request_id: str) -> None:
        self.request_id = request_id
        self._sequence = 0

    def emit(
        self,
        event_type: str,
        payload: dict[str, Any],
        *,
        stream: str | None = None,
        total_expected: int | None = None,
        sequence: bool = True,
    ) -> None:
        envelope: dict[str, Any] = {
            "protocol_version": PROTOCOL_VERSION,
            "request_id": self.request_id,
            "event_id": uuid7(),
            "type": event_type,
            "occurred_at": utc_now(),
        }
        if stream is not None:
            envelope["stream"] = stream
        if total_expected is not None:
            envelope["total_expected"] = total_expected
        if sequence and stream is not None:
            self._sequence += 1
            envelope["sequence"] = self._sequence
        envelope["payload"] = payload
        write_event(envelope)

    def progress(self, phase: str, message: str | None = None) -> None:
        payload: dict[str, Any] = {"phase": phase}
        if message is not None:
            payload["message"] = message
        self.emit("progress", payload, stream=None)


def write_event(event: dict[str, Any]) -> None:
    """写一条协议事件到 stdout，超限时报错（调用方决定拆批策略）。"""
    line = json.dumps(event, ensure_ascii=False, separators=(",", ":"))
    if len(line.encode("utf-8")) > MAX_MESSAGE_BYTES:
        raise ValueError(
            f"event exceeds {MAX_MESSAGE_BYTES} bytes: type={event.get('type')}"
        )
    sys.stdout.write(line + "\n")
    sys.stdout.flush()


def log(level: str, msg: str, **extra: Any) -> None:
    """结构化诊断日志（stderr），禁止输出认证秘密。"""
    record: dict[str, Any] = {"ts": utc_now(), "level": level, "msg": msg}
    record.update(extra)
    sys.stderr.write(json.dumps(record, ensure_ascii=False) + "\n")
    sys.stderr.flush()
