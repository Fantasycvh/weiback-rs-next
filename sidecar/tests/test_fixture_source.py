"""Fixture 驱动的假采集流测试（使用仓库黄金 fixture）。"""

import json
import os
import unittest
from io import StringIO
from unittest.mock import patch

from weiback_collector import events
from weiback_collector.fixture_source import FixtureSource

FIXTURE_ROOT = os.path.normpath(
    os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "..", "fixtures")
)

REQUEST_ID = "018f6c10-0000-7000-8000-00000000000a"
NEW_REQUEST_ID = "018f6c10-0000-7000-8000-0000000000ff"


def _replay(category, stream, *, checkpoint=None, request_id=NEW_REQUEST_ID):
    source = FixtureSource(FIXTURE_ROOT)
    out = StringIO()
    with patch("sys.stdout", out):
        emitter = events.EventEmitter(request_id)
        count = source.replay(
            emitter, category, stream, checkpoint=checkpoint
        )
    lines = [json.loads(line) for line in out.getvalue().strip().splitlines()]
    return count, lines


class FixtureReplayTest(unittest.TestCase):
    def test_posts_fixture_replays_complete_stream(self):
        with patch.dict(
            os.environ,
            {"WEIBACK_COLLECTOR_FIXTURE": "posts/long_text_full.jsonl"},
        ):
            count, lines = _replay("posts", "user:1234567890:posts")
        self.assertGreater(count, 0)
        self.assertEqual(lines[0]["type"], "started")
        self.assertEqual(lines[0]["stream"], "user:1234567890:posts")
        types = [line["type"] for line in lines]
        self.assertIn("post", types)
        avatar_refs = [
            line for line in lines
            if line["type"] == "media_reference"
            and line["payload"]["media_type"] == "avatar"
        ]
        self.assertEqual(len(avatar_refs), 1)
        self.assertTrue(avatar_refs[0]["payload"]["url"].startswith("https://"))
        self.assertEqual(lines[-1]["type"], "done")

    def test_envelope_is_rewritten_for_current_request(self):
        _, lines = _replay("posts", "user:1234567890:posts")
        for line in lines:
            self.assertEqual(line["request_id"], NEW_REQUEST_ID)
            self.assertRegex(line["event_id"], r"-7[0-9a-f]{3}-[89ab]")
        # 原始 fixture 里的固定 request_id 已被替换
        self.assertTrue(
            all(line["request_id"] != REQUEST_ID for line in lines)
        )

    def test_sequence_is_monotonic_within_stream(self):
        _, lines = _replay("posts", "user:1234567890:posts")
        sequences = [
            line["sequence"]
            for line in lines
            if "sequence" in line
        ]
        self.assertEqual(sequences, sorted(sequences))
        self.assertEqual(sequences[0], 1)

    def test_checkpoint_resumes_from_matching_cursor(self):
        # 命令 checkpoint max_id='p2_after' 匹配 fixture 第二个 checkpoint，
        # 之前的事件（含 p1 checkpoint 及其数据）应被跳过。
        _, lines = _replay(
            "checkpoints",
            "user:1000000010:posts",
            checkpoint={"max_id": "p2_after", "max_id_type": 0},
        )
        checkpoint_max_ids = []
        for line in lines:
            if line["type"] == "checkpoint":
                checkpoint_max_ids.append(line["payload"]["cursor"]["max_id"])
        # 已提交游标 p2_after 及其之前的 checkpoint 全部跳过
        self.assertNotIn("p1_after", checkpoint_max_ids)
        self.assertNotIn("p2_after", checkpoint_max_ids)
        self.assertIn("p3_after", checkpoint_max_ids)
        self.assertEqual(lines[-1]["type"], "done")

    def test_missing_category_raises(self):
        source = FixtureSource(FIXTURE_ROOT)
        out = StringIO()
        with patch("sys.stdout", out):
            emitter = events.EventEmitter(REQUEST_ID)
            with self.assertRaises(Exception):
                source.replay(emitter, "does-not-exist", "user:1:posts")

    def test_reply_fixture_uses_replies_stream(self):
        count, lines = _replay("comments", "post:456:replies")
        self.assertGreater(count, 0)
        for line in lines:
            self.assertEqual(line.get("stream"), "post:456:replies")


if __name__ == "__main__":
    unittest.main()
