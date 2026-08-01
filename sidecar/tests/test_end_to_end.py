"""端到端测试：真实启动 Sidecar，完成握手与 fixture 采集流。"""

import json
import os
import subprocess
import sys
import time
import unittest

SIDECAR_DIR = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
FIXTURE_ROOT = os.path.normpath(os.path.join(SIDECAR_DIR, "..", "fixtures"))


def _send(process, line: str) -> None:
    assert process.stdin is not None
    process.stdin.write(line + "\n")
    process.stdin.flush()


def _read_event(process, timeout=10.0):
    assert process.stdout is not None
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        line = process.stdout.readline()
        if not line:
            return None
        line = line.strip()
        if line:
            return json.loads(line)
    return None


class EndToEndTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        env = os.environ.copy()
        env["WEIBACK_COLLECTOR_FIXTURE_DIR"] = FIXTURE_ROOT
        env["PYTHONUNBUFFERED"] = "1"
        cls.process = subprocess.Popen(
            [sys.executable, "-m", "weiback_collector"],
            cwd=SIDECAR_DIR,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
            env=env,
        )

    @classmethod
    def tearDownClass(cls):
        if cls.process.poll() is None:
            cls.process.kill()
            cls.process.wait(timeout=5)

    def test_handshake(self):
        _send(self.process, json.dumps({
            "protocol_version": 1,
            "request_id": "018f6c10-0000-7000-8000-0000000000aa",
            "type": "hello",
            "payload": {"client_name": "weiback-next", "client_version": "0.3.1"},
        }))
        ready = _read_event(self.process)
        self.assertIsNotNone(ready)
        self.assertEqual(ready["type"], "ready")
        self.assertEqual(ready["payload"]["sidecar_name"], "weiback-collector")
        capabilities = _read_event(self.process)
        self.assertIsNotNone(capabilities)
        self.assertEqual(capabilities["type"], "capabilities")
        self.assertIn(1, capabilities["payload"]["protocol_versions"])

    def test_fixture_collection_flow(self):
        _send(self.process, json.dumps({
            "protocol_version": 1,
            "request_id": "018f6c10-0000-7000-8000-0000000000ab",
            "type": "collect_user_posts",
            "payload": {"uid": "1234567890", "max_pages": 2},
        }))
        events_seen = []
        while True:
            event = _read_event(self.process)
            self.assertIsNotNone(event, "stream closed before done")
            events_seen.append(event)
            if event["type"] == "done":
                break

        self.assertEqual(events_seen[0]["type"], "started")
        self.assertEqual(events_seen[0]["stream"], "user:1234567890:posts")
        self.assertIn("post", [e["type"] for e in events_seen])
        self.assertEqual(events_seen[-1]["type"], "done")

    def test_invalid_json_yields_error_event(self):
        _send(self.process, "{ not valid json")
        event = _read_event(self.process)
        self.assertIsNotNone(event)
        self.assertEqual(event["type"], "error")
        self.assertEqual(event["payload"]["code"], "INVALID_COMMAND")

    def test_shutdown_exits_cleanly(self):
        _send(self.process, json.dumps({
            "protocol_version": 1,
            "request_id": "018f6c10-0000-7000-8000-0000000000ac",
            "type": "shutdown",
            "payload": {"grace_ms": 1000},
        }))
        code = self.process.wait(timeout=10)
        self.assertEqual(code, 0)


if __name__ == "__main__":
    unittest.main()
