"""协议契约测试：全部采集 fixture 必须通过协议 schema 校验。

P1-A 交付物：
- "现有采集 fixture 全部通过协议契约测试"。
- "stdout 不出现日志或 Cookie"：校验 fixture 行中不含常见认证秘密。
"""

import json
import os
import unittest

from weiback_collector import contract
from weiback_collector.extract import extract_comment, unpack_child_comment_page
from weiback_collector.protocol import is_uuid_v7

SIDECAR_DIR = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
FIXTURE_ROOT = os.path.normpath(os.path.join(SIDECAR_DIR, "..", "fixtures"))

# 认证秘密特征：一旦出现在 stdout 事件行即为违规
_SECRET_MARKERS = ("cookie", "Cookie", "token", "passport", "XSRF", "SUB=", "gsid")


def iter_fixture_events():
    """遍历 fixtures 下所有 .jsonl，产出 (相对路径, 事件 dict)。"""
    for root, _dirs, files in os.walk(FIXTURE_ROOT):
        for name in sorted(files):
            if not name.endswith(".jsonl"):
                continue
            path = os.path.join(root, name)
            rel = os.path.relpath(path, FIXTURE_ROOT)
            with open(path, encoding="utf-8") as handle:
                for lineno, raw in enumerate(handle, 1):
                    line = raw.strip()
                    if not line:
                        continue
                    event = json.loads(line)
                    yield rel, lineno, event


class FixtureContractTest(unittest.TestCase):
    def test_all_fixture_events_pass_contract(self):
        """每个 fixture 事件都通过协议契约校验（无错误）。"""
        checked = 0
        all_errors = []
        for rel, lineno, event in iter_fixture_events():
            checked += 1
            errors = contract.validate_event(event)
            if errors:
                all_errors.append(f"{rel}:{lineno}: {errors}")
        self.assertGreater(checked, 0, "no fixture events checked")
        self.assertEqual(all_errors, [], "\n".join(all_errors))

    def test_no_secret_markers_on_event_lines(self):
        """stdout 事件行不得出现 Cookie/token 等认证秘密特征。"""
        offenders = []
        for rel, lineno, event in iter_fixture_events():
            raw = json.dumps(event, ensure_ascii=False)
            for marker in _SECRET_MARKERS:
                if marker in raw:
                    offenders.append(f"{rel}:{lineno}: contains {marker!r}")
        self.assertEqual(offenders, [], "\n".join(offenders))

    def test_sequence_monotonic_per_stream(self):
        """同一流内 sequence 必须单调递增（按文件内顺序）。"""
        offenders = []
        trackers = {}
        for rel, lineno, event in iter_fixture_events():
            stream = event.get("stream")
            sequence = event.get("sequence")
            if not stream or sequence is None:
                continue
            key = (rel, stream)
            previous = trackers.get(key)
            if previous is not None and sequence <= previous:
                offenders.append(f"{rel}:{lineno}: {stream} sequence {sequence} <= {previous}")
            trackers[key] = sequence
        self.assertEqual(offenders, [], "\n".join(offenders))


class RawHotFlowChildFixtureTest(unittest.TestCase):
    """hotFlowChild 两种信封格式原始 fixture 可解析为合规 comment DTO。"""

    def test_envelope_a_parses_to_valid_comments(self):
        path = os.path.join(FIXTURE_ROOT, "raw", "hotflowchild_envelope_a.json")
        with open(path, encoding="utf-8") as handle:
            response = json.load(handle)
        items, max_id, max_id_type = unpack_child_comment_page(response)
        self.assertEqual(len(items), 2)
        self.assertEqual(max_id, "0")
        self.assertEqual(max_id_type, 0)
        for item in items:
            dto = extract_comment(item, "5550000000000000200", depth=1)
            errors = contract.validate_payload("comment", dto)
            self.assertEqual(errors, [])

    def test_envelope_b_parses_to_valid_comments(self):
        path = os.path.join(FIXTURE_ROOT, "raw", "hotflowchild_envelope_b.json")
        with open(path, encoding="utf-8") as handle:
            response = json.load(handle)
        items, max_id, max_id_type = unpack_child_comment_page(response)
        self.assertEqual(len(items), 1)
        self.assertEqual(max_id, "0")
        for item in items:
            dto = extract_comment(item, "5550000000000000200", depth=1)
            errors = contract.validate_payload("comment", dto)
            self.assertEqual(errors, [])


