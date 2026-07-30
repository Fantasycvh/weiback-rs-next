#!/usr/bin/env python3
"""本地打包脚本：自动处理 Playwright 路径"""

import os
import sys
import subprocess
import shutil
from pathlib import Path


def main():
    root = Path(__file__).parent
    dist_dir = root / "dist"
    build_dir = root / "build"

    if dist_dir.exists():
        shutil.rmtree(dist_dir)
    if build_dir.exists():
        shutil.rmtree(build_dir)

    subprocess.run([sys.executable, "-m", "playwright", "install", "chromium"], check=True)
    subprocess.run([sys.executable, "-m", "pip", "install", "pyinstaller", "upx"], check=True)

    spec_file = root / "weibo-monitor.spec"
    cmd = [
        sys.executable, "-m", "PyInstaller",
        "--clean",
        "--noconfirm",
        str(spec_file),
        "--distpath", str(dist_dir),
        "--workpath", str(build_dir),
    ]
    subprocess.run(cmd, check=True)

    print(f"\n\u2705 打包完成！输出目录: {dist_dir}")
    print(f"   - {dist_dir / 'weibo-monitor'} (Linux/macOS)")
    print(f"   - {dist_dir / 'weibo-monitor.exe'} (Windows)")


if __name__ == "__main__":
    main()
