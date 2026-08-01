"""headless JSONL 入口。

用法：
    python -m weiback_collector
从 stdin 逐行读取命令，向 stdout 输出事件，stderr 输出结构化日志。

环境变量：
    WEIBACK_COLLECTOR_FIXTURE_DIR   fixtures 根目录（默认仓库 fixtures/）
    WEIBACK_COLLECTOR_FIXTURE       指定 fixture 文件名（覆盖默认选择）
"""

from __future__ import annotations

import os
import sys

from . import events
from .commands import CommandDispatcher
from .fixture_source import FixtureSource
from .protocol import InvalidCommandError, PROTOCOL_VERSION, parse_command
from .uuid7 import uuid7


def default_fixture_dir() -> str:
    here = os.path.dirname(os.path.abspath(__file__))
    return os.path.join(os.path.dirname(os.path.dirname(here)), "fixtures")


def _emit_command_error(request_id: str | None, message: str) -> None:
    payload = {
        "code": "INVALID_COMMAND",
        "message": message,
        "retryable": False,
        "scope": "request",
    }
    envelope = {
        "protocol_version": PROTOCOL_VERSION,
        "request_id": request_id or uuid7(),
        "event_id": uuid7(),
        "type": "error",
        "occurred_at": events.utc_now(),
        "payload": payload,
    }
    events.write_event(envelope)


def _force_utf8_stdio() -> None:
    """stdin/stdout/stderr 一律 UTF-8，避免 Windows 默认代码页破坏 JSONL 帧。"""
    for stream in (sys.stdin, sys.stdout, sys.stderr):
        try:
            stream.reconfigure(encoding="utf-8")
        except (AttributeError, ValueError):
            pass


def main() -> int:
    _force_utf8_stdio()
    fixture_dir = os.environ.get(
        "WEIBACK_COLLECTOR_FIXTURE_DIR", default_fixture_dir()
    )
    source = FixtureSource(fixture_dir)
    dispatcher = CommandDispatcher(source, __import__("weiback_collector").__version__)
    events.log("info", "sidecar starting", fixture_dir=fixture_dir)

    running = True
    for raw in sys.stdin:
        line = raw.strip()
        if not line:
            continue
        try:
            command = parse_command(line)
        except InvalidCommandError as exc:
            events.log("warn", "invalid command line", error=str(exc))
            _emit_command_error(None, str(exc))
            continue

        try:
            running = dispatcher.dispatch(command)
        except Exception as exc:  # noqa: BLE001
            events.log("error", "dispatch raised", error=str(exc))
            _emit_command_error(command["request_id"], f"internal error: {exc}")
        if not running:
            break

    events.log("info", "sidecar exiting")
    return 0


if __name__ == "__main__":
    sys.exit(main())
