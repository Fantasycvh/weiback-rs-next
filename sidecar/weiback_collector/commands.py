"""命令分发：hello/health/collect_*/cancel/shutdown。"""

from __future__ import annotations

from typing import Any

from . import events
from .events import EventEmitter
from .collector import Collector
from .fixture_source import FixtureSource
from .protocol import PROTOCOL_VERSION

SIDECAR_NAME = "weiback-collector"

_COMMAND_CATEGORY = {
    "collect_user_posts": "posts",
    "collect_comments": "comments",
    "collect_comment_replies": "comments",
}


class CommandDispatcher:
    def __init__(
        self,
        fixture_source: FixtureSource | None,
        sidecar_version: str,
        collector: Collector | None = None,
    ) -> None:
        self.fixture_source = fixture_source
        self.sidecar_version = sidecar_version
        self.collector = collector
        self._default_max_pages = collector.max_pages if collector is not None else 10
        self._active_request: str | None = None
        self._browser_installed = False
        self._browser_version: str | None = None
        self._auth_state: str = "unknown"

    def dispatch(self, command: dict) -> bool:
        """处理一条命令。返回 False 表示 Sidecar 应退出。"""
        command_type = command["type"]
        request_id = command["request_id"]
        payload = command.get("payload") or {}

        handler = {
            "hello": self._hello,
            "health": self._health,
            "collect_user_posts": self._collect,
            "collect_comments": self._collect,
            "collect_comment_replies": self._collect,
            "cancel": self._cancel,
            "shutdown": self._shutdown,
        }[command_type]

        try:
            if command_type.startswith("collect_"):
                return handler(request_id, command_type, payload)
            return handler(request_id, payload)
        except Exception as exc:  # noqa: BLE001
            events.log("error", "command dispatch failed", type=command_type, error=str(exc))
            emitter = EventEmitter(request_id)
            emitter.emit(
                "error",
                {
                    "code": "INTERNAL_ERROR",
                    "message": str(exc),
                    "retryable": False,
                    "scope": "request",
                },
            )
            return True

    # -- 命令实现 --------------------------------------------------------

    def _hello(self, request_id: str, payload: dict) -> bool:
        emitter = EventEmitter(request_id)
        emitter.emit(
            "ready",
            {
                "sidecar_name": SIDECAR_NAME,
                "sidecar_version": self.sidecar_version,
                "protocol_version": PROTOCOL_VERSION,
            },
        )
        emitter.emit(
            "capabilities",
            {
                "protocol_versions": [PROTOCOL_VERSION],
                "commands": [
                    "hello",
                    "health",
                    "collect_user_posts",
                    "collect_comments",
                    "collect_comment_replies",
                    "cancel",
                    "shutdown",
                ],
                "browser_installed": self._browser_installed,
                "browser_version": self._browser_version,
                "auth_state": self._auth_state,
            },
        )
        return True

    def _health(self, request_id: str, payload: dict) -> bool:
        emitter = EventEmitter(request_id)
        emitter.emit(
            "capabilities",
            {
                "protocol_versions": [PROTOCOL_VERSION],
                "commands": [
                    "hello",
                    "health",
                    "collect_user_posts",
                    "collect_comments",
                    "collect_comment_replies",
                    "cancel",
                    "shutdown",
                ],
                "browser_installed": self._browser_installed,
                "browser_version": self._browser_version,
                "auth_state": self._auth_state,
            },
        )
        return True

    def _collect(
        self,
        request_id: str,
        command_type: str,
        payload: dict,
    ) -> bool:
        if self._active_request is not None:
            # 首期单任务；并发请求留给 P1，但协议层不假设全局单任务。
            emitter = EventEmitter(request_id)
            emitter.emit(
                "error",
                {
                    "code": "INVALID_COMMAND",
                    "message": f"request already active: {self._active_request}",
                    "retryable": False,
                    "scope": "request",
                },
            )
            return True

        stream = self._build_stream(command_type, payload)
        if stream is None:
            emitter = EventEmitter(request_id)
            emitter.emit(
                "error",
                {
                    "code": "INVALID_COMMAND",
                    "message": f"missing required parameter for {command_type}",
                    "retryable": False,
                    "scope": "request",
                },
            )
            return True

        self._active_request = request_id
        emitter = EventEmitter(request_id)
        try:
            events.log(
                "info",
                "collect started",
                type=command_type,
                request_id=request_id,
                stream=stream,
            )
            if self.collector is not None:
                self._collect_live(emitter, command_type, payload)
            elif self.fixture_source is not None:
                self.fixture_source.replay(
                    emitter,
                    _COMMAND_CATEGORY[command_type],
                    stream,
                    checkpoint=payload.get("checkpoint"),
                    is_replies=command_type == "collect_comment_replies",
                )
            else:
                raise RuntimeError("no collector backend configured")
        finally:
            self._active_request = None
        return True

    def _collect_live(self, emitter: EventEmitter, command_type: str, payload: dict) -> None:
        assert self.collector is not None
        checkpoint = payload.get("checkpoint")
        max_pages = payload.get("max_pages")
        if isinstance(max_pages, int) and max_pages > 0:
            self.collector.max_pages = min(max_pages, 1000)
        else:
            self.collector.max_pages = self._default_max_pages
        if command_type == "collect_user_posts":
            self.collector.collect_posts(emitter, payload["uid"], checkpoint=checkpoint)
        elif command_type == "collect_comments":
            self.collector.collect_comments(emitter, payload["post_id"], checkpoint=checkpoint)
        else:
            self.collector.collect_replies(
                emitter,
                payload["post_id"],
                payload["root_comment_id"],
                checkpoint=checkpoint,
            )

    def _cancel(self, request_id: str, payload: dict) -> bool:
        target = payload.get("request_id")
        emitter = EventEmitter(request_id)
        if target == self._active_request:
            self._active_request = None
            emitter.emit(
                "cancelled",
                {"request_id": target, "fetched_count": 0},
            )
        else:
            emitter.emit(
                "error",
                {
                    "code": "INVALID_COMMAND",
                    "message": f"no active request: {target!r}",
                    "retryable": False,
                    "scope": "request",
                },
            )
        return True

    def _shutdown(self, request_id: str, payload: dict) -> bool:
        grace_ms = payload.get("grace_ms")
        if not isinstance(grace_ms, int) or grace_ms < 0:
            grace_ms = 3000
        events.log("info", "shutdown requested", grace_ms=grace_ms)
        self._active_request = None
        return False

    @staticmethod
    def _build_stream(command_type: str, payload: dict) -> str | None:
        if command_type == "collect_user_posts":
            uid = payload.get("uid")
            return f"user:{uid}:posts" if uid is not None else None
        if command_type == "collect_comments":
            post_id = payload.get("post_id")
            return f"post:{post_id}:comments" if post_id is not None else None
        if command_type == "collect_comment_replies":
            post_id = payload.get("post_id")
            root_id = payload.get("root_comment_id")
            if post_id is None or root_id is None:
                return None
            return f"post:{post_id}:comment:{root_id}:replies"
        return None
