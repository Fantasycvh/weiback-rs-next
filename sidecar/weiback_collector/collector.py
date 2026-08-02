"""事件驱动采集编排。

P1-A 交付物：
- collector 与 Python writer 解耦：本模块只调用 `fetch_page` 获取上游原始
  响应，经 `extract` 抽取出规范化事件并输出到 Emitter，不写任何数据库
  （写库职责在 Rust 侧，见 ADR-002/003）。
- 分页后 checkpoint：每页结束输出 checkpoint 事件，cursor 单调推进。
- 异常分类/指数退避/认证状态：复用 `upstream`。限流自动退避重试；
  认证失效停止任务并标记 done stopped；非可重试错误输出 error 后停止。

`fetch_page(kind, params) -> (http_status, body)` 是唯一网络边界。P1-B 可
将其替换为真实微博 HTTP 调用，本模块无需改动。fixture 模式由
[`FixtureFetchPage`] 提供。
"""

from __future__ import annotations

import json
import os
from dataclasses import dataclass
from typing import Any, Callable, Protocol

from . import extract
from . import upstream
from .events import EventEmitter
from .upstream import AuthState, BackoffPolicy, ErrorClassification, UpstreamError

# fetch_page(kind, params) -> (http_status, body)
# kind ∈ {user_posts, comments, replies}
FetchPage = Callable[[str, dict[str, Any]], tuple[int, dict]]

KIND_USER_POSTS = "user_posts"
KIND_COMMENTS = "comments"
KIND_REPLIES = "replies"


class Emitter(Protocol):
    """事件输出接口：`EventEmitter`（写 stdout）与测试 BufferEmitter 均满足。"""

    request_id: str

    def emit(
        self,
        event_type: str,
        payload: dict[str, Any],
        *,
        stream: str | None = None,
        total_expected: int | None = None,
        sequence: bool = True,
    ) -> None: ...

    def progress(self, phase: str, message: str | None = None) -> None: ...


@dataclass
class CollectResult:
    """一次采集请求的汇总。"""

    fetched_count: int = 0
    status: str = "completed"  # completed | stopped
    pages: int = 0
    last_error: ErrorClassification | None = None


class _Stop(Exception):
    """内部信号：异常中止采集（认证失效/重试耗尽/协议错误）。"""

    def __init__(self, status: str, error: ErrorClassification | None = None) -> None:
        super().__init__(error.message if error is not None else status)
        self.status = status
        self.error = error


