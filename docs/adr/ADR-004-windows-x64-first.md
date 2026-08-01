# ADR-004：首期仅 Windows x64

- 状态：Accepted（2026-08-01）

## 背景

PyInstaller Sidecar 与 Playwright Chromium 均需逐平台构建验证；多平台同时首发会放大发布与验收风险。

## 决策

首期仅发布和验收 Windows x64：

- 打包、签名、Playwright 与崩溃恢复链路先在单一平台闭合。
- 原 README 中已有的其他桌面平台在新采集能力上暂不对等。
- IPC 与数据协议保持平台无关，避免架构固化。

## 后果

- 正面：发布链路和验收范围聚焦，问题可被快速定位。
- 负面：macOS/Linux/Windows ARM64 用户暂时无法使用新采集能力。
- 后续：按平台逐版扩展，协议与 fixture 无需改动。

## 约束

- Sidecar 可执行文件按 Tauri target triple 规则命名（`bundle.externalBin`）。
- 发布前在无 Python、无 Chromium 的干净 Windows x64 环境验收。
