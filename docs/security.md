# 敏感字段与日志脱敏规则

本文档定义 WeiBack Next（Rust 主进程 + Python Sidecar）对敏感数据的分类、日志输出边界与脱敏要求，落实 PLAN.md §13（可观测性与安全）。

## 1. 敏感数据分类

| 类别 | 示例 | 级别 |
|---|---|---|
| 认证秘密 | Sidecar Cookie、微博令牌、登录密钥 | 最高：禁止出现在 stdout、普通日志、协议 payload |
| 会话文件 | `config/weiback-next/session.json` 内容 | 最高：不迁移、不导入、不进日志 |
| 个人数据响应 | 上游原始完整响应、私信、手机号 | 高：默认不入普通日志；需要诊断时仅保存脱敏摘要 |
| 半公开数据 | 已发布微博正文、公开评论 | 中：可记录 ID/摘要，不整段复制正文进日志 |
| 文件路径 | 用户数据目录、下载路径 | 低：可记录，但相对应用根或用户目录缩写 |

## 2. 输出通道边界

- **stdout（Sidecar）**：仅允许 JSONL 协议消息（`docs/protocol/v1/`）。任何日志、Cookie、原始响应都不得写入 stdout。
- **stderr（Sidecar）**：结构化诊断日志。禁止认证秘密；个人数据仅允许脱敏摘要。
- **Rust 日志**（`data/weiback-next/logs/weiback-next.log`）：遵守 §3 脱敏规则；记录进程启动、握手、事务提交、checkpoint、重试和退出码，不记录认证秘密。

## 3. 脱敏规则

在进入任何日志通道（Rust 或 Sidecar stderr）之前执行：

1. **Cookie / 令牌**：整体脱敏为 `***`。禁止记录 `Cookie:` 头、`Set-Cookie` 值、token 字段。
2. **手机号**：保留前 3 后 4，中间脱敏（`138****5678`）。
3. **认证响应**：`raw_data` 中任何含 session/token/cookie 的键删除或替换为 `"<redacted>"`。
4. **完整个人数据响应**：不整段记录；若必须记录诊断信息，仅保留：
   - 状态码与响应分类（正常/429/认证失败/schema 变化）；
   - 关键字段存在性（如 `has_text`、`pic_count`）；
   - 截断摘要（正文前 80 字符）仅在明确标注 `redacted=true` 时使用。
5. **URL 查询参数**：图片 URL 的 `ssig`、`Expires` 等签名参数不视为认证秘密，但不得整条复制进错误日志；仅记录无查询串的路径。
6. **路径**：统一缩写为相对 `weiback-next` 命名空间根的相对路径（如 `media/pictures/xxx.jpg`），避免暴露完整用户名目录。

## 4. session 文件

- 位于 `config/weiback-next/session.json`（应用专属配置目录），不进入数据目录，不随数据导入导出。
- 旧版 session 默认不迁移、不导入（ADR-006）；新版独立登录。
- 文件权限按操作系统默认用户私有设置，不额外开放共享读。
- 日志中只允许出现 `session.json` 路径，绝不出现内容。

## 5. 不可信输入

- 导入库、JSONL payload、文件路径与远程 URL 一律按不可信输入处理：
  - 解析失败不得导致进程崩溃，必须以 `INVALID_COMMAND`/`RESPONSE_SCHEMA_CHANGED` 等稳定错误码上报。
  - 路径穿越：Sidecar 返回的媒体路径只允许落在应用管理的目录内。
- 脱敏函数必须独立于业务日志调用点，避免误放行。

## 6. 发布校验

- 打包时校验 Sidecar 版本与协议版本一致（`ready.sidecar_version` + `protocol_version`）。
- 正式发布加入 Sidecar 文件哈希或签名检查；开发/调试构建例外须显式标注。
- 任何 debug 日志在合并前必须经本规则审计；发现认证秘密即阻塞发布。

## 7. 检查清单（提交前）

- [ ] grep 未发现 Cookie/令牌值出现在日志模板或 stdout 分支
- [ ] `raw_data` 无 session/token/cookie 字段原样落库或落日志
- [ ] stderr/stdout 分离：协议消息只在 stdout，诊断只在 stderr
- [ ] session 文件未出现在备份/导入/导出的文件清单
