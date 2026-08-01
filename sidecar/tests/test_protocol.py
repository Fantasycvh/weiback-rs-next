"""协议解析测试：合法命令、非法 JSON、版本不匹配、未知类型。"""

import json
import unittest
from io import StringIO
from unittest.mock import patch

from weiback_collector import events
from weiback_collector.protocol import (
    InvalidCommandError,
    parse_command,
    serialize_command,
)


def valid_command(**overrides):
    command = {
        "protocol_version": 1,
        "request_id": "018f6c10-0000-7000-8000-00000000000a",
        "type": "hello",
        "payload": {"client_name": "weiback-next", "client_version": "0.3.1"},
    }
    command.update(overrides)
    return command


class ParseCommandTest(unittest.TestCase):
    def test_hello_command_round_trip(self):
        command = valid_command()
        parsed = parse_command(serialize_command(command))
        self.assertEqual(parsed, command)

    def test_invalid_json_raises(self):
        with self.assertRaises(InvalidCommandError):
            parse_command("not json {")

    def test_non_object_raises(self):
        with self.assertRaises(InvalidCommandError):
            parse_command(json.dumps([1, 2, 3]))

    def test_wrong_protocol_version_raises(self):
        line = serialize_command(valid_command(protocol_version=2))
        with self.assertRaises(InvalidCommandError):
            parse_command(line)

    def test_unknown_command_type_raises(self):
        line = serialize_command(valid_command(type="teleport"))
        with self.assertRaises(InvalidCommandError):
            parse_command(line)

    def test_invalid_request_id_raises(self):
        line = serialize_command(valid_command(request_id="not-a-uuid"))
        with self.assertRaises(InvalidCommandError):
            parse_command(line)

    def test_payload_must_be_object(self):
        line = serialize_command(valid_command(payload=[1]))
        with self.assertRaises(InvalidCommandError):
            parse_command(line)

    def test_absent_protocol_version_raises(self):
        command = valid_command()
        command.pop("protocol_version")
        with self.assertRaises(InvalidCommandError):
            parse_command(json.dumps(command))


class EventEnvelopeTest(unittest.TestCase):
    def test_event_has_required_envelope_fields(self):
        out = StringIO()
        with patch("sys.stdout", out):
            emitter = events.EventEmitter("018f6c10-0000-7000-8000-00000000000a")
            emitter.emit("post", {"id": "1", "uid": "2"}, stream="user:2:posts")

        record = json.loads(out.getvalue().strip())
        self.assertEqual(record["protocol_version"], 1)
        self.assertEqual(record["type"], "post")
        self.assertEqual(record["stream"], "user:2:posts")
        self.assertEqual(record["sequence"], 1)
        self.assertTrue(record["occurred_at"])
        self.assertRegex(record["event_id"], r"^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-")

    def test_sequence_increments_per_stream_event(self):
        out = StringIO()
        with patch("sys.stdout", out):
            emitter = events.EventEmitter("018f6c10-0000-7000-8000-00000000000a")
            for _ in range(3):
                emitter.emit("post", {"id": "1", "uid": "2"}, stream="user:2:posts")

        sequences = [
            json.loads(line)["sequence"]
            for line in out.getvalue().strip().splitlines()
        ]
        self.assertEqual(sequences, [1, 2, 3])

    def test_oversized_event_raises(self):
        with self.assertRaises(ValueError):
            out = StringIO()
            with patch("sys.stdout", out):
                emitter = events.EventEmitter("018f6c10-0000-7000-8000-00000000000a")
                emitter.emit(
                    "post",
                    {"id": "x" * (events.MAX_MESSAGE_BYTES)},
                    stream="user:2:posts",
                )


if __name__ == "__main__":
    unittest.main()
