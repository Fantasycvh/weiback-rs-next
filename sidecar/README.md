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
    uuid7.py          # UUID v7 生成（stdlib 无原生支持）
  tests/              # unittest 测试套件
  pyproject.toml
  weiback-collector.spec  # PyInstaller 瘦打包 spec（P0-B6）
```

## 运行

```bash
python -m weiback_collector < commands.jsonl
```

环境变量：

- `WEIBACK_COLLECTOR_FIXTURE_DIR` — fixtures 根目录（默认仓库 `fixtures/`）
- `WEIBACK_COLLECTOR_FIXTURE` — 指定 fixture 文件名（覆盖默认选择）

stdout 仅输出协议事件；stderr 输出结构化诊断日志（禁止认证秘密）。

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
