# -*- mode: python ; coding: utf-8 -*-

import sys
from PyInstaller.utils.hooks import collect_data_files, collect_submodules

datas = []
try:
    datas += collect_data_files('crawl4weibo')
except:
    pass

a = Analysis(
    ['weibo_monitor.py'],
    pathex=[],
    binaries=[],
    datas=datas,
    hiddenimports=[
        'crawl4weibo',
        'crawl4weibo.models',
        'weiback',
        'weiback.writer',
        'weiback.collector',
        'weiback.scheduler',
        'weiback.models',
        'web.main',
        'apscheduler',
        'apscheduler.triggers.interval',
        'apscheduler.triggers.cron',
        'fastapi',
        'uvicorn',
        'uvicorn.logging',
        'uvicorn.loops',
        'uvicorn.loops.auto',
        'uvicorn.protocols',
        'uvicorn.protocols.http',
        'uvicorn.protocols.http.auto',
        'jinja2',
        'jinja2.ext',
    ],
    hookspath=[],
    hooksconfig={},
    runtime_hooks=[],
    excludes=[],
    noarchive=False,
)

pyz = PYZ(a.pure)

exe = EXE(
    pyz,
    a.scripts,
    a.binaries,
    a.datas,
    [],
    name='weibo-monitor',
    debug=False,
    bootloader_ignore_signals=False,
    strip=False,
    upx=True,
    upx_exclude=[],
    runtime_tmpdir=None,
    console=True,
    disable_windowed_traceback=False,
    argv_emulation=False,
    target_arch=None,
    codesign_identity=None,
    entitlements_file=None,
)
