"""Fixture 驱动的假采集流。

P0-B 阶段不访问真实微博网络。`FixtureSource` 从仓库 `fixtures/` 目录读取
黄金 JSONL fixture，把其中的流事件重放为当前请求的事件。

重放规则：
- 信封字段（request_id/event_id/occurred_at/stream/sequence）由当前请求重新生成，
  payload 原样保留。
- 跳过握手期事件（ready/capabilities）与不属于当前流的数据事件。
- 命令携带 checkpoint 时，跳过已覆盖的数据事件（max_id 单调比较），
  从 checkpoint 之后的游标继续重放，模拟断点续传。
"""

from __future__ import annotations

import json
import os

from . import events
from .events import EventEmitter
from .protocol import ProtocolError, is_uuid_v7

# 命令类型 -> fixtures 子目录
CATEGORY_DIRS = {
    "collect_user_posts": "posts",
    "collect_comments": "comments",
    "collect_comment_replies": "comments",
}

# 数据事件类型（fixture 里需按流重放的事件）
_STREAM_EVENT_TYPES = {
    "started",
    "user",
    "post",
    "comment",
    "media_reference",
    "checkpoint",
    "progress",
    "rate_limited",
    "auth_required",
    "warning",
    "error",
    "done",
    "cancelled",
}


class FixtureSource:
    def __init__(self, fixture_dir: str) -> None:
        self.fixture_dir = fixture_dir

    def resolve_file(self, category: str) -> str | None:
        """选择类别目录下的 fixture 文件。

        优先 `WEIBACK_COLLECTOR_FIXTURE` 环境变量指定文件名；否则取该目录下
        按字典序第一个 `*.jsonl`。
        """
        override = os.environ.get("WEIBACK_COLLECTOR_FIXTURE")
        if override:
            candidate = os.path.join(self.fixture_dir, override)
            if os.path.isfile(candidate):
                return candidate
            if not os.path.isabs(override):
                candidate = os.path.join(
                    self.fixture_dir, category, override
                )
            if os.path.isfile(candidate):
                return candidate

        directory = os.path.join(self.fixture_dir, category)
        if not os.path.isdir(directory):
            return None
        files = sorted(
            f for f in os.listdir(directory) if f.endswith(".jsonl")
        )
        if not files:
            return None
        return os.path.join(directory, files[0])

    def replay(
        self,
        emitter: EventEmitter,
        category: str,
        stream: str,
        *,
        checkpoint: dict | None = None,
        is_replies: bool = False,
    ) -> int:
        """把选定 fixture 的事件流重放到 emitter。

        返回重放的事件条数。fixture 缺失或为空时抛出 ProtocolError。
        """
        path = self.resolve_file(category)
        if path is None:
            raise ProtocolError(f"no fixture found for category: {category}")

        records = self._read_records(path)
        skip_before = self._locate_resume_point(records, checkpoint)

        count = 0
        for record in records[skip_before:]:
            if not isinstance(record, dict):
                continue
            event_type = record.get("type")
            if event_type not in _STREAM_EVENT_TYPES:
                continue
            if event_type in ("started",):
                emitter.emit(
                    "started",
                    {"stream": stream},
                    stream=stream,
                    total_expected=self._total_expected(record),
                )
                count += 1
                continue
            payload = record.get("payload") or {}
            if not isinstance(payload, dict):
                continue
            if event_type == "error" and payload.get("code") == "REQUEST_CANCELLED":
                emitter.emit(
                    "done",
                    {"status": "stopped", "fetched_count": count, "has_more": True},
                    stream=stream,
                )
                count += 1
                return count
            emitter.emit(
                event_type,
                payload,
                stream=stream,
                total_expected=self._total_expected(record),
            )
            count += 1
            if event_type == "done":
                return count
        # fixture 未以 done 结尾（例如 partial_stopped 的 stopped 形态），补一个 done
        emitter.emit(
            "done",
            {"status": "completed", "fetched_count": count, "has_more": False},
            stream=stream,
        )
        count += 1
        return count

    @staticmethod
    def _read_records(path: str) -> list[dict]:
        records = []
        with open(path, encoding="utf-8") as handle:
            for raw in handle:
                line = raw.strip()
                if not line:
                    continue
                record = json.loads(line)
                if isinstance(record, dict):
                    records.append(record)
        return records

    @staticmethod
    def _total_expected(record: dict) -> int | None:
        value = record.get("total_expected")
        return value if isinstance(value, int) else None

    @staticmethod
    def _locate_resume_point(records: list[dict], checkpoint: dict | None) -> int:
        """命令 checkpoint 匹配 fixture 中某个 checkpoint 游标时，
        跳过该 checkpoint 及其之前的事件，模拟断点续传。"""
        if not checkpoint:
            return 0
        target = checkpoint.get("max_id")
        if target is None:
            return 0
        for index, record in enumerate(records):
            if record.get("type") != "checkpoint":
                continue
            payload_cursor = (record.get("payload") or {}).get("cursor") or {}
            if payload_cursor.get("max_id") == target:
                return index + 1
        return 0
