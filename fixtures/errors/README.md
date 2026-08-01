# 黄金 fixture：错误事件

覆盖 `docs/protocol/v1/errors.md` 的 11 个稳定错误码。每条 fixture 是一个合法的 `error` 事件（信封 + `error_payload`）。

## 稳定错误码

| code | retryable | scope | 语义 |
|---|---|---|---|
| `PROTOCOL_VERSION_UNSUPPORTED` | false | sidecar | Sidecar 拒绝不支持的协议版本 |
| `INVALID_COMMAND` | false | request | 命令信封或 payload 非法 |
| `INVALID_CHECKPOINT` | false | request | 回传的 checkpoint 无法解析/已失效 |
| `AUTH_REQUIRED` | true | request | 需要登录态 |
| `UPSTREAM_RATE_LIMITED` | true | endpoint | 上游限流，带 `retry_after_ms` |
| `UPSTREAM_UNAVAILABLE` | true | request | 上游临时不可用 |
| `RESPONSE_SCHEMA_CHANGED` | false | request | 上游返回结构变化 |
| `BROWSER_NOT_INSTALLED` | true | sidecar | Playwright 浏览器缺失 |
| `BROWSER_START_FAILED` | true | sidecar | 浏览器启动失败 |
| `REQUEST_CANCELLED` | false | request | 请求被取消（不视为错误重试） |
| `INTERNAL_ERROR` | false | request | Sidecar 内部错误 |

## 文件索引

| 文件 | 覆盖点 |
|---|---|
| `all_stable_codes.jsonl` | 11 个错误码各一条事件 |
| `rate_limited_flow.jsonl` | 限流→退避→重试成功的完整流 |
