import argparse

import pytest


@pytest.fixture
def parser():
    p = argparse.ArgumentParser(description="微博备份工具")
    p.add_argument("--db-path", required=True)
    p.add_argument("--now", action="store_true")
    p.add_argument("--daemon", action="store_true")
    p.add_argument("--port", type=int, default=8080)
    p.add_argument("--interval", type=int, default=30)
    p.add_argument("--with-comments", action="store_true")
    p.add_argument("--backfill", action="store_true")
    p.add_argument("--limit", type=int, default=50)
    p.add_argument("--serve-only", action="store_true")
    p.add_argument("--add-user")
    p.add_argument("--user-name")
    return p


class TestCliArgs:
    def test_default_port(self, parser):
        args = parser.parse_args(["--db-path", "test.db", "--now"])
        assert args.db_path == "test.db"
        assert args.now is True
        assert args.port == 8080

    def test_custom_port(self, parser):
        args = parser.parse_args(["--db-path", "d.db", "--daemon", "--port", "9090"])
        assert args.port == 9090

    def test_custom_interval(self, parser):
        args = parser.parse_args(["--db-path", "d.db", "--daemon", "--interval", "60"])
        assert args.interval == 60

    def test_with_comments_flag(self, parser):
        args = parser.parse_args(["--db-path", "d.db", "--now", "--with-comments"])
        assert args.with_comments is True

    def test_backfill_with_limit(self, parser):
        args = parser.parse_args(["--db-path", "d.db", "--backfill", "--limit", "100"])
        assert args.backfill is True
        assert args.limit == 100

    def test_add_user(self, parser):
        args = parser.parse_args(["--db-path", "d.db", "--add-user", "123456", "--user-name", "测试"])
        assert args.add_user == "123456"
        assert args.user_name == "测试"

    def test_add_user_without_name(self, parser):
        args = parser.parse_args(["--db-path", "d.db", "--add-user", "123456"])
        assert args.add_user == "123456"
        assert args.user_name is None

    def test_serve_only(self, parser):
        args = parser.parse_args(["--db-path", "d.db", "--serve-only"])
        assert args.serve_only is True

    def test_missing_db_path(self, parser):
        with pytest.raises(SystemExit):
            parser.parse_args(["--now"])

    def test_no_args_exits(self, parser):
        with pytest.raises(SystemExit):
            parser.parse_args([])
