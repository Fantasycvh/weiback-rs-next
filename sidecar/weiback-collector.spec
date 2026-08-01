# -*- mode: python ; coding: utf-8 -*-

"""WeiBack Next Sidecar 瘦打包 spec（P0-B6）。

只打包协议与 fixture 采集所需模块，不包含 FastAPI/模板/媒体下载路径。
产物基础名 `weiback-collector`（ADR-005），Tauri 按 target triple 选择。

构建（Windows x64）：
    pyinstaller --noconfirm weiback-collector.spec
输出：
    dist/weiback-collector/weiback-collector.exe  （onedir）
"""

from PyInstaller.utils.hooks import collect_submodules

block_cipher = None

a = Analysis(
    ["weiback_collector/__main__.py"],
    pathex=["."],
    binaries=[],
    datas=[],
    hiddenimports=collect_submodules("weiback_collector"),
    hookspath=[],
    hooksconfig={},
    runtime_hooks=[],
    excludes=[
        "fastapi",
        "uvicorn",
        "jinja2",
        "apscheduler",
        "templates",
        "PIL",
        "crawl4weibo",
        "playwright",
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
    [],
    exclude_binaries=True,
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

coll = COLLECT(
    exe,
    a.binaries,
    a.zipfiles,
    a.datas,
    strip=False,
    upx=False,
    upx_exclude=[],
    name="weiback-collector",
)
