"""collector.py 事件驱动采集编排的测试。"""

from __future__ import annotations

import json
import os
import unittest
from typing import Any, Callable

from weiback_collector import collector, contract
from weiback_collector.collector import Collector, FetchPage, FixtureFetchPage
from weiback_collector.upstream import BackoffPolicy, UpstreamError, classify_network_error

FIXTURE_ROOT = os.path.abspath(
    os.path.join(os.path.dirname(__file__), "..", "..", "fixtures")
)
RAW_ROOT = os.path.join(FIXTURE_ROOT, "raw")

_EVENT_ID = "019fbbd7-ea26-7b7c-b113-c89ac2788773"
_REQUEST_ID = "019fbbd7-ea26-7b7c-b113-c89ac2788773"


class BufferEmitter:
    """捕获事件的假 emitter（对齐 Emitter 接口）。"""

    def __init__(self, request_id: str = _REQUEST_ID) -> None:
        self.request_id = request_id
        self.events: list[dict] = []
        self._sequence = 0

    def emit(
        self,
        event_type: str,
        payload: dict,
        *,
        stream: str | None = None,
        total_expected: int | None = None,
        sequence: bool = True,
    ) -> None:
        envelope: dict = {
            "protocol_version": 1,
            "request_id": self.request_id,
            "event_id": _EVENT_ID,
            "type": event_type,
            "occurred_at": "2026-07-31T12:00:00.000Z",
        }
        if stream is not None:
            envelope["stream"] = stream
        if total_expected is not None:
            envelope["total_expected"] = total_expected
        if sequence and stream is not None:
            self._sequence += 1
            envelope["sequence"] = self._sequence
        envelope["payload"] = payload
        self.events.append(envelope)

    def progress(self, phase: str, message: str | None = None) -> None:
        self.emit("progress", {"phase": phase, "message": message}, stream=None)

    def types(self) -> list[str]:
        return [e["type"] for e in self.events]

    def events_of(self, event_type: str) -> list[dict]:
        return [e for e in self.events if e["type"] == event_type]


def _fast_backoff() -> BackoffPolicy:
    """零延迟退避，测试不等待。"""
    return BackoffPolicy(
        base_seconds=0.0, factor=1.0, max_seconds=0.0, jitter_ratio=0.0, max_attempts=3
    )


def _read_raw(name: str) -> dict:
    with open(os.path.join(RAW_ROOT, name), encoding="utf-8") as handle:
        return json.load(handle)


def _collector(fetch: FetchPage, **kwargs: Any) -> Collector:
    return Collector(fetch_page=fetch, backoff=_fast_backoff(), **kwargs)


class RepliesCollectTest(unittest.TestCase):
    def _collect(self, envelope: str) -> tuple[BufferEmitter, collector.CollectResult]:
        emitter = BufferEmitter()
        page = FixtureFetchPage(FIXTURE_ROOT, envelope=envelope)
        collector_inst = _collector(page, max_pages=5)
        result = collector_inst.collect_replies(
            emitter, "5550000000000000200", "c200000000000000000"
        )
        return emitter, result

    def test_envelope_a_replays_normalized_comments(self):
        emitter, result = self._collect("a")
        types = emitter.types()
        self.assertEqual(types[0], "started")
        self.assertEqual(types[-1], "done")
        self.assertEqual(result.status, "completed")
        comments = emitter.events_of("comment")
        self.assertEqual(len(comments), 2)
        self.assertEqual(comments[0]["payload"]["id"], "c200000000000000001")
        self.assertEqual(comments[0]["payload"]["post_id"], "5550000000000000200")
        self.assertEqual(comments[0]["payload"]["depth"], 1)
        self.assertEqual(comments[0]["payload"]["root_id"], "c200000000000000000")
        self.assertIn("回复", comments[0]["payload"]["text"])
        self.assertNotIn("<a", comments[0]["payload"]["text"])

    def test_envelope_b_replays_nested_comments(self):
        emitter, _result = self._collect("b")
        comments = emitter.events_of("comment")
        self.assertEqual(len(comments), 1)
        self.assertEqual(comments[0]["payload"]["id"], "c200000000000000003")

    def test_checkpoint_reports_terminal_cursor(self):
        emitter, _result = self._collect("a")
        checkpoints = emitter.events_of("checkpoint")
        self.assertEqual(len(checkpoints), 1)
        cursor = checkpoints[0]["payload"]["cursor"]
        self.assertEqual(cursor["max_id"], "0")
        self.assertEqual(checkpoints[0]["payload"]["has_more"], False)
        self.assertEqual(checkpoints[0]["payload"]["fetched_count"], 2)

    def test_done_reports_completed(self):
        emitter, _result = self._collect("a")
        done = emitter.events_of("done")[0]
        self.assertEqual(done["payload"]["status"], "completed")
        self.assertEqual(done["payload"]["fetched_count"], 2)

    def test_stream_events_pass_contract(self):
        emitter, _result = self._collect("a")
        expected_stream = (
            "post:5550000000000000200:comment:"
            "c200000000000000000:replies"
        )
        for event in emitter.events:
            issues = contract.validate_event(event)
            self.assertEqual(issues, [], f"contract violations: {issues}")
            self.assertEqual(event.get("stream"), expected_stream)


