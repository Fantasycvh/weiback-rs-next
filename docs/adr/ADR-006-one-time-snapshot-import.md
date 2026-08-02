# ADR-006：旧版数据只做一次性快照导入

- 状态：Accepted（2026-08-01）
- 关联实现：`weiback::legacy::detect_legacy_sources`

## 背景

旧 Rust WeiBack 数据库和 Python v2 数据库的 schema 与新版不同，且旧版可能在运行中（WAL 模式）。原地升级会造成新版数据与旧版写入冲突；共享媒体所有权会导致删除语义混乱。

## 决策

从旧 Rust 库或 Python v2 库创建一致性只读视图，一次性导入，导入后新旧应用不再自动同步：

- 不原地升级旧库；只读识别源类型、schema 版本和媒体根目录。
- 导入期间以同一只读 SQLite 连接上的显式 `BEGIN` 覆盖逻辑 snapshot fingerprint 与全部源查询；指纹按来源类型、相关表 schema 和 canonical 有序行值计算。在 WAL 模式下这提供该连接的稳定读取快照，指纹与实际导入数据必定相同。它不是跨连接的原子快照，也不假称已创建 SQLite backup API 的源副本；禁止 attach、写入源库或直接复制活跃数据库及其未合并 WAL。
- 在 `weiback-next/imports/` 创建临时目标库并执行最新 migrations。
- 媒体先复制到 `imports/import-media-<uuid>/files/`，以 URL 与媒体类型稳定 hash 预计算最终名；发布使用 no-replace 操作，绝不覆盖或删除既有 `media_root` 文件。同 URL 在 picture/video 间冲突时资产层固定选择 video，引用仍按所有者与 definition 保留。
- `manifest.json` 记录本批 staging、媒体类型与最终路径，但它是不可信输入：恢复前必须校验 UUID 批目录、所有相对组件、staging/files 与 media_root 的 canonical containment。目标 DB 提交前媒体行是 `pending/local_path=NULL/import_hold=1`，worker 不可领取；提交后逐项发布并按 batch、URL、类型 CAS 更新为 `downloaded`。发布中断时，已发布项与 DB 一致，未发布项保持 hold，结果为 `partial_recoverable`，下次导入只恢复 `legacy_imports` 中登记且校验通过的批次；缺失本地文件及 Python 远程资产在整个批次发布结束后才转 `pending`。
- 导入以 canonical source path、同一源读取事务中的逻辑 SHA-256 snapshot fingerprint、类型和持久状态记录在 `legacy_imports`；同一 completed fingerprint 返回 `already_completed`，不创建 rollback backup、不再插入引用。文件内容不是 fingerprint 输入，因此 WAL 写入不会让指纹与已读取的逻辑数据脱节。提交前失败会回滚 DB 并删除本批 staging；提交后发布失败保留批次以便恢复。旧库始终不变。数据库与文件系统不是单一原子提交单元。

## 后果

- 正面：渐进验证与快速回退；避免跨 schema 双向同步、冲突合并和共享媒体所有权。
- 负面：导入后两边数据会分叉；需要向导明确语义，并要求用户选择主要使用的版本。
- 默认不导入旧 session、Cookie 或认证秘密；不迁移瞬时 `running` 任务状态。

## 约束

- 检测过程不得自动打开写连接或修改旧库（`legacy` 模块仅解析文件头）。
- 首次启动检测到旧版数据时提供：导入旧版数据 / 全新开始 / 稍后处理。
- 首期 UI 不提供“再次合并旧版新增数据”入口。

## P4 接口与回滚

- Rust 检查接口：`weiback::legacy::inspect_legacy_source(source_path, current_db)`。
  它仅打开现存 SQLite 文件的只读连接读取表签名；`user_version` 仅作诊断，不能单独决定来源类型。
- Rust 导入接口：`weiback::legacy::import_legacy_source(pool, LegacyImportRequest)`。
  写入前会在 `RuntimeDirs::imports_dir` 用 `VACUUM INTO` 生成
  `rollback-backup-<uuid>.db`。导入在单个 `BEGIN IMMEDIATE` 事务内完成；源库不 attach、
  不改写，媒体先进入持久化批次 staging。数据库提交前不发布媒体；提交后才逐项 no-replace 发布并将对应行从 `pending` CAS 为 `downloaded`。
- Tauri 入口：`inspect_legacy_source({ sourcePath })` 和
  `import_legacy_source({ sourcePath })`。仅允许存在的绝对 `weiback.db` 路径；后端先
  canonicalize，再检查该文件仍属于其父目录。失败仅返回稳定脱敏消息，详细原因写日志。
- 回滚操作：停止应用后，将 `imports/rollback-backup-<uuid>.db` 作为恢复源替换
  `weiback-next/weiback.db`，再启动应用。不得在仍有 SQLite 连接或 `-wal`/`-shm` 文件
  时替换；恢复前应将当前数据库另存一份，便于反向回退。该备份只恢复数据库，不会恢复媒体；DB 回滚时必须按该次导入的 manifest 删除仅该批新发布且仍匹配 manifest 内容的文件，绝不触碰既有媒体文件。
