# ADR-001：采用受控混合架构

- 状态：Accepted（2026-08-01）
- 决策者：项目维护者

## 背景

现有 Rust/Tauri 桌面应用已具备稳定的 SQLite 存储、FTS 导出、媒体下载和 Tauri command API。Python 采集器（`crawl4weibo`）已覆盖评论、长文、话题、@、位置和浏览器兜底解析，且经过真实网络验证。立即用纯 Rust 重写全部采集端点会重复投入并推迟交付。

## 决策

保留 Rust/Tauri 作为主产品，引入 Python Sidecar 复用难以替代的网络采集能力：

- Rust/Tauri：进程生命周期、协议校验、SQLite 唯一 writer、迁移、任务、checkpoint、调度、媒体下载。
- Python Sidecar：登录态使用、网络请求、页面/API 解析、规范化事件输出。
- 职责边界见 PLAN.md §4.1。

## 后果

- 正面：快速补齐评论/长文等能力，复用已验证解析逻辑。
- 负面：必须维护跨语言协议、Sidecar 打包和进程诊断。
- 后续：保留采集后端接口，未来可按端点渐进替换为 Rust 实现（见 PLAN.md §16）。

## 约束

- 首期 Windows x64 首发。
- 协议、JSON Schema 和黄金 fixture 与平台无关，仅打包层限定 Windows。