class PaginationTest(unittest.TestCase):
    def test_cursor_advances_across_pages(self):
        pages = [
            {"ok": 1, "data": [{"id": "r1"}], "max_id": "p1_after", "max_id_type": 0},
            {"ok": 1, "data": [{"id": "r2"}], "max_id": "0", "max_id_type": 0},
        ]
        calls: list[dict] = []

        def fetch(kind: str, params: dict) -> tuple[int, dict]:
            calls.append(params)
            index = len(calls) - 1
            return 200, pages[min(index, len(pages) - 1)]

        emitter = BufferEmitter()
        result = _collector(fetch, max_pages=5).collect_replies(emitter, "1", "root1")

        checkpoints = emitter.events_of("checkpoint")
        self.assertEqual(result.pages, 2)
        self.assertEqual(len(checkpoints), 2)
        self.assertEqual(checkpoints[0]["payload"]["cursor"]["max_id"], "p1_after")
        self.assertEqual(checkpoints[0]["payload"]["has_more"], True)
        self.assertEqual(checkpoints[1]["payload"]["cursor"]["max_id"], "0")
        self.assertEqual(checkpoints[1]["payload"]["has_more"], False)
        self.assertEqual(result.fetched_count, 2)
        # 第二页请求携带上一页游标
        self.assertEqual(calls[1]["max_id"], "p1_after")

    def test_resume_from_checkpoint_cursor(self):
        pages = [
            {"ok": 1, "data": [{"id": "r2"}], "max_id": "0", "max_id_type": 0},
        ]
        calls: list[dict] = []

        def fetch(kind: str, params: dict) -> tuple[int, dict]:
            calls.append(params)
            return 200, pages[0]

        emitter = BufferEmitter()
        # command.schema.json defines checkpoint as the cursor object itself.
        checkpoint = {
            "max_id": "p1_after",
            "max_id_type": 0,
            "fetched_count": 20,
        }
        result = _collector(fetch, max_pages=5).collect_replies(
            emitter, "1", "root1", checkpoint=checkpoint
        )
        self.assertEqual(calls[0]["max_id"], "p1_after")
        self.assertEqual(calls[0]["max_id_type"], 0)
        self.assertEqual(result.fetched_count, 21)
        comments = emitter.events_of("comment")
        self.assertEqual(comments[0]["payload"]["id"], "r2")

    def test_unknown_replies_envelope_fails_without_checkpoint(self):
        emitter = BufferEmitter()
        result = _collector(
            lambda kind, params: (200, {"ok": 1, "data": {"unexpected": []}}),
            max_pages=1,
        ).collect_replies(emitter, "1", "root1")

        self.assertEqual(result.status, "stopped")
        self.assertIn("error", emitter.types())
        self.assertEqual(emitter.events_of("checkpoint"), [])

    def test_max_pages_bounds_pagination(self):
        calls: list[int] = []

        def fetch(kind: str, params: dict) -> tuple[int, dict]:
            calls.append(1)
            return 200, {
                "ok": 1,
                "data": [{"id": f"r{len(calls)}"}],
                "max_id": "more",
                "max_id_type": 0,
            }

        emitter = BufferEmitter()
        result = _collector(fetch, max_pages=3).collect_comments(emitter, "1")
        self.assertEqual(result.pages, 3)
        self.assertEqual(len(calls), 3)