class Collector:
    """事件驱动的采集编排器。

    对每个请求：
    1. 输出 started 事件。
    2. 分页调用 `fetch_page`，把原始响应抽取出规范化事件输出。
    3. 每页结束输出 checkpoint（cursor + fetched_count + has_more）。
    4. 全部页完成输出 done（completed）；异常停止输出 done（stopped）。
    """

    def __init__(
        self,
        *,
        fetch_page: FetchPage,
        backoff: BackoffPolicy | None = None,
        auth: AuthState | None = None,
        max_pages: int = 10,
    ) -> None:
        self.fetch_page = fetch_page
        self.backoff = backoff or BackoffPolicy()
        self.auth = auth or AuthState.UNKNOWN
        self.max_pages = max_pages

    # -- 公开入口 --------------------------------------------------------

    def collect_posts(
        self,
        emitter: Emitter,
        uid: Any,
        *,
        checkpoint: dict | None = None,
    ) -> CollectResult:
        """采集用户帖子流 `user:{uid}:posts`。"""
        stream = f"user:{uid}:posts"
        params = {"uid": uid}
        return self._run(
            emitter,
            stream,
            KIND_USER_POSTS,
            params,
            self._page_to_posts_events,
            checkpoint,
        )

    def collect_comments(
        self,
        emitter: Emitter,
        post_id: Any,
        *,
        checkpoint: dict | None = None,
    ) -> CollectResult:
        """采集帖子一级评论流 `post:{post_id}:comments`。"""
        stream = f"post:{post_id}:comments"
        params = {"post_id": post_id}
        return self._run(
            emitter,
            stream,
            KIND_COMMENTS,
            params,
            self._page_to_comments_events,
            checkpoint,
        )

    def collect_replies(
        self,
        emitter: Emitter,
        post_id: Any,
        root_comment_id: Any,
        *,
        checkpoint: dict | None = None,
    ) -> CollectResult:
        """采集二级回复流 `post:{post_id}:comment:{root_id}:replies`。

        兼容 hotFlowChild 两种信封格式（`extract.unpack_child_comment_page`）。
        """
        stream = f"post:{post_id}:comment:{root_comment_id}:replies"
        params = {"post_id": post_id, "root_comment_id": root_comment_id}
        return self._run(
            emitter,
            stream,
            KIND_REPLIES,
            params,
            self._page_to_replies_events,
            checkpoint,
        )

    # -- 分页主循环 ------------------------------------------------------

    def _run(
        self,
        emitter: Emitter,
        stream: str,
        kind: str,
        base_params: dict[str, Any],
        page_handler: Callable[[dict[str, Any], dict[str, Any]], tuple[dict, list, int]],
        checkpoint: dict | None,
    ) -> CollectResult:
        result = CollectResult(fetched_count=_checkpoint_fetched_count(checkpoint))
        emitter.emit("started", {"stream": stream}, stream=stream)

        cursor = _cursor_from(checkpoint)
        page = 0
        try:
            while page < self.max_pages:
                page += 1
                params = dict(base_params)
                if cursor is not None:
                    params["max_id"] = cursor.get("max_id")
                    params["max_id_type"] = cursor.get("max_id_type", 0)

                body = self._fetch_with_retry(emitter, stream, kind, params)

                try:
                    cursor_next, events, has_more = page_handler(body, params)
                except UpstreamError as exc:
                    # 响应结构错误：不可重试，直接终止并输出 error。
                    self._handle_classification(emitter, stream, exc.classification, attempt=0)
                    raise
                for event_type, payload in events:
                    emitter.emit(event_type, payload, stream=stream)
                    if event_type in ("post", "comment"):
                        result.fetched_count += 1

                emitter.emit(
                    "checkpoint",
                    {
                        "cursor": cursor_next,
                        "fetched_count": result.fetched_count,
                        "has_more": has_more,
                    },
                    stream=stream,
                )
                result.pages = page
                cursor = cursor_next
                if not has_more:
                    break
        except _Stop as stop:
            result.status = stop.status
            result.last_error = stop.error

        _finish(emitter, stream, result)
        return result

    # -- 上游请求与错误处理 ---------------------------------------------

    def _fetch_with_retry(self, emitter: Emitter, stream: str, kind: str, params: dict) -> dict:
        """单页上游请求 + 指数退避重试。

        成功返回 body。认证失效/重试耗尽时抛 [`_Stop`] 终止整个采集。
        """
        attempt = 0
        while True:
            attempt += 1
            try:
                status, body = self.fetch_page(kind, params)
            except UpstreamError as exc:
                self._handle_classification(emitter, stream, exc.classification, attempt)
                continue
            if status < 400:
                return body
            classification = upstream.classify_http_status(
                status, body=body if isinstance(body, dict) else None
            )
            self._handle_classification(emitter, stream, classification, attempt)

    def _handle_classification(
        self,
        emitter: Emitter,
        stream: str,
        classification: ErrorClassification,
        attempt: int,
    ) -> None:
        """按分类处理一次失败；需要重试时 sleep 后返回，否则抛 [`_Stop`]。"""
        if classification.code == upstream.ERR_AUTH_REQUIRED:
            self.auth = self.auth.mark(http_status=401)
            emitter.emit(
                "auth_required",
                {
                    "code": upstream.ERR_AUTH_REQUIRED,
                    "auth_state": self.auth.value,
                    "message": classification.message,
                },
                stream=stream,
            )
            raise _Stop("stopped", classification)

        if classification.code == upstream.ERR_UPSTREAM_RATE_LIMITED:
            emitter.emit(
                "rate_limited",
                {
                    "code": upstream.ERR_UPSTREAM_RATE_LIMITED,
                    "retryable": True,
                    "retry_after_ms": classification.retry_after_ms,
                    "scope": "request",
                    "message": classification.message,
                },
                stream=stream,
            )
            if self.backoff.should_retry(attempt):
                delay = self.backoff.delay_for(
                    attempt - 1, retry_after_ms=classification.retry_after_ms
                )
                upstream.sleep_for(delay)
                return
            emitter.emit(
                "error",
                {
                    "code": classification.code,
                    "message": classification.message,
                    "retryable": classification.retryable,
                    "scope": classification.scope,
                },
                stream=stream,
            )
            raise _Stop("stopped", classification)

        if classification.retryable and self.backoff.should_retry(attempt):
            emitter.emit(
                "warning",
                {
                    "code": classification.code,
                    "message": classification.message,
                    "retry_after_ms": classification.retry_after_ms,
                },
                stream=stream,
            )
            delay = self.backoff.delay_for(
                attempt - 1, retry_after_ms=classification.retry_after_ms
            )
            upstream.sleep_for(delay)
            return

        emitter.emit(
            "error",
            {
                "code": classification.code,
                "message": classification.message,
                "retryable": classification.retryable,
                "scope": classification.scope,
            },
            stream=stream,
        )
        raise _Stop("stopped", classification)

    # -- 页面 → 事件 -----------------------------------------------------

    def _page_to_posts_events(
        self, body: dict, params: dict[str, Any]
    ) -> tuple[dict, list, bool]:
        raw_posts = body.get("statuses") if "statuses" in body else body.get("data")
        if not isinstance(raw_posts, list):
            raise UpstreamError(upstream.classify_schema_error("posts response missing list"))

        events: list[tuple[str, dict]] = []
        emitted_users: set[str] = set()
        uid = params.get("uid")
        for raw_post in raw_posts:
            if not isinstance(raw_post, dict):
                continue
            _append_user_event(events, emitted_users, raw_post.get("user"))
            post_dto = extract.extract_post(raw_post, uid)
            events.append(("post", post_dto))
            for media_ref in extract.media_references_from_post(post_dto, raw_post):
                events.append(("media_reference", media_ref))

        next_cursor = _next_posts_cursor(body)
        has_more = bool(raw_posts) and _has_more(next_cursor)
        return next_cursor, events, has_more

    def _page_to_comments_events(
        self, body: dict, params: dict[str, Any]
    ) -> tuple[dict, list, bool]:
        raw_comments = body.get("data")
        if not isinstance(raw_comments, list):
            raise UpstreamError(upstream.classify_schema_error("comments response missing list"))

        events: list[tuple[str, dict]] = []
        emitted_users: set[str] = set()
        post_id = params.get("post_id")
        for raw_comment in raw_comments:
            if not isinstance(raw_comment, dict):
                continue
            _append_user_event(events, emitted_users, raw_comment.get("user"))
            comment_dto = extract.extract_comment(raw_comment, post_id, depth=0)
            events.append(("comment", comment_dto))
            media_ref = extract.media_reference_from_comment(comment_dto)
            if media_ref is not None:
                events.append(("media_reference", media_ref))

        next_cursor = {
            "max_id": _as_id_str(body.get("max_id", 0)),
            "max_id_type": _as_int(body.get("max_id_type", 0)),
        }
        has_more = bool(raw_comments) and _has_more(next_cursor)
        return next_cursor, events, has_more

    def _page_to_replies_events(
        self, body: dict, params: dict[str, Any]
    ) -> tuple[dict, list, bool]:
        raw_data = body.get("data")
        known_envelope = isinstance(raw_data, list) or (
            isinstance(raw_data, dict)
            and (
                isinstance(raw_data.get("comments"), list)
                or isinstance(raw_data.get("data"), list)
            )
        )
        if not known_envelope:
            raise UpstreamError(
                upstream.classify_schema_error("replies response missing list")
            )
        raw_comments, max_id, max_id_type = extract.unpack_child_comment_page(body)

        events: list[tuple[str, dict]] = []
        emitted_users: set[str] = set()
        post_id = params.get("post_id")
        for raw_comment in raw_comments:
            if not isinstance(raw_comment, dict):
                continue
            _append_user_event(events, emitted_users, raw_comment.get("user"))
            comment_dto = extract.extract_comment(
                raw_comment,
                post_id,
                root_id=params.get("root_comment_id"),
                parent_id=params.get("root_comment_id"),
                depth=1,
            )
            events.append(("comment", comment_dto))
            media_ref = extract.media_reference_from_comment(comment_dto)
            if media_ref is not None:
                events.append(("media_reference", media_ref))

        next_cursor = {"max_id": max_id, "max_id_type": max_id_type}
        has_more = _has_more(next_cursor)
        return next_cursor, events, has_more


