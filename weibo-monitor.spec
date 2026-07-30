# -*- mode: python ; coding: utf-8 -*-

import sys
import os
from PyInstaller.utils.hooks import collect_data_files, collect_submodules

block_cipher = None

datas = []

try:
    datas += collect_data_files('crawl4weibo')
except:
    pass

try:
    import playwright
    datas += collect_data_files('playwright')
except:
    pass

datas += [
    ('web/templates', 'web/templates'),
    ('web/static', 'web/static'),
]

hiddenimports = [
    'crawl4weibo',
    'crawl4weibo.models',
    'crawl4weibo.utils',
    'apscheduler',
    'apscheduler.triggers.cron',
    'apscheduler.triggers.interval',
    'apscheduler.executors.pool',
    'apscheduler.jobstores.sqlalchemy',
    'fastapi',
    'fastapi.templating',
    'fastapi.staticfiles',
    'uvicorn',
    'uvicorn.lifespan.on',
    'uvicorn.protocols.http.httptools_impl',
    'uvicorn.loops.auto',
    'jinja2',
    'jinja2.ext',
    'sqlite3',
    'json',
    'datetime',
    'logging',
    'signal',
]

a = Analysis(
    ['weibo-monitor.py'],
    pathex=[],
    binaries=[],
    datas=datas,
    hiddenimports=hiddenimports,
    hookspath=[],
    hooksconfig={},
    runtime_hooks=[],
    excludes=[
        'tkinter',
        'PyQt5',
        'PyQt6',
        'matplotlib',
        'numpy',
        'pandas',
        'jupyter',
        'IPython',
        'pytest',
    ],
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

coll = COLLECT(
    exe,
    a.binaries,
    a.datas,
    a.zipfiles,
    a.scripts,
    strip=False,
    upx=True,
    upx_exclude=[],
    name='weibo-monitor',
)
