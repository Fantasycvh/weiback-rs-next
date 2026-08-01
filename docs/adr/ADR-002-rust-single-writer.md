# ADR-002：Rust 是唯一数据库 writer

- 状态：Accepted（2026-08-01）

## 背景

旧架构中 Python 直接写自己的 SQLite schema（`writer.py`），且会备份 `weiback.db.pre-v2.bak` 后原地迁移。若新版继续让 Python 接触产品库，会引入 SQLite 锁竞争、schema 漂移、迁移归属冲突和任务状态割裂。

## 决策

产品 SQLite 由 Rust 独占读写，Python 绝不接触：

- 所有新表和字段由 Rust sqlx migration 创建。
- 禁止 Sidecar 执行 DDL 或使用 `PRAGMA user_version` 管理产品库。
- 抓取数据和 checkpoint 必须在同一 Rust 数据库事务中提交。
- 所有数据事件必须幂等；重放不得产生重复业务数据。

## 后果

- 正面：单一迁移所有者、任务状态与数据一致、崩溃恢复可靠。
- 负面：Rust 必须实现协议校验、批事务和所有新 Storage API。
- 媒体：Rust 后台下载，Python 只输出 `media_reference`（不含下载结果）。

## 约束

- 旧库升级前必须创建可恢复备份；migration 失败时保留原库。
- Sidecar Cookie、令牌及原始敏感响应不得写入 stdout 或普通日志。