def _append_user_event(
    events: list[tuple[str, dict]], emitted_users: set[str], raw_user: Any
) -> None:
    if not isinstance(raw_user, dict):
        return
    user = extract.extract_user(raw_user)
    user_id = user.get("id")
    if user_id is None or str(user_id) in emitted_users:
        return
    emitted_users.add(str(user_id))
    events.append(("user", user))
    avatar_ref = extract.media_reference_from_user(user)
    if avatar_ref is not None:
        events.append(("media_reference", avatar_ref))


def _finish(emitter: Emitter, stream: str, result: CollectResult) -> None:
    emitter.emit(
        "done",
        {"status": result.status, "fetched_count": result.fetched_count, "has_more": False},
        stream=stream,
    )


# -- 游标工具 ------------------------------------------------------------

def _cursor_from(checkpoint: dict | None) -> dict | None:
    if not isinstance(checkpoint, dict):
        return None
    if checkpoint.get("max_id") is not None:
        return checkpoint
    cursor = checkpoint.get("cursor")
    return cursor if isinstance(cursor, dict) and cursor.get("max_id") is not None else None


def _checkpoint_fetched_count(checkpoint: dict | None) -> int:
    if not isinstance(checkpoint, dict):
        return 0
    return max(_as_int(checkpoint.get("fetched_count"), 0), 0)


