"""微博 HTTP 读取边界。

只负责把上游页面读取并规范化为 ``Collector`` 需要的分页信封；不写库、
不下载媒体，也不把 Cookie、gsid 或原始响应写入 stdout/stderr。
"""

from __future__ import annotations

import json
import os
from datetime import datetime, timezone
from email.utils import parsedate_to_datetime
from http.cookies import SimpleCookie
from typing import Any, Callable
from urllib.error import HTTPError, URLError
from urllib.parse import urlencode
from urllib.request import Request, urlopen

from .collector import KIND_COMMENTS, KIND_REPLIES, KIND_USER_POSTS
from .upstream import (
    UpstreamError,
    classify_http_status,
    classify_network_error,
    classify_schema_error,
)

_POSTS_URL = "https://m.weibo.cn/api/container/getIndex"
_COMMENTS_URL = "https://m.weibo.cn/comments/hotflow"
_REPLIES_URL = "https://m.weibo.cn/comments/hotFlowChild"
_USER_AGENT = (
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) "
    "AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126 Safari/537.36"
)

OpenUrl = Callable[[Request, float], tuple[int, bytes]]


class WeiboHttpFetcher:
    """同步 HTTP 分页读取器，可直接作为 ``Collector.fetch_page``。"""

    def __init__(
        self,
        *,
        session_path: str | None = None,
        timeout: float = 30.0,
        open_url: OpenUrl | None = None,
    ) -> None:
        self.timeout = timeout
        self._open_url = open_url or _open_url
        self._cookie, self._gsid = _load_auth(session_path)

    def __call__(self, kind: str, params: dict[str, Any]) -> tuple[int, dict]:
        url, query = self._request_for(kind, params)
        if self._gsid:
            query.setdefault("gsid", self._gsid)
        headers = {
            "Accept": "application/json, text/plain, */*",
            "Referer": "https://m.weibo.cn/",
            "User-Agent": _USER_AGENT,
        }
        if self._cookie:
            headers["Cookie"] = self._cookie
        request = Request(f"{url}?{urlencode(query)}", headers=headers)
        try:
            status, raw = self._open_url(request, self.timeout)
        except HTTPError as exc:
            try:
                body = _decode_json(exc.read(), strict=False)
                retry_after_ms = _retry_after_ms(exc.headers.get("Retry-After"))
            finally:
                exc.close()
            raise UpstreamError(
                classify_http_status(
                    exc.code,
                    body=body,
                    retry_after_ms=retry_after_ms,
                )
            ) from exc
        except (URLError, OSError, TimeoutError) as exc:
            raise UpstreamError(classify_network_error(str(exc))) from exc
        body = _decode_json(raw)
        status = _business_status(status, body)
        return status, self._normalize(kind, body)

    @staticmethod
    def _request_for(kind: str, params: dict[str, Any]) -> tuple[str, dict[str, Any]]:
        if kind == KIND_USER_POSTS:
            uid = params["uid"]
            query: dict[str, Any] = {
                "type": "uid",
                "value": uid,
                "containerid": f"107603{uid}",
            }
            cursor = params.get("max_id")
            if cursor not in (None, "", "0", 0):
                query["since_id"] = cursor
            return _POSTS_URL, query
        if kind == KIND_COMMENTS:
            post_id = params["post_id"]
            return _COMMENTS_URL, {
                "id": post_id,
                "mid": post_id,
                "max_id": params.get("max_id", 0),
                "max_id_type": params.get("max_id_type", 0),
            }
        if kind == KIND_REPLIES:
            return _REPLIES_URL, {
                "cid": params["root_comment_id"],
                "max_id": params.get("max_id", 0),
                "max_id_type": params.get("max_id_type", 0),
            }
        raise ValueError(f"unsupported fetch kind: {kind}")

    @staticmethod
    def _normalize(kind: str, body: dict) -> dict:
        if kind == KIND_USER_POSTS:
            raw_data = body.get("data")
            if not isinstance(raw_data, dict):
                return body
            data: dict = raw_data
            raw_cards = data.get("cards")
            if not isinstance(raw_cards, list):
                return body
            cards: list = raw_cards
            statuses = [
                card["mblog"]
                for card in cards
                if isinstance(card, dict) and isinstance(card.get("mblog"), dict)
            ]
            cardlist_info = data.get("cardlistInfo")
            cardlist_info = cardlist_info if isinstance(cardlist_info, dict) else {}
            return {
                "statuses": statuses,
                "since_id": data.get("since_id", cardlist_info.get("since_id", 0)),
            }
        if kind == KIND_COMMENTS:
            raw_data = body.get("data")
            if not isinstance(raw_data, dict):
                return body
            data = raw_data
            comments = data.get("data") if "data" in data else data.get("comments")
            if not isinstance(comments, list):
                return body
            return {
                "data": comments,
                "max_id": data.get("max_id", 0),
                "max_id_type": data.get("max_id_type", 0),
            }
        return body


