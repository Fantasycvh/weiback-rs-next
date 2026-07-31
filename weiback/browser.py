import os
import platform
import runpy
import sys
from pathlib import Path


def setup_playwright():
    """确保 Playwright 浏览器可用（兼容 PyInstaller 打包后路径）"""
    if platform.system() == "Windows":
        base = Path(os.environ.get("LOCALAPPDATA", Path.home() / "AppData" / "Local"))
    else:
        base = Path.home() / ".cache"
    browsers_path = base / "ms-playwright"
    browsers_path.mkdir(parents=True, exist_ok=True)
    os.environ["PLAYWRIGHT_BROWSERS_PATH"] = str(browsers_path)

    if any(p.name.startswith("chromium") for p in browsers_path.iterdir()):
        return

    original_argv = sys.argv[:]
    try:
        sys.argv = ["playwright", "install", "chromium"]
        runpy.run_module("playwright.__main__", run_name="__main__", alter_sys=True)
    except Exception as e:
        raise RuntimeError(f"浏览器下载失败: {e}") from e
    finally:
        sys.argv = original_argv