class UpstreamErrorTest(unittest.TestCase):
    def test_raised_network_error_retries_then_succeeds(self):
        calls: list[int] = []

        def fetch(kind: str, params: dict) -> tuple[int, dict]:
            calls.append(1)
            if len(calls) == 1:
                raise UpstreamError(classify_network_error("connection reset"))
            return 200, {"statuses": [], "since_id": "0"}

        emitter = BufferEmitter()
        result = _collector(fetch, max_pages=2).collect_posts(emitter, "123")
        self.assertEqual(len(calls), 2)
        self.assertEqual(result.status, "completed")
        self.assertIn("warning", emitter.types())

    def test_rate_limited_retries_then_succeeds(self):
        calls: list[int] = []

        def fetch(kind: str, params: dict) -> tuple[int, dict]:
            calls.append(1)
            if len(calls) == 1:
                return 429, {"msg": "rate limited"}
            return 200, {"ok": 1, "data": [{"id": "r1"}], "max_id": "0", "max_id_type": 0}

        emitter = BufferEmitter()
        result = _collector(fetch, max_pages=5).collect_comments(emitter, "1")
        types = emitter.types()
        self.assertIn("rate_limited", types)
        self.assertEqual(types[-1], "done")
        self.assertEqual(result.status, "completed")
        self.assertEqual(result.fetched_count, 1)
        rate_limited = emitter.events_of("rate_limited")[0]["payload"]
        self.assertEqual(rate_limited["code"], "UPSTREAM_RATE_LIMITED")
        self.assertTrue(rate_limited["retryable"])

    def test_rate_limit_exhaustion_emits_terminal_error(self):
        def fetch(kind: str, params: dict) -> tuple[int, dict]:
            return 429, {"msg": "rate limited"}

        backoff = BackoffPolicy(
            base_seconds=0.0,
            factor=1.0,
            max_seconds=0.0,
            jitter_ratio=0.0,
            max_attempts=1,
        )
        emitter = BufferEmitter()
        result = Collector(fetch_page=fetch, backoff=backoff).collect_posts(emitter, "123")
        self.assertEqual(result.status, "stopped")
        self.assertEqual(emitter.events_of("error")[0]["payload"]["code"], "UPSTREAM_RATE_LIMITED")

    def test_auth_required_stops_and_marks_guest(self):
        def fetch(kind: str, params: dict) -> tuple[int, dict]:
            return 403, {"msg": "forbidden"}

        collector_inst = _collector(fetch, max_pages=5)
        emitter = BufferEmitter()
        result = collector_inst.collect_posts(emitter, "123")
        types = emitter.types()
        self.assertIn("auth_required", types)
        self.assertEqual(collector_inst.auth.value, "guest")
        self.assertEqual(result.status, "stopped")
        auth_event = emitter.events_of("auth_required")[0]
        self.assertEqual(auth_event["payload"]["code"], "AUTH_REQUIRED")
        self.assertEqual(contract.validate_event(auth_event), [])
        done = emitter.events_of("done")[0]
        self.assertEqual(done["payload"]["status"], "stopped")

    def test_non_retryable_error_emits_error_then_stopped(self):
        def fetch(kind: str, params: dict) -> tuple[int, dict]:
            return 502, {"msg": "bad gateway"}

        emitter = BufferEmitter()
        result = _collector(fetch, max_pages=5).collect_posts(emitter, "123")
        types = emitter.types()
        self.assertIn("error", types)
        self.assertEqual(result.status, "stopped")
        error_payload = emitter.events_of("error")[0]["payload"]
        self.assertEqual(error_payload["code"], "UPSTREAM_ERROR")
        done = emitter.events_of("done")[0]
        self.assertEqual(done["payload"]["status"], "stopped")

    def test_retryable_network_aborts_after_max_attempts(self):
        calls: list[int] = []

        def fetch(kind: str, params: dict) -> tuple[int, dict]:
            calls.append(1)
            return 504, {"msg": "gateway timeout"}

        backoff = BackoffPolicy(
            base_seconds=0.0, factor=1.0, max_seconds=0.0, jitter_ratio=0.0, max_attempts=2
        )
        emitter = BufferEmitter()
        collector_inst = Collector(fetch_page=fetch, backoff=backoff, max_pages=5)
        result = collector_inst.collect_posts(emitter, "123")
        self.assertEqual(len(calls), 2)
        self.assertEqual(result.status, "stopped")
        assert result.last_error is not None
        self.assertEqual(result.last_error.code, "UPSTREAM_ERROR")


