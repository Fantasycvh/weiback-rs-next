# 协议 v1 — 稳定错误码

首期错误码清单（PLAN.md §5.7）。`scope` 取值：`request`（单个请求失败）、`stream`（整个流失败）、`sidecar`（进程级失败）。

| code | retryable | scope | 含义 |
|---|---|---|---|
| `PROTOCOL_VERSION_UNSUPPORTED` | false | sidecar | 协议版本不兼容 |
| `INVALID_COMMAND` | false | request | 命令信封或 payload 非法 |
| `INVALID_CHECKPOINT` | false | request | 提供的 checkpoint 非法或与请求冲突 |
| `AUTH_REQUIRED` | true | request | 登录态失效或缺失 |
| `UPSTREAM_RATE_LIMITED` | true | account/endpoint/request | 限流；尊重 `retry_after_ms` |
| `UPSTREAM_UNAVAILABLE` | true | request | 上游临时不可用 |
| `RESPONSE_SCHEMA_CHANGED` | false | request | 上游响应结构变化，保留原始诊断摘要 |
| `BROWSER_NOT_INSTALLED` | true | sidecar | Playwright Chromium 未安装 |
| `BROWSER_START_FAILED` | true | sidecar | 浏览器启动失败 |
| `REQUEST_CANCELLED` | false | request | 请求已取消 |
| `INTERNAL_ERROR` | false | request | 未分类内部错误 |

## 错误事件示例

```json
{
  "protocol_version": 1,
  "request_id": "0198c6f4-9d5e-7a00-8f2a-000000000001",
  "event_id": "0198c6f4-9d5e-7a00-8f2a-000000000002",
  "type": "error",
  "stream": "post:123456:comments",
  "sequence": 12,
  "occurred_at": "2026-07-31T12:34:56.789Z",
  "payload": {
    "code": "UPSTREAM_RATE_LIMITED",
    "message": "Request rate limited",
    "retryable": true,
    "retry_after_ms": 60000,
    "scope": "request"
  }
}
```
