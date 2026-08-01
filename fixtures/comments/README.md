# 黄金 fixture：评论流

覆盖协议 v1 中 `comment` 事件的典型形态（`comment_dto`，对齐 Python collector 的 `_comment_to_dict`）。

两个集合分别对应两类 stream：
- `post:<post_id>:comments` —— 一级评论及二楼（`depth` 0/1，父子关系经 `root_id`/`parent_id`/`reply_id` 表达）。
- `post:<post_id>:replies` —— 针对某个一楼评论的回复子流（命令 `collect_comment_replies`，携带 `root_comment_id`）。

## 文件索引

| 文件 | 覆盖点 |
|---|---|
| `first_level.jsonl` | 一级评论 + 二楼回复混合流（comments stream） |
| `second_level_replies.jsonl` | 指定一楼评论的回复子流（replies stream） |

## 关键语义

- `depth` 最小 0（schema 强制）。
- 一条评论最多出现在一个流中一次；`sequence` 在同一请求内单调递增。
- `root_id`/`parent_id`/`reply_id` 为 `null` 表示顶级评论。
- 评论仅携带引用 URL 与 `pic_url`，实际下载由 Rust 经 `media_reference` 事件独立请求。
