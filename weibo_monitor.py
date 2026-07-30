#!/usr/bin/env python3
"""微博备份工具：Crawl4Weibo + Web GUI

用法:
  weiback --db-path data.db --daemon --port 8080
  weiback --db-path data.db --now
  weiback --db-path data.db --backfill --limit 50
  weiback --db-path data.db --add-user 123456
  weiback --db-path data.db --serve-only --port 8080
"""
import argparse
import logging
import os
import platform
import runpy
import sys
import threading
from datetime import datetime, timezone, timedelta
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
        print(f"❌ 浏览器下载失败: {e}")
        print("💡 请手动运行: playwright install chromium")
        raise
    finally:
        sys.argv = original_argv


from weiback import writer, collector
from weiback.scheduler import SyncScheduler

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s [%(name)s] %(levelname)s: %(message)s",
    datefmt="%Y-%m-%d %H:%M:%S",
)
logger = logging.getLogger("weiback")


def main():
    parser = argparse.ArgumentParser(description="微博备份工具")
    parser.add_argument("--db-path", required=True, help="SQLite 数据库路径")
    parser.add_argument("--now", action="store_true", help="单次同步后退出")
    parser.add_argument("--daemon", action="store_true", help="守护模式 + Web GUI")
    parser.add_argument("--port", type=int, default=8080, help="Web GUI 端口 (默认 8080)")
    parser.add_argument("--interval", type=int, default=30, help="同步间隔分钟数 (默认 30)")
    parser.add_argument("--with-comments", action="store_true", help="同步时一并抓取评论")
    parser.add_argument("--backfill", action="store_true", help="回补已有帖子的评论")
    parser.add_argument("--limit", type=int, default=50, help="回补评论数量上限")
    parser.add_argument("--serve-only", action="store_true", help="仅启动 Web GUI，不启动同步")
    parser.add_argument("--add-user", help="添加监控用户 (UID)")
    parser.add_argument("--user-name", help="添加监控用户时的昵称")

    args = parser.parse_args()

    if not os.path.isfile(args.db_path):
        logger.error("数据库文件不存在: %s", args.db_path)
        sys.exit(1)

    if args.add_user:
        conn = writer.connect(args.db_path)
        writer.add_monitored_user(conn, args.add_user, args.user_name or "")
        conn.close()
        logger.info("已添加监控用户: %s (%s)", args.add_user, args.user_name or "")
        return

    if args.backfill:
        _cmd_backfill(args)
        return

    if args.now:
        _cmd_now(args)
        return

    if args.serve_only:
        _start_web(args.db_path, args.port)
        return

    if args.daemon:
        _start_daemon(args)
        return

    parser.print_help()


def _cmd_backfill(args):
    setup_playwright()
    from crawl4weibo import WeiboClient
    conn = writer.connect(args.db_path)
    client = WeiboClient()
    count = collector.backfill_comments(conn, client, limit=args.limit)
    logger.info("评论回补完成，共处理 %d 篇帖子", count)
    conn.close()


def _cmd_now(args):
    setup_playwright()
    from crawl4weibo import WeiboClient
    conn = writer.connect(args.db_path)
    client = WeiboClient()
    users = writer.get_monitored_users(conn)
    if not users:
        logger.warning("没有监控用户，如需添加请使用 --add-user 参数")
        conn.close()
        return
    total = 0
    for user in users:
        uid = user["uid"]
        name = user.get("screen_name", uid)
        logger.info("单次同步: %s (%s)", name, uid)
        try:
            count = collector.sync_user(conn, client, uid, with_comments=args.with_comments)
            writer.write_sync_history(conn, uid, _now_str(), count, "success")
            total += count
            logger.info("同步成功: %s, 新增 %d 条", name, count)
        except Exception as e:
            logger.exception("同步失败: %s", name)
            writer.write_sync_history(conn, uid, _now_str(), 0, "failed", str(e))
    conn.close()
    logger.info("单次同步结束，共新增 %d 条", total)


def _start_daemon(args):
    setup_playwright()
    import uvicorn
    from web.main import create_app

    ss = SyncScheduler(args.db_path, args.interval, args.with_comments)
    sync_thread = threading.Thread(target=ss.start, daemon=True)
    sync_thread.start()

    try:
        app = create_app(args.db_path)
        logger.info("守护模式启动: Web GUI -> http://127.0.0.1:%d", args.port)
        import webbrowser
        webbrowser.open(f"http://127.0.0.1:{args.port}")
        uvicorn.run(app, host="127.0.0.1", port=args.port, log_level="info")
    finally:
        ss.stop()


def _start_web(db_path: str, port: int):
    import uvicorn
    import webbrowser
    from web.main import create_app

    app = create_app(db_path)
    logger.info("Web GUI 启动于 http://127.0.0.1:%d", port)
    webbrowser.open(f"http://127.0.0.1:{port}")
    uvicorn.run(app, host="127.0.0.1", port=port, log_level="info")


def _now_str() -> str:
    return datetime.now(tz=timezone(timedelta(hours=8))).isoformat()


if __name__ == "__main__":
    main()
