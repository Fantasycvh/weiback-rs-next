# -*- mode: python ; coding: utf-8 -*-

"""WeiBack Next Sidecar 瘦打包 spec（P0-B6）。

只打包协议与 fixture 采集所需模块，不包含 FastAPI/模板/媒体下载路径。
产物基础名 `weiback-collector`（ADR-005），Tauri 按 target triple 选择。
使用 onefile，确保 Tauri `externalBin` 可直接携带完整运行时。

构建（Windows x64）：
    pyinstaller --noconfirm weiback-collector.spec
输出：
    dist/weiback-collector.exe  （onefile）
"""

from PyInstaller.utils.hooks import collect_submodules

block_cipher = None

a = Analysis(
    ["weiback_collector_entry.py"],
    pathex=["."],
    binaries=[],
    datas=[],
    hiddenimports=collect_submodules("weiback_collector"),
    hookspath=[],
    hooksconfig={},
    runtime_hooks=[],
    excludes=[
        # P1-A：Sidecar 绝不加载采集之外的职责（交付物 6）
        "fastapi",
        "uvicorn",
        "jinja2",
        "apscheduler",
        "templates",
        "PIL",
        "crawl4weibo",
        "playwright",
        # 写库/媒体下载/调度/Web 服务路径全在 Rust 侧，禁止进入打包产物
        "weiback.writer",
        "weiback.media_downloader",
        "weiback.scheduler",
        "weiback.web",
        "weiback.weibo_adapter",
        "weiback.collector",
    ],
    win_no_prefer_redirects=False,
    win_private_assemblies=False,
    cipher=block_cipher,
    noarchive=False,
)

pyz = PYZ(a.pure, a.zipped_data, cipher=block_cipher)

exe = EXE(
    pyz,
    a.scripts,
    a.binaries,
    a.datas,
    [],
    exclude_binaries=False,
    name="weiback-collector",
    debug=False,
    bootloader_ignore_signals=False,
    strip=False,
    upx=False,
    console=True,
    disable_windowed_traceback=False,
    argv_emulation=False,
    target_arch=None,
    codesign_identity=None,
    entitlements_file=None,
)
