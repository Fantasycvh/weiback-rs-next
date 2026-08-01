# ADR-005：新版采用永久独立应用身份

- 状态：Accepted（2026-08-01）
- 关联实现：`mainBinaryName`、`RuntimeDirs`、NSIS `currentUser`

## 背景

旧版以 `weiback-rs` / `com.weiback.app` / `weiback` 命名空间运行。若新版沿用旧身份，安装器会把新版识别为旧版升级，导致覆盖、卸载或重置互相影响。

## 决策

新版固定使用永久独立身份，`Next` 不是发布稳定后要撤销的临时后缀：

| 标识 | 新版固定值 |
|---|---|
| 产品名 | `WeiBack Next` |
| Tauri identifier | `com.weiback.next` |
| Rust binary / Windows 可执行文件 | `weiback-next` / `weiback-next.exe` |
| 数据与配置命名空间 | `weiback-next` |
| 窗口标题与快捷方式 | `WeiBack Next` |
| Sidecar 基础名 | `weiback-collector`（Tauri 按 target triple 选择实际文件） |

默认运行目录整体隔离（见 PLAN.md §4.3）：数据在 `data/weiback-next/`（db/media/logs/sidecar/chromium/imports），配置在 `config/weiback-next/`（config.toml/session.json）。

## 后果

- 正面：新旧安装、运行、更新、重置和卸载互不影响；避免未来二次身份与数据迁移。
- 负面：新版需独立维护发布渠道、快捷方式、配置和用户登录态。
- 卸载或重置任一应用不得删除另一应用的文件。

## 约束

- 新版不得搜索、读取或回写旧版 `weiback/config.toml`。
- 数据库在独立目录内仍命名为 `weiback.db`；文件名不承担 schema 版本职责。
- Windows 保持 per-user 安装（NSIS `installMode: currentUser`）。
