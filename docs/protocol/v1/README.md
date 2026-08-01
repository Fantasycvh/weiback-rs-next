# 协议 v1 — 概览

版本：`1`。变更纪律见 PLAN.md §17：不兼容变更必须提升 `protocol_version`；兼容新增字段不得改变既有字段语义。

## 传输约束

- 编码：UTF-8。
- 帧格式：每行一个完整 JSON 对象，以 `\n` 结束。
- stdout：仅协议消息。stderr：结构化诊断日志，禁止认证秘密。
- 单条消息大小上限：128 KiB（`MAX_MESSAGE_BYTES`）。超限时 Sidecar 拆批，Rust 拒绝异常消息。
- Rust 逐行解析，单条非法 JSON 不得污染数据库。
- 首次握手必须协商 `protocol_version` 和 capabilities。

## 文件索引

- `command.schema.json` — Rust 到 Sidecar 的命令信封 + 全部首期命令 payload。
- `event.schema.json` — Sidecar 到 Rust 的事件信封。
- `dtos.schema.json` — 事件 payload 中的规范化 DTO（user/post/comment/media_reference/checkpoint/rate_limited/auth_required/error/done）。
- `errors.json` — 首期稳定错误码清单。

## 命令信封

```json
{
  "protocol_version": 1,
  "request_id": "0198...uuid-v7",
  "type": "collect_comments",
  "payload": {}
}
```

## 事件信封

```json
{
  "protocol_version": 1,
  "request_id": "0198...uuid-v7",
  "event_id": "0198...uuid-v7",
  "type": "comment",
  "stream": "post:123456:comments",
  "sequence": 42,
  "total_expected": 500,
  "occurred_at": "2026-07-31T12:34:56.789Z",
  "payload": {}
}
```

## 关键语义

- `sequence` 用于检测缺失、重复和乱序；真正的恢复依据是 checkpoint cursor。
- `done.payload.has_more == false`（或明确的范围完成状态）才可结束任务；`sequence == total_expected` 不能作为完成条件。
- 抓取数据与 checkpoint 必须在同一 Rust 数据库事务中提交，提交成功后才发布可信进度。
