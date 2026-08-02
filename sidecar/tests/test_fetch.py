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
            body = {"data": {"list": [
                {"id": 1},
                {"id": 2},
            ], "total": 100}}
            return 200, json.dumps(body).encode()

        fetcher = WeiboHttpFetcher(open_url=open_url)
        status, body = fetcher(KIND_USER_POSTS, {"uid": "123", "max_id": "2"})
        self.assertEqual(status, 200)
        self.assertEqual(body, {"statuses": [{"id": 1}, {"id": 2}], "page": 2, "total": 100})
        self.assertIn("mymblog", captured["url"])
        self.assertIn("page=2", captured["url"])

    def test_posts_first_page_defaults_to_page_one(self):
        captured = {}

        def open_url(request, timeout):
            captured["url"] = request.full_url
            return 200, b'{"data": {"list": [], "total": 0}}'

        fetcher = WeiboHttpFetcher(open_url=open_url)
        _, body = fetcher(KIND_USER_POSTS, {"uid": "123"})
        self.assertIn("page=1", captured["url"])
        self.assertEqual(body["page"], 1)

    def test_comments_are_normalized(self):
        raw = {"data": [{"id": "c1"}], "max_id": "5"}
        fetcher = WeiboHttpFetcher(
            open_url=lambda request, timeout: (200, json.dumps(raw).encode())
        )
        _, body = fetcher(KIND_COMMENTS, {"post_id": 9})
        self.assertEqual(body["data"], [{"id": "c1"}])
        self.assertEqual(body["max_id"], "5")

    def test_comments_first_page_uses_reload(self):
        captured = {}

        def open_url(request, timeout):
            captured["url"] = request.full_url
            return 200, b'{"data": [], "max_id": "0"}'

        fetcher = WeiboHttpFetcher(open_url=open_url)
        _, _ = fetcher(KIND_COMMENTS, {"post_id": 9})
        self.assertIn("buildComments", captured["url"])
        self.assertIn("is_reload=1", captured["url"])
        self.assertIn("id=9", captured["url"])

    def test_comments_followup_page_uses_max_id(self):
        captured = {}

        def open_url(request, timeout):
            captured["url"] = request.full_url
            return 200, b'{"data": [], "max_id": "0"}'

        fetcher = WeiboHttpFetcher(open_url=open_url)
        _, _ = fetcher(KIND_COMMENTS, {"post_id": 9, "max_id": "208264901815794"})
        self.assertIn("is_reload=0", captured["url"])
        self.assertIn("max_id=208264901815794", captured["url"])

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
            ({"ok": -100, "msg": "需要登录"}, 401),
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
                captured["x_requested_with"] = request.headers.get("X-requested-with")
                return 200, b'{"data": {"list": []}}'

            fetcher = WeiboHttpFetcher(session_path=session_path, open_url=open_url)
            _, body = fetcher(KIND_USER_POSTS, {"uid": 1})
            self.assertEqual(captured["cookie"], "SUB=secret-cookie")
            self.assertEqual(captured["x_requested_with"], "XMLHttpRequest")
            self.assertNotIn("secret", json.dumps(body))

    def test_session_gsid_only_injected_for_mobile_replies(self):
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
                return 200, b'{"data": []}'

            fetcher = WeiboHttpFetcher(session_path=session_path, open_url=open_url)
            fetcher(KIND_REPLIES, {"post_id": 9, "root_comment_id": "c1"})
            self.assertIn("gsid=secret-gsid", captured["url"])

    def test_duplicate_cookie_names_keep_last_value(self):
        with tempfile.TemporaryDirectory() as directory:
            session_path = os.path.join(directory, "session.json")
            with open(session_path, "w", encoding="utf-8") as handle:
                json.dump(
                    {
                        "cookie_store": [
                            {"raw_cookie": "SUB=stale-sub; Path=/; Secure"},
                            {"raw_cookie": "SUBP=old; Path=/; Secure"},
                            {"raw_cookie": "SUB=fresh-sub; Path=/; Secure"},
                        ]
                    },
                    handle,
                )
            captured = {}

            def open_url(request, timeout):
                captured["cookie"] = request.headers.get("Cookie")
                return 200, b'{"data": {"list": []}}'

            fetcher = WeiboHttpFetcher(session_path=session_path, open_url=open_url)
            fetcher(KIND_USER_POSTS, {"uid": 1})
            cookie = captured["cookie"]
            self.assertEqual(cookie.count("SUB="), 1)
            self.assertIn("SUB=fresh-sub", cookie)
            self.assertNotIn("stale-sub", cookie)


if __name__ == "__main__":
    unittest.main()
