# 黄金 fixture：帖子流

本目录的 fixture 覆盖协议 v1 中 `post` 事件的典型形态与边界（见 `docs/protocol/v1/event.schema.json` 与 `docs/protocol/v1/dtos.schema.json`）。

每个文件是一个合法的 JSONL 流（stdout 一帧一行）。行内字段均满足事件信封与 `post_dto` 约束：
- 信封必填：`protocol_version`、`request_id`、`event_id`（UUID v7）、`type`、`occurred_at`。
- 帖子必填：`id`、`uid`；其余字段按场景提供。
- `stream` 使用 `user:<uid>:posts` 形式。

## 文件索引

| 文件 | 覆盖点 | 用途 |
|---|---|---|
| `long_text_full.jsonl` | 长文完整字段（`is_long_text=true`、全部新增字段、`content_status=complete`） | 正向解析、字段完整性校验 |
| `retweet_chain.jsonl` | 转发链（`retweeted_id` 指向另一帖、`repost_type=1`） | 转发关联解析 |
| `deleted_post.jsonl` | 删除态帖子（`deleted=true`） | 删除语义 |
| `minimal_missing_fields.jsonl` | 最少字段集（仅 `id`/`uid`）与可选字段缺失 | 容错、字段降级 |

## 校验方式

所有 fixture 均可作为 JSON Lines 逐行解析，并可分别用 `command.schema.json`/`event.schema.json` + `dtos.schema.json` 校验（`$ref` 相对引用基于本仓库 `docs/protocol/v1/`）。

> 说明：fixture 中 `occurred_at` 使用固定 UTC 时间以保持可重复；真实 Sidecar 会输出当前时间，仅用于诊断，不作为语义判据。
