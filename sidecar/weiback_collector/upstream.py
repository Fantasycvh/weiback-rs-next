"""上游交互策略：异常分类、指数退避、随机抖动与认证状态。

P1-A 交付物：
- 异常分类：把上游 HTTP 错误映射到稳定协议错误码（对齐
  `docs/protocol/v1/errors.md` 的 11 个稳定错误码）。
- 指数退避 + 随机抖动：请求级重试策略，尊重上游 `Retry-After`。
- 认证状态：unknown/authenticated/guest 的显式状态机。

零第三方依赖。
"""

from __future__ import annotations

import random
import time
from dataclasses import dataclass, field
from enum import Enum

# 稳定错误码（docs/protocol/v1/errors.md）
ERR_UPSTREAM_RATE_LIMITED = "UPSTREAM_RATE_LIMITED"
ERR_AUTH_REQUIRED = "AUTH_REQUIRED"
ERR_BROWSER_NOT_INSTALLED = "BROWSER_NOT_INSTALLED"
ERR_NETWORK_ERROR = "NETWORK_ERROR"
ERR_UPSTREAM_ERROR = "UPSTREAM_ERROR"
ERR_RESPONSE_SCHEMA_CHANGED = "RESPONSE_SCHEMA_CHANGED"
ERR_INVALID_COMMAND = "INVALID_COMMAND"
ERR_REQUEST_CANCELLED = "REQUEST_CANCELLED"
ERR_INTERNAL_ERROR = "INTERNAL_ERROR"

# 认证相关 HTTP 状态码
_AUTH_STATUSES = {401, 403}
# 可重试的状态码（不含认证，认证走独立路径）
_RETRYABLE_STATUSES = {
    408,  # Request Timeout
    429,  # Too Many Requests
    500,  # Internal Server Error
    502,  # Bad Gateway
    503,  # Service Unavailable
    504,  # Gateway Timeout
}


class AuthState(str, Enum):
    """认证状态机。"""

    UNKNOWN = "unknown"
    AUTHENTICATED = "authenticated"
    GUEST = "guest"

    def mark(self, *, authenticated: bool | None = None, http_status: int | None = None) -> "AuthState":
        """按观测更新状态：403/401 → guest；显式 authenticated → authenticated。"""
        if http_status is not None and http_status in _AUTH_STATUSES:
            return AuthState.GUEST
        if authenticated is True:
            return AuthState.AUTHENTICATED
        if authenticated is False:
            return AuthState.GUEST
        return self


@dataclass(frozen=True)
class ErrorClassification:
    """上游错误的稳定协议分类。"""

    code: str
    message: str
    retryable: bool = False
    retry_after_ms: int | None = None
    scope: str = "request"


class UpstreamError(Exception):
    """携带稳定分类的上游错误。"""

    def __init__(self, classification: ErrorClassification) -> None:
        self.classification = classification
        super().__init__(classification.message)


def classify_http_status(
    status: int,
    *,
    body: dict | None = None,
    retry_after_ms: int | None = None,
) -> ErrorClassification:
    """按 HTTP 状态码分类上游错误。

    - 401/403 → AUTH_REQUIRED（retryable=False，需重新登录）。
    - 429 → UPSTREAM_RATE_LIMITED（retryable=True，尊重 Retry-After）。
    - 408/5xx → UPSTREAM_ERROR（retryable=True）。
    - 其它 → UPSTREAM_ERROR（retryable=False）。
    """
    body = body or {}
    if status in _AUTH_STATUSES:
        return ErrorClassification(
            code=ERR_AUTH_REQUIRED,
            message=body.get("msg") or body.get("error") or f"auth required (http {status})",
            retryable=False,
            scope="request",
        )
    if status == 429:
        return ErrorClassification(
            code=ERR_UPSTREAM_RATE_LIMITED,
            message=body.get("msg") or f"upstream rate limited (http {status})",
            retryable=True,
            retry_after_ms=retry_after_ms,
            scope="request",
        )
    if status in _RETRYABLE_STATUSES:
        return ErrorClassification(
            code=ERR_UPSTREAM_ERROR,
            message=f"upstream error (http {status})",
            retryable=True,
            retry_after_ms=retry_after_ms,
            scope="request",
        )
    return ErrorClassification(
        code=ERR_UPSTREAM_ERROR,
        message=body.get("msg") or f"upstream error (http {status})",
        retryable=False,
        scope="request",
    )


def classify_network_error(message: str) -> ErrorClassification:
    """网络层异常（DNS/连接/超时）→ NETWORK_ERROR（retryable=True）。"""
    return ErrorClassification(
        code=ERR_NETWORK_ERROR,
        message=message or "network error",
        retryable=True,
        scope="request",
    )


def classify_schema_error(message: str) -> ErrorClassification:
    """上游响应结构变化 → RESPONSE_SCHEMA_CHANGED（retryable=False）。"""
    return ErrorClassification(
        code=ERR_RESPONSE_SCHEMA_CHANGED,
        message=message or "upstream response schema changed",
        retryable=False,
        scope="request",
    )


@dataclass
class BackoffPolicy:
    """指数退避 + 随机抖动。

    - `base_seconds`：首次重试基准延迟。
    - `factor`：每次重试延迟乘数（指数增长）。
    - `max_seconds`：单次延迟上限。
    - `jitter_ratio`：随机抖动比例（0~1），真实延迟在 [1-ratio, 1+ratio] 之间。
    - `max_attempts`：最大尝试次数（含首次）。
    """

    base_seconds: float = 1.0
    factor: float = 2.0
    max_seconds: float = 60.0
    jitter_ratio: float = 0.2
    max_attempts: int = 5
    rng: random.Random = field(default_factory=random.Random)

    def delay_for(self, attempt: int, *, retry_after_ms: int | None = None) -> float:
        """第 `attempt` 次（从 0 开始）重试应等待的秒数。

        尊重上游 `Retry-After`（作为下限）；仍叠加指数增长与抖动。
        """
        base = self.base_seconds * (self.factor**attempt)
        base = min(base, self.max_seconds)

        jitter = base * self.jitter_ratio
        delay = max(0.0, base + self.rng.uniform(-jitter, jitter))
        if retry_after_ms and retry_after_ms > 0:
            delay = max(delay, retry_after_ms / 1000.0)
        return delay

    def should_retry(self, attempt: int) -> bool:
        """`attempt` 为已尝试次数（首次=1），返回是否还有重试机会。"""
        return attempt < self.max_attempts


def sleep_for(delay_seconds: float) -> None:
    """等待；可被子类/测试替换为即时返回。"""
    time.sleep(max(0.0, delay_seconds))
