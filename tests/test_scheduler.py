import time
import threading


class TestSyncSchedulerInit:
    def test_init_creates_connection(self, db_path):
        from weiback.scheduler import SyncScheduler
        ss = SyncScheduler(db_path, interval_minutes=60)
        assert ss.db_path == db_path
        assert ss.interval_minutes == 60
        assert not ss.with_comments
        ss.stop()

    def test_init_with_comments(self, db_path):
        from weiback.scheduler import SyncScheduler
        ss = SyncScheduler(db_path, interval_minutes=30, with_comments=True)
        assert ss.with_comments
        ss.stop()

    def test_start_and_stop(self, db_path):
        from weiback.scheduler import SyncScheduler
        ss = SyncScheduler(db_path, interval_minutes=999)
        ss.start()
        assert ss.scheduler.running
        ss.stop()
        assert not ss.scheduler.running

    def test_stop_without_start(self, db_path):
        from weiback.scheduler import SyncScheduler
        ss = SyncScheduler(db_path)
        ss.stop()

    def test_del_stops_scheduler(self, db_path):
        from weiback.scheduler import SyncScheduler
        ss = SyncScheduler(db_path, interval_minutes=999)
        ss.start()
        assert ss.scheduler.running
        ss.__del__()
        assert not ss.scheduler.running


class TestRunDaemon:
    def test_run_daemon_starts_scheduler(self, db_path):
        from weiback.scheduler import run_daemon
        t = threading.Thread(target=run_daemon, args=(db_path, 999), daemon=True)
        t.start()
        time.sleep(0.3)
        assert t.is_alive()