class ContractValidatorTest(unittest.TestCase):
    def test_valid_event_passes(self):
        event = {
            "protocol_version": 1,
            "request_id": "018f6c10-0000-7000-8000-00000000000a",
            "event_id": "018f6c10-0000-7000-8000-000000000010",
            "type": "post",
            "stream": "user:123:posts",
            "sequence": 1,
            "occurred_at": "2026-08-01T04:00:00.000Z",
            "payload": {"id": "1", "uid": 123, "content_status": "complete"},
        }
        self.assertEqual(contract.validate_event(event), [])

    def test_missing_payload_required_field(self):
        event = {
            "protocol_version": 1,
            "request_id": "018f6c10-0000-7000-8000-00000000000a",
            "event_id": "018f6c10-0000-7000-8000-000000000010",
            "type": "post",
            "stream": "user:123:posts",
            "occurred_at": "2026-08-01T04:00:00.000Z",
            "payload": {"id": "1"},
        }
        errors = contract.validate_event(event)
        self.assertTrue(any("uid" in e for e in errors))

    def test_invalid_content_status(self):
        event = {
            "protocol_version": 1,
            "request_id": "018f6c10-0000-7000-8000-00000000000a",
            "event_id": "018f6c10-0000-7000-8000-000000000010",
            "type": "post",
            "stream": "user:123:posts",
            "occurred_at": "2026-08-01T04:00:00.000Z",
            "payload": {"id": "1", "uid": 123, "content_status": "bogus"},
        }
        self.assertTrue(any("content_status" in e for e in contract.validate_event(event)))

    def test_invalid_stream_pattern(self):
        event = {
            "protocol_version": 1,
            "request_id": "018f6c10-0000-7000-8000-00000000000a",
            "event_id": "018f6c10-0000-7000-8000-000000000010",
            "type": "post",
            "stream": "weird:stream",
            "occurred_at": "2026-08-01T04:00:00.000Z",
            "payload": {"id": "1", "uid": 123},
        }
        self.assertTrue(any("stream" in e for e in contract.validate_event(event)))

    def test_invalid_event_id(self):
        event = {
            "protocol_version": 1,
            "request_id": "018f6c10-0000-7000-8000-00000000000a",
            "event_id": "not-a-uuid",
            "type": "post",
            "occurred_at": "2026-08-01T04:00:00.000Z",
            "payload": {"id": "1", "uid": 123},
        }
        self.assertTrue(any("event_id" in e for e in contract.validate_event(event)))

    def test_media_reference_enums(self):
        event = {
            "protocol_version": 1,
            "request_id": "018f6c10-0000-7000-8000-00000000000a",
            "event_id": "018f6c10-0000-7000-8000-000000000010",
            "type": "media_reference",
            "occurred_at": "2026-08-01T04:00:00.000Z",
            "payload": {
                "owner_type": "bogus",
                "owner_id": "1",
                "media_type": "emoji",
                "url": "https://x/y.png",
            },
        }
        self.assertTrue(any("owner_type" in e for e in contract.validate_event(event)))

    def test_done_status_enum(self):
        event = {
            "protocol_version": 1,
            "request_id": "018f6c10-0000-7000-8000-00000000000a",
            "event_id": "018f6c10-0000-7000-8000-000000000010",
            "type": "done",
            "occurred_at": "2026-08-01T04:00:00.000Z",
            "payload": {"status": "nope", "fetched_count": 0, "has_more": False},
        }
        self.assertTrue(any("status" in e for e in contract.validate_event(event)))


if __name__ == "__main__":
    unittest.main()
