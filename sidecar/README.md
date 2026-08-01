# weiback-collector — WeiBack Next Sidecar

协议 v1 的 headless JSONL 采集 Sidecar（ADR-002/003/005）。零运行时第三方依赖。

## 目录

```
sidecar/
  weiback_collector/
    __main__.py       # headless 入口：stdin 命令 → stdout 事件
    protocol.py       # 协议常量与命令信封解析
    events.py         # 事件构造/输出与 stderr 结构化日志
    fixture_source.py # fixture 驱动的假采集流（P0-B5）
    commands.py       # hello/health/collect_*/cancel/shutdown
    extract.py        # 事件抽取：上游原始响应 → 规范化 DTO（P1-A）
    upstream.py       # 异常分类/指数退避/随机抖动/认证状态（P1-A）
    contract.py       # 协议契约校验：fixture/事件全量验证（P1-A）
    collector.py      # 事件驱动采集编排：分页+checkpoint+重试（P1-A）
    uuid7.py          # UUID v7 生成（stdlib 无原生支持）
  tests/              # unittest 测试套件
  pyproject.toml
  weiback-collector.spec  # PyInstaller 瘦打包 spec（P0-B6/P1-A）
```

## 运行

```bash
python -m weiback_collector < commands.jsonl
```

环境变量：

- `WEIBACK_COLLECTOR_FIXTURE_DIR` — fixtures 根目录（默认仓库 `fixtures/`）
- `WEIBACK_COLLECTOR_FIXTURE` — 指定 fixture 文件名（覆盖默认选择）

stdout 仅输出协议事件；stderr 输出结构化诊断日志（禁止认证秘密）。

## P1-A 采集编排

`collector.py` 是事件驱动采集编排器：对每个请求输出 `started` → 分页
数据事件（`user/post/comment/media_reference`）→ 每页 `checkpoint` → `done`。
网络边界是 `fetch_page(kind, params) -> (http_status, body)`；限流自动
指数退避重试，认证失效停止任务（`auth_required` + `done stopped`）。
fixture 模式用 `FixtureFetchPage` 从 `fixtures/raw/` 重放 hotFlowChild
两种信封格式（P1-A 交付物 3/4/5）。

## 测试

```bash
python -m unittest discover -s tests -t . -v
```

## 打包（Windows x64）

```bash
pyinstaller --noconfirm weiback-collector.spec
```

产物 `dist/weiback-collector/weiback-collector.exe`，复制为 Tauri
`binaries/weiback-collector-x86_64-pc-windows-msvc.exe` 由 `externalBin` 打包。
