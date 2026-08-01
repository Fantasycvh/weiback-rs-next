# 黄金 fixture：checkpoint

覆盖 `checkpoint` 事件与 `done`/`cancelled` 收尾的游标语义。核心规则（见 `docs/protocol/v1/README.md`）：

- `checkpoint.payload.has_more == true` 表示上游还有更多分页，Rust 应记录 cursor 以便续传。
- 仅当 `done.payload.has_more == false` 才视为该流完成；`has_more == true` 且流被中断时，恢复从最后一次 `cursor` 继续。
- `fetched_count` 在请求内单调不减，是**诊断**而非完成判据。
- `cursor.max_id` / `max_id_type` 由 Rust 原样持久化并回传，语义由 Sidecar 定义。

## 文件索引

| 文件 | 覆盖点 |
|---|---|
| `pagination_cursor_progress.jsonl` | 多页推进：cursor 单调前进、`has_more` true→false、最终 `done completed` |
| `partial_stopped.jsonl` | 中断恢复：`has_more == true` 时流中断（进程退出/被取消），展示续传入口 cursor |
