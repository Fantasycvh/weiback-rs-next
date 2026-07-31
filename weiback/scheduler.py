import logging
import signal
import sys
import threading
from datetime import datetime, timezone, timedelta

from apscheduler.schedulers.background import BackgroundScheduler
from apscheduler.triggers.interval import IntervalTrigger

from . import collector, writer
from .task_manager import TaskManager, TaskType, TaskStatus

logger = logging.getLogger(__name__)
TZ_CST = timezone(timedelta(hours=8))


class SyncScheduler:
    def __init__(self, db_path: str, interval_minutes: int = 30, with_comments: bool = False,
                 max_pages: int | None = None, page_delay: float | None = None):
        self.db_path = db_path
        self.interval_minutes = interval_minutes
        self.with_comments = with_comments
        self.max_pages = max_pages
        self.page_delay = page_delay
        self.task_manager = TaskManager()
        self.conn = writer.connect(db_path)
        self.client = None
        self.scheduler = BackgroundScheduler(timezone="Asia/Shanghai")

    def _do_sync(self):
        from .browser import setup_playwright
        setup_playwright()
        from crawl4weibo import WeiboClient
        if self.client is None:
            self.client = WeiboClient()

        users = writer.get_monitored_users(self.conn)
        if not users:
            logger.info("没有监控用户，跳过同步")
            return

        task = self.task_manager.start_task(TaskType.SYNC_USER, f"同步 {len(users)} 个用户", total=len(users))
        for idx, user in enumerate(users, 1):
            uid = user["uid"]
            screen_name = user.get("screen_name", uid)
            logger.info("定时同步: %s (%s) [%d/%d]", screen_name, uid, idx, len(users))
            try:
                count = collector.sync_user(self.conn, self.client, uid,
                                            max_pages=self.max_pages,
                                            with_comments=self.with_comments,
                                            task_manager=self.task_manager,
                                            page_delay=self.page_delay)
                writer.write_sync_history(self.conn, uid, _now_str(), count, "success")
                logger.info("同步成功: %s, 新增 %d 条", screen_name, count)
            except Exception as e:
                logger.exception("同步失败: %s", screen_name)
                writer.write_sync_history(self.conn, uid, _now_str(), 0, "failed", str(e))
                self.task_manager.report_error("sync_user", str(e), uid)
            self.task_manager.update_progress(idx, len(users))
        self.task_manager.finish_task()

    def start(self):
        self._do_sync()
        trigger = IntervalTrigger(minutes=self.interval_minutes)
        self.scheduler.add_job(self._do_sync, trigger, id="sync_all", replace_existing=True)
        self.scheduler.start()
        logger.info("调度器已启动 (interval=%dmin)", self.interval_minutes)

    def stop(self):
        if self.scheduler.running:
            self.scheduler.shutdown(wait=False)
        try:
            self.conn.close()
        except Exception:
            pass
        logger.info("调度器已停止")

    def __del__(self):
        self.stop()


def run_daemon(db_path: str, interval_minutes: int = 30, with_comments: bool = False):
    ss = SyncScheduler(db_path, interval_minutes, with_comments)

    stop_event = threading.Event()

    def _handler(signum, frame):
        logger.info("收到退出信号，清理中...")
        ss.stop()
        stop_event.set()

    if threading.current_thread() is threading.main_thread():
        signal.signal(signal.SIGTERM, _handler)
    ss.start()

    try:
        stop_event.wait()
    except KeyboardInterrupt:
        logger.info("收到 Ctrl+C，清理中...")
        ss.stop()


def _now_str() -> str:
    return datetime.now(tz=TZ_CST).isoformat()
