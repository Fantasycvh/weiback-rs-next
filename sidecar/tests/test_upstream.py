"""上游策略测试：异常分类、认证状态、指数退避与抖动。"""

import unittest

from weiback_collector import upstream
from weiback_collector.upstream import (
    AuthState,
    BackoffPolicy,
    UpstreamError,
    classify_http_status,
    classify_network_error,
    classify_schema_error,
)


class ClassifyHttpStatusTest(unittest.TestCase):
    def test_401_auth_required(self):
        classification = classify_http_status(401)
        self.assertEqual(classification.code, upstream.ERR_AUTH_REQUIRED)
        self.assertFalse(classification.retryable)

    def test_403_auth_required(self):
        classification = classify_http_status(403)
        self.assertEqual(classification.code, upstream.ERR_AUTH_REQUIRED)
        self.assertFalse(classification.retryable)

    def test_429_rate_limited(self):
        classification = classify_http_status(429, retry_after_ms=5000)
        self.assertEqual(classification.code, upstream.ERR_UPSTREAM_RATE_LIMITED)
        self.assertTrue(classification.retryable)
        self.assertEqual(classification.retry_after_ms, 5000)

    def test_503_upstream_error_retryable(self):
        classification = classify_http_status(503)
        self.assertEqual(classification.code, upstream.ERR_UPSTREAM_ERROR)
        self.assertTrue(classification.retryable)

    def test_404_not_retryable(self):
        classification = classify_http_status(404)
        self.assertEqual(classification.code, upstream.ERR_UPSTREAM_ERROR)
        self.assertFalse(classification.retryable)

    def test_body_message_used(self):
        classification = classify_http_status(429, body={"msg": "频次限制"})
        self.assertIn("频次限制", classification.message)


class AuthStateTest(unittest.TestCase):
    def test_default_unknown(self):
        self.assertEqual(AuthState.UNKNOWN.value, "unknown")

    def test_403_downgrades_to_guest(self):
        state = AuthState.AUTHENTICATED.mark(http_status=403)
        self.assertEqual(state, AuthState.GUEST)

    def test_authenticated_stays(self):
        state = AuthState.AUTHENTICATED.mark(authenticated=True)
        self.assertEqual(state, AuthState.AUTHENTICATED)

    def test_unknown_with_success_stays_unknown(self):
        state = AuthState.UNKNOWN.mark(authenticated=None, http_status=200)
        self.assertEqual(state, AuthState.UNKNOWN)

    def test_authenticated_false_downgrades(self):
        state = AuthState.UNKNOWN.mark(authenticated=False)
        self.assertEqual(state, AuthState.GUEST)


class BackoffPolicyTest(unittest.TestCase):
    def setUp(self):
        self.policy = BackoffPolicy(
            base_seconds=1.0,
            factor=2.0,
            max_seconds=60.0,
            jitter_ratio=0.0,
            max_attempts=5,
        )

    def test_exponential_growth(self):
        self.assertAlmostEqual(self.policy.delay_for(0), 1.0)
        self.assertAlmostEqual(self.policy.delay_for(1), 2.0)
        self.assertAlmostEqual(self.policy.delay_for(2), 4.0)
        self.assertAlmostEqual(self.policy.delay_for(3), 8.0)

    def test_max_seconds_caps_growth(self):
        self.assertAlmostEqual(self.policy.delay_for(10), 60.0)

    def test_respects_retry_after(self):
        delay = self.policy.delay_for(0, retry_after_ms=5000)
        self.assertGreaterEqual(delay, 5.0)

    def test_negative_jitter_never_undercuts_retry_after(self):
        policy = BackoffPolicy(
            base_seconds=1.0,
            jitter_ratio=0.2,
            rng=__import__("random").Random(1),
        )
        self.assertGreaterEqual(policy.delay_for(0, retry_after_ms=5000), 5.0)

    def test_jitter_bounds(self):
        policy = BackoffPolicy(
            base_seconds=10.0,
            factor=1.0,
            max_seconds=60.0,
            jitter_ratio=0.2,
            max_attempts=3,
            rng=__import__("random").Random(42),
        )
        for attempt in range(5):
            delay = policy.delay_for(attempt)
            self.assertGreaterEqual(delay, 8.0)
            self.assertLessEqual(delay, 12.0)

    def test_should_retry_limits(self):
        self.assertTrue(self.policy.should_retry(1))
        self.assertTrue(self.policy.should_retry(4))
        self.assertFalse(self.policy.should_retry(5))

    def test_delay_never_negative(self):
        delay = self.policy.delay_for(0, retry_after_ms=0)
        self.assertGreaterEqual(delay, 0.0)


class ErrorClassificationHelpersTest(unittest.TestCase):
    def test_network_error(self):
        classification = classify_network_error("connection reset")
        self.assertEqual(classification.code, upstream.ERR_NETWORK_ERROR)
        self.assertTrue(classification.retryable)

    def test_schema_error(self):
        classification = classify_schema_error("missing field")
        self.assertEqual(classification.code, upstream.ERR_RESPONSE_SCHEMA_CHANGED)
        self.assertFalse(classification.retryable)

    def test_upstream_error_carry(self):
        classification = classify_http_status(503)
        error = UpstreamError(classification)
        self.assertEqual(error.classification.code, upstream.ERR_UPSTREAM_ERROR)
        self.assertIn("503", str(error))


if __name__ == "__main__":
    unittest.main()