def _next_posts_cursor(body: dict) -> dict:
    # posts 分页用 page 序号作为游标；微博在超页时返回空 list 终止。
    page = _as_int(body.get("page"), 1)
    return {"max_id": _as_id_str(page + 1), "max_id_type": 0}


def _has_more(cursor: dict) -> bool:
    return cursor.get("max_id") not in ("0", "", None)


def _as_id_str(value) -> str:
    if value is None:
        return "0"
    return str(value)


def _as_int(value, default: int = 0) -> int:
    try:
        return int(value)
    except (TypeError, ValueError):
        return default


class FixtureFetchPage:
    """fixture 模式的上游：从 `fixtures/raw/` 读取响应文件。

    用于 P1-A 演示与测试，不访问网络。文件映射：
    - replies  → `hotflowchild_envelope_a.json`（格式 A）或 `_b.json`（格式 B）。
    - 其它 kind 无默认文件，返回 (404, {})。
    """

    def __init__(self, fixture_dir: str, *, envelope: str = "a") -> None:
        self.fixture_dir = fixture_dir
        self.envelope = envelope

    def __call__(self, kind: str, params: dict[str, Any]) -> tuple[int, dict]:
        if kind != KIND_REPLIES:
            return 404, {}
        name = f"hotflowchild_envelope_{self.envelope}.json"
        path = os.path.join(self.fixture_dir, "raw", name)
        if not os.path.isfile(path):
            return 404, {}
        with open(path, encoding="utf-8") as handle:
            body = json.load(handle)
        return 200, body if isinstance(body, dict) else {}
