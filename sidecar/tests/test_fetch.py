"""真实 HTTP 边界的无网络单元测试。"""

import json
import os
import tempfile
import unittest
from email.message import Message
from urllib.error import HTTPError

from weiback_collector.collector import KIND_COMMENTS, KIND_REPLIES, KIND_USER_POSTS
from weiback_collector.fetch import WeiboHttpFetcher
from weiback_collector.upstream import UpstreamError


class FetcherTest(unittest.TestCase):
    def test_posts_request_and_normalization(self):
        captured = {}

        def open_url(request, timeout):
            captured["url"] = request.full_url
            body = {"data": {"cards": [
                {"card_type": 9, "mblog": {"id": 1}},
                {"card_type": 11},
            ], "cardlistInfo": {"since_id": "next"}}}
            return 200, json.dumps(body).encode()

        fetcher = WeiboHttpFetcher(open_url=open_url)
        status, body = fetcher(KIND_USER_POSTS, {"uid": "123", "max_id": "prev"})
        self.assertEqual(status, 200)
        self.assertEqual(body, {"statuses": [{"id": 1}], "since_id": "next"})
        self.assertIn("containerid=107603123", captured["url"])
        self.assertIn("since_id=prev", captured["url"])

    def test_comments_are_normalized(self):
        raw = {"data": {"data": [{"id": "c1"}], "max_id": 5, "max_id_type": 1}}
        fetcher = WeiboHttpFetcher(
            open_url=lambda request, timeout: (200, json.dumps(raw).encode())
        )
        _, body = fetcher(KIND_COMMENTS, {"post_id": 9})
        self.assertEqual(body["data"], [{"id": "c1"}])
        self.assertEqual(body["max_id"], 5)

    def test_replies_preserve_both_known_envelopes(self):
        raw = {"data": [{"id": "r1"}], "max_id": 0, "max_id_type": 0}
        fetcher = WeiboHttpFetcher(
            open_url=lambda request, timeout: (200, json.dumps(raw).encode())
        )
        _, body = fetcher(KIND_REPLIES, {"post_id": 9, "root_comment_id": "c1"})
        self.assertEqual(body, raw)

    def test_http_200_business_errors_are_classified(self):
        cases = [
            ({"ok": 0, "msg": "请先登录"}, 401),
            ({"ok": 0, "msg": "访问频次过高"}, 429),
            ({"ok": 0, "msg": "服务暂不可用"}, 502),
        ]
        for raw, expected_status in cases:
            with self.subTest(raw=raw):
                fetcher = WeiboHttpFetcher(
                    open_url=lambda request, timeout, raw=raw: (
                        200,
                        json.dumps(raw).encode(),
                    )
                )
                status, _ = fetcher(KIND_USER_POSTS, {"uid": 1})
                self.assertEqual(status, expected_status)

    def test_http_429_respects_retry_after_header(self):
        headers = Message()
        headers["Retry-After"] = "12"

        def open_url(request, timeout):
            raise HTTPError(request.full_url, 429, "limited", headers, None)

        fetcher = WeiboHttpFetcher(open_url=open_url)
        with self.assertRaises(UpstreamError) as context:
            fetcher(KIND_USER_POSTS, {"uid": 1})
        classification = context.exception.classification
        self.assertEqual(classification.code, "UPSTREAM_RATE_LIMITED")
        self.assertEqual(classification.retry_after_ms, 12_000)

    def test_invalid_success_json_is_schema_error(self):
        fetcher = WeiboHttpFetcher(open_url=lambda request, timeout: (200, b"not-json"))
        with self.assertRaises(UpstreamError) as context:
            fetcher(KIND_USER_POSTS, {"uid": 1})
        self.assertEqual(context.exception.classification.code, "RESPONSE_SCHEMA_CHANGED")

    def test_missing_posts_envelope_is_not_normalized_to_empty_success(self):
        raw = {"ok": 1, "data": {"unexpected": []}}
        fetcher = WeiboHttpFetcher(
            open_url=lambda request, timeout: (200, json.dumps(raw).encode())
        )
        _, body = fetcher(KIND_USER_POSTS, {"uid": 1})
        self.assertNotIn("statuses", body)

    def test_missing_comments_envelope_is_not_normalized_to_empty_success(self):
        raw = {"ok": 1, "data": {"unexpected": []}}
        fetcher = WeiboHttpFetcher(
            open_url=lambda request, timeout: (200, json.dumps(raw).encode())
        )
        _, body = fetcher(KIND_COMMENTS, {"post_id": 1})
        self.assertNotIsInstance(body.get("data"), list)

    def test_session_auth_is_used_but_not_exposed_in_body(self):
        with tempfile.TemporaryDirectory() as directory:
            session_path = os.path.join(directory, "session.json")
            with open(session_path, "w", encoding="utf-8") as handle:
                json.dump(
                    {
                        "gsid": "secret-gsid",
                        "cookie_store": [
                            {
                                "raw_cookie": "SUB=secret-cookie; Path=/; Secure",
                                "path": "/",
                                "domain": {"HostOnly": "weibo.com"},
                            }
                        ],
                    },
                    handle,
                )
            captured = {}

            def open_url(request, timeout):
                captured["url"] = request.full_url
                captured["cookie"] = request.headers.get("Cookie")
                return 200, b'{"data": {"cards": []}}'

            fetcher = WeiboHttpFetcher(session_path=session_path, open_url=open_url)
            _, body = fetcher(KIND_USER_POSTS, {"uid": 1})
            self.assertEqual(captured["cookie"], "SUB=secret-cookie")
            self.assertIn("gsid=secret-gsid", captured["url"])
            self.assertNotIn("secret", json.dumps(body))


if __name__ == "__main__":
    unittest.main()
