# ADR-003：使用 JSONL stdio IPC

- 状态：Accepted（2026-08-01）

## 背景

候选通信方案包括本机 HTTP 服务、Unix/Windows 命名管道和 stdio。HTTP 需要端口、CORS、服务发现和鉴权；命名管道跨平台实现差异大。

## 决策

Sidecar 与 Rust 之间使用 stdin/stdout JSON Lines 协议，stderr 输出结构化诊断日志：

- 编码 UTF-8，每行一个完整 JSON 对象，以 `\n` 结束。
- stdout 仅协议消息；普通日志只能写 stderr。
- 首次握手必须协商 `protocol_version` 和 capabilities。
- 单条消息设置大小上限；超限时 Sidecar 拆批，Rust 拒绝异常消息。
- Rust 独立消费 stdout 与 stderr，避免缓冲区阻塞子进程。

## 后果

- 正面：无需本地端口、CORS、服务发现和额外鉴权，适合 Tauri 子进程生命周期。
- 负面：必须严格管理帧、消息大小、stdout 纯净度和背压。
- 版本不兼容、无效 JSON 和进程退出均产生可诊断错误且不损坏数据库。

## 协议

协议 v1 定义见 `docs/protocol/v1/`，包含命令信封、事件信封、DTO 与错误码。