class PostsAndCommentsExtractTest(unittest.TestCase):
    def test_collect_posts_emits_post_and_media(self):
        page1 = {
            "statuses": [
                {
                    "id": "4242424242424242",
                    "idstr": "4242424242424242",
                    "user": {"id": "1234567890", "screen_name": "用户A"},
                    "text": "带图 <a href=\"//weibo.com\">@用户B</a> 的帖子",
                    "created_at": "Sat Aug 01 12:00:00 +0800 2026",
                    "pic_infos": {
                        "p1": {
                            "original_pic": {"url": "https://wx4.sinaimg.cn/original/abc.jpg"},
                            "large_pic": {"url": "https://wx4.sinaimg.cn/large/abc.jpg"},
                        }
                    },
                    "attitudes_count": 3,
                    "reposts_count": 1,
                    "comments_count": 2,
                }
            ],
            "page": 1,
            "total": 1,
        }
        page2 = {"statuses": [], "page": 2, "total": 1}

        def fetch(kind: str, params: dict) -> tuple[int, dict]:
            if params.get("max_id") == "2":
                return 200, page2
            return 200, page1

        emitter = BufferEmitter()
        result = _collector(fetch, max_pages=2).collect_posts(emitter, "1234567890")
        posts = emitter.events_of("post")
        self.assertEqual(len(posts), 1)
        users = emitter.events_of("user")
        self.assertEqual(len(users), 1)
        self.assertEqual(users[0]["payload"]["id"], "1234567890")
        self.assertEqual(posts[0]["payload"]["uid"], 1234567890)
        self.assertIn("@用户B", posts[0]["payload"]["text"])
        self.assertNotIn("<a", posts[0]["payload"]["text"])

        refs = emitter.events_of("media_reference")
        self.assertGreaterEqual(len(refs), 1)
        self.assertEqual(refs[0]["payload"]["owner_type"], "post")
        self.assertEqual(refs[0]["payload"]["owner_id"], "4242424242424242")
        self.assertEqual(refs[0]["payload"]["media_type"], "picture")
        self.assertEqual(result.fetched_count, 1)
        self.assertEqual(result.status, "completed")

    def test_collect_comments_emits_comment_and_media(self):
        body = {
            "data": [
                {
                    "id": "100000000000000001",
                    "text": "一级评论",
                    "created_at": "Sat Aug 01 13:00:00 +0800 2026",
                    "user": {"id": "2000000021", "screen_name": "评论者"},
                    "like_counts": 4,
                    "pic": {"large": {"url": "https://wx4.sinaimg.cn/comment.jpg"}},
                    "total_number": 0,
                }
            ],
            "max_id": "0",
            "max_id_type": 0,
        }

        def fetch(kind: str, params: dict) -> tuple[int, dict]:
            return 200, body

        emitter = BufferEmitter()
        result = _collector(fetch, max_pages=2).collect_comments(emitter, "5550000000000000200")
        self.assertEqual(len(emitter.events_of("user")), 1)
        comments = emitter.events_of("comment")
        self.assertEqual(len(comments), 1)
        self.assertEqual(comments[0]["payload"]["depth"], 0)
        self.assertEqual(comments[0]["payload"]["like_count"], 4)
        refs = emitter.events_of("media_reference")
        self.assertEqual(len(refs), 1)
        self.assertEqual(refs[0]["payload"]["owner_type"], "comment")
        self.assertEqual(refs[0]["payload"]["url"], "https://wx4.sinaimg.cn/comment.jpg")
        self.assertEqual(result.fetched_count, 1)


if __name__ == "__main__":
    unittest.main()
