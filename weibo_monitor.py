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
import sys
import threading
from datetime import datetime, timezone, timedelta
from pathlib import Path

from weiback.browser import setup_playwright


def get_default_db_path() -> Path:
    """获取各平台默认数据库路径"""
    home = Path.home()
    if sys.platform == "win32":
        return home / "AppData" / "Roaming" / "weiback" / "weiback.db"
    if sys.platform == "darwin":
        return home / "Library" / "Application Support" / "weiback" / "weiback.db"
    return home / ".local" / "share" / "weiback" / "weiback.db"


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
    parser.add_argument("--config", help="配置文件路径 (默认自动检测)")
    parser.add_argument("--db-path", help="SQLite 数据库路径（默认自动检测）")
    parser.add_argument("--now", action="store_true", help="单次同步后退出")
    parser.add_argument("--daemon", action="store_true", help="守护模式 + Web GUI")
    parser.add_argument("--port", type=int, default=argparse.SUPPRESS, help="Web GUI 端口")
    parser.add_argument("--interval", type=int, default=argparse.SUPPRESS, help="同步间隔分钟数")
    parser.add_argument("--with-comments", action="store_true", default=argparse.SUPPRESS, help="同步时一并抓取评论")
    parser.add_argument("--max-pages", type=int, default=argparse.SUPPRESS, help="每用户最大页数 (0=不限)")
    parser.add_argument("--page-delay", type=float, default=argparse.SUPPRESS, help="翻页间隔秒数")
    parser.add_argument("--backfill", action="store_true", help="回补已有帖子的评论")
    parser.add_argument("--download-images", action="store_true", help="下载未落盘的图片到本地")
    parser.add_argument("--download-dir", help="图片下载目录（默认: db 同级 images/）")
    parser.add_argument("--limit", type=int, default=50, help="回补评论数量上限")
    parser.add_argument("--serve-only", action="store_true", help="仅启动 Web GUI，不启动同步")
    parser.add_argument("--add-user", help="添加监控用户 (UID)")
    parser.add_argument("--user-name", help="添加监控用户时的昵称")

    args = parser.parse_args()

    from weiback.config import load_config
    cfg = load_config(getattr(args, "config", None))

    args.db_path = getattr(args, "db_path", None) or cfg.db_path or str(get_default_db_path())
    args.port = getattr(args, "port", cfg.port)
    args.interval = getattr(args, "interval", cfg.interval_minutes)
    args.with_comments = getattr(args, "with_comments", cfg.with_comments)
    args.max_pages = getattr(args, "max_pages", cfg.max_pages)
    args.page_delay = getattr(args, "page_delay", cfg.page_delay)
    args.download_dir = args.download_dir or cfg.download_dir or str(Path(args.db_path).parent / "images")
    Path(args.db_path).parent.mkdir(parents=True, exist_ok=True)

    action_flags = [args.now, args.daemon, args.backfill, args.serve_only, args.add_user, args.download_images]
    if not any(action_flags):
        args.daemon = True

    if args.add_user:
        conn = writer.connect(args.db_path)
        writer.add_monitored_user(conn, args.add_user, args.user_name or "")
        conn.close()
        logger.info("已添加监控用户: %s (%s)", args.add_user, args.user_name or "")
        return

    if args.download_images:
        _cmd_download_images(args)
        return

    if args.backfill:
        _cmd_backfill(args)
        return

    if args.now:
        _cmd_now(args)
        return

    if args.serve_only:
        _start_web(args.db_path, args.port, args.download_dir)
        return

    if args.daemon:
        _start_daemon(args)
        return


def _cmd_backfill(args):
    setup_playwright()
    from crawl4weibo import WeiboClient
    conn = writer.connect(args.db_path)
    client = WeiboClient()
    count = collector.backfill_comments(conn, client, limit=args.limit)
    logger.info("评论回补完成，共处理 %d 篇帖子", count)
    conn.close()


def _cmd_download_images(args):
    from weiback.media_downloader import download_all_pending
    conn = writer.connect(args.db_path)
    completed = download_all_pending(conn, args.download_dir, max_workers=5)
    logger.info("图片下载完成: 共下载 %d 张", completed)
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
            count = collector.sync_user(conn, client, uid, max_pages=args.max_pages or None,
                                        with_comments=args.with_comments, page_delay=args.page_delay)
            writer.write_sync_history(conn, uid, _now_str(), count, "success")
            total += count
            logger.info("同步成功: %s, 新增 %d 条", name, count)
        except Exception as e:
            logger.exception("同步失败: %s", name)
            writer.write_sync_history(conn, uid, _now_str(), 0, "failed", str(e))
    conn.close()
    logger.info("单次同步结束，共新增 %d 条", total)


def _start_daemon(args):
    import uvicorn
    from web.main import create_app

    ss = SyncScheduler(args.db_path, args.interval, args.with_comments,
                       max_pages=args.max_pages or None, page_delay=args.page_delay)
    sync_thread = threading.Thread(target=ss.start, daemon=True)
    sync_thread.start()

    try:
        app = create_app(args.db_path, download_dir=args.download_dir)
        logger.info("守护模式启动: Web GUI -> http://127.0.0.1:%d", args.port)
        import webbrowser
        webbrowser.open(f"http://127.0.0.1:{args.port}")
        uvicorn.run(app, host="127.0.0.1", port=args.port, log_level="info")
    finally:
        ss.stop()


def _start_web(db_path: str, port: int, download_dir: str = ""):
    import uvicorn
    import webbrowser
    from web.main import create_app

    app = create_app(db_path, download_dir=download_dir)
    logger.info("Web GUI 启动于 http://127.0.0.1:%d", port)
    webbrowser.open(f"http://127.0.0.1:{port}")
    uvicorn.run(app, host="127.0.0.1", port=port, log_level="info")


def _now_str() -> str:
    return datetime.now(tz=timezone(timedelta(hours=8))).isoformat()


if __name__ == "__main__":
    main()