def _open_url(request: Request, timeout: float) -> tuple[int, bytes]:
    with urlopen(request, timeout=timeout) as response:  # noqa: S310
        return response.status, response.read()


def _decode_json(raw: bytes, *, strict: bool = True) -> dict:
    try:
        value = json.loads(raw.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        if strict:
            raise UpstreamError(classify_schema_error("upstream returned invalid JSON")) from exc
        return {}
    if isinstance(value, dict):
        return value
    if strict:
        raise UpstreamError(classify_schema_error("upstream JSON is not an object"))
    return {}


def _business_status(status: int, body: dict) -> int:
    """把微博 HTTP 200 的业务失败映射回稳定 HTTP 分类入口。"""
    if status >= 400 or body.get("ok", 1) not in (0, False):
        return status
    message = str(body.get("msg") or body.get("message") or "").lower()
    if any(marker in message for marker in ("登录", "登陆", "身份", "login", "auth")):
        return 401
    if any(marker in message for marker in ("频繁", "频次", "rate", "limit")):
        return 429
    return 502


def _retry_after_ms(value: str | None) -> int | None:
    if not value:
        return None
    try:
        seconds = int(value)
    except ValueError:
        try:
            retry_at = parsedate_to_datetime(value)
        except (TypeError, ValueError, OverflowError):
            return None
        if retry_at.tzinfo is None:
            retry_at = retry_at.replace(tzinfo=timezone.utc)
        seconds = max(0, int((retry_at - datetime.now(timezone.utc)).total_seconds()))
    return max(0, seconds) * 1000


def _load_auth(session_path: str | None) -> tuple[str | None, str | None]:
    cookie = os.environ.get("WEIBACK_COLLECTOR_COOKIE")
    gsid = os.environ.get("WEIBACK_COLLECTOR_GSID")
    if not session_path or not os.path.isfile(session_path):
        return cookie, gsid
    try:
        with open(session_path, encoding="utf-8") as handle:
            session = json.load(handle)
    except (OSError, json.JSONDecodeError):
        return cookie, gsid
    if not isinstance(session, dict):
        return cookie, gsid
    gsid = gsid or _string(session.get("gsid"))
    if not cookie:
        pairs: list[str] = []
        _collect_cookie_pairs(session.get("cookie_store"), pairs)
        cookie = "; ".join(dict.fromkeys(pairs)) or None
    return cookie, gsid


def _collect_cookie_pairs(value: Any, output: list[str]) -> None:
    if isinstance(value, dict):
        name = value.get("name")
        cookie_value = value.get("value")
        if isinstance(name, str) and isinstance(cookie_value, str):
            output.append(f"{name}={cookie_value}")
        raw_cookie = value.get("raw_cookie")
        if isinstance(raw_cookie, str):
            parsed = SimpleCookie()
            parsed.load(raw_cookie)
            output.extend(f"{key}={morsel.value}" for key, morsel in parsed.items())
        for nested in value.values():
            _collect_cookie_pairs(nested, output)
    elif isinstance(value, list):
        for nested in value:
            _collect_cookie_pairs(nested, output)


def _string(value: Any) -> str | None:
    return str(value) if value not in (None, "") else None
