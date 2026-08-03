from fastapi.testclient import TestClient


def _save_post(conn, **overrides):
    from weiback.writer import save_posts

    post = {
        "id": 1,
        "uid": 100,
        "text": "测试正文",
        "created_at": "2024-05-10T12:00:00+08:00",
        "content_status": "complete",
    }
    post.update(overrides)
    save_posts(conn, [post])
    conn.commit()


class TestCreateApp:
    def test_create_app_returns_fastapi(self, db_path):
        from web.main import create_app
        app = create_app(db_path)
        assert app.title == "WeiBack"

    def test_dashboard_returns_html(self, db_path):
        from web.main import create_app
        app = create_app(db_path)
        client = TestClient(app)
        resp = client.get("/")
        assert resp.status_code == 200
        assert "text/html" in resp.headers["content-type"]

    def test_dashboard_shows_empty_state(self, db_path):
        from web.main import create_app
        app = create_app(db_path)
        client = TestClient(app)
        resp = client.get("/")
        assert resp.status_code == 200
        assert "暂无同步记录" in resp.text
        assert "暂无监控用户" in resp.text

    def test_users_page_returns_html(self, db_path):
        from web.main import create_app
        app = create_app(db_path)
        client = TestClient(app)
        resp = client.get("/users")
        assert resp.status_code == 200

    def test_posts_page_returns_html(self, db_path):
        from web.main import create_app
        app = create_app(db_path)
        client = TestClient(app)
        resp = client.get("/posts")
        assert resp.status_code == 200

    def test_posts_page_excludes_deleted_by_default(self, db_path, conn):
        _save_post(conn, id=1, text="保留的微博")
        _save_post(conn, id=2, text="已删除的微博")
        conn.execute("UPDATE posts SET deleted=1 WHERE id=2")
        conn.commit()

        from web.main import create_app
        resp = TestClient(create_app(db_path)).get("/posts")

        assert resp.status_code == 200
        assert "保留的微博" in resp.text
        assert "已删除的微博" not in resp.text
        assert "共 1 条" in resp.text

    def test_posts_page_combines_all_search_filters(self, db_path, conn):
        _save_post(
            conn,
            id=11,
            uid=100,
            text="命中的关键词",
            created_at="2024-05-10T23:59:59+08:00",
            content_status="complete",
        )
        _save_post(conn, id=12, uid=100, text="不含搜索内容")
        _save_post(conn, id=13, uid=200, text="命中的关键词")
        _save_post(
            conn,
            id=14,
            uid=100,
            text="命中的关键词",
            created_at="2024-05-11T00:00:00+08:00",
        )
        _save_post(conn, id=15, uid=100, text="命中的关键词", content_status="partial")

        from web.main import create_app
        resp = TestClient(create_app(db_path)).get(
            "/posts",
            params={
                "q": "命中",
                "uid": "100",
                "date_from": "2024-05-10",
                "date_to": "2024-05-10",
                "content_status": "complete",
            },
        )

        assert resp.status_code == 200
        assert "命中的关键词" in resp.text
        assert "不含搜索内容" not in resp.text
        assert "共 1 条" in resp.text
        assert 'value="命中"' in resp.text
        assert 'value="2024-05-10"' in resp.text

    def test_post_detail_shows_repost_media_and_comment_tree(self, db_path, conn, tmp_path):
        from weiback.writer import save_comments, save_media

        _save_post(conn, id=90, uid=200, text="转发原文正文")
        _save_post(conn, id=91, uid=100, text="外层转发正文", retweeted_id=90)
        save_media(
            conn,
            owner_type="post",
            owner_id="91",
            post_id=91,
            media_type="image",
            url="https://example.com/outer.jpg",
            path="outer.jpg",
        )
        save_media(
            conn,
            owner_type="post",
            owner_id="90",
            post_id=90,
            media_type="video",
            url="https://example.com/original.mp4",
        )
        save_comments(conn, [
            {
                "id": "root-1",
                "post_id": 91,
                "user_screen_name": "一级用户",
                "text": "一级评论",
                "root_id": "root-1",
                "depth": 0,
            },
            {
                "id": "reply-1",
                "post_id": 91,
                "user_screen_name": "二级用户",
                "text": "二级评论",
                "root_id": "root-1",
                "parent_id": "root-1",
                "depth": 1,
            },
        ])
        save_media(
            conn,
            owner_type="comment",
            owner_id="reply-1",
            post_id=91,
            media_type="image",
            url="https://example.com/reply.jpg",
        )
        conn.commit()

        from web.main import create_app
        resp = TestClient(create_app(db_path, download_dir=str(tmp_path))).get("/posts/91")

        assert resp.status_code == 200
        assert "外层转发正文" in resp.text
        assert "转发原文正文" in resp.text
        assert 'src="/images/outer.jpg"' in resp.text
        assert 'src="https://example.com/reply.jpg"' in resp.text
        assert 'href="https://example.com/original.mp4"' in resp.text
        assert resp.text.index("一级评论") < resp.text.index("二级评论")
        assert 'class="comment-replies"' in resp.text

    def test_missing_post_detail_returns_404(self, db_path):
        from web.main import create_app

        resp = TestClient(create_app(db_path)).get("/posts/999")

        assert resp.status_code == 404

    def test_fetches_comment_replies_on_demand(self, db_path, conn):
        from weiback.writer import save_comments

        _save_post(conn, id=91)
        save_comments(conn, [{
            "id": "root-1",
            "post_id": 91,
            "text": "一级评论",
            "root_id": "root-1",
            "depth": 0,
        }])
        conn.commit()

        class Client:
            def _request(self, url, params, use_proxy=True):
                return {
                    "data": [{
                        "id": "reply-1",
                        "text": "按需抓取的回复",
                        "user": {"id": "9", "screen_name": "回复者"},
                    }],
                    "max_id": 0,
                    "max_id_type": 0,
                }

        from web.main import create_app
        client = TestClient(
            create_app(db_path, client_factory=Client), follow_redirects=False
        )
        resp = client.post("/comments/root-1/replies")

        assert resp.status_code == 303
        assert resp.headers["location"] == "/posts/91"
        saved = conn.execute(
            "SELECT text, root_id, depth FROM comments WHERE id='reply-1'"
        ).fetchone()
        assert dict(saved) == {
            "text": "按需抓取的回复",
            "root_id": "root-1",
            "depth": 1,
        }

    def test_fetch_replies_sets_up_playwright_before_default_client(
        self, db_path, conn, monkeypatch
    ):
        from weiback.writer import save_comments

        _save_post(conn, id=92)
        save_comments(conn, [{
            "id": "root-2",
            "post_id": 92,
            "text": "一级评论",
            "root_id": "root-2",
            "depth": 0,
        }])
        conn.commit()
        events = []

        monkeypatch.setattr(
            "weiback.browser.setup_playwright", lambda: events.append("setup")
        )

        class Client:
            def __init__(self):
                events.append("client")

            def _request(self, url, params, use_proxy=True):
                return {"data": [], "max_id": 0, "max_id_type": 0}

        monkeypatch.setattr("crawl4weibo.WeiboClient", Client)

        from web.main import create_app
        resp = TestClient(create_app(db_path), follow_redirects=False).post(
            "/comments/root-2/replies"
        )

        assert resp.status_code == 303
        assert events == ["setup", "client"]

    def test_fetch_replies_rejects_missing_comment(self, db_path):
        from web.main import create_app

        resp = TestClient(create_app(db_path)).post("/comments/missing/replies")

        assert resp.status_code == 404

    def test_add_user_via_web(self, db_path):
        from web.main import create_app
        app = create_app(db_path)
        client = TestClient(app, follow_redirects=False)
        resp = client.post("/users/add", data={"uid": "123456", "screen_name": "测试"})
        assert resp.status_code == 303

    def test_sync_now_redirects(self, db_path, monkeypatch):
        from web.main import create_app
        app = create_app(db_path)
        client = TestClient(app, follow_redirects=False)
        resp = client.post("/sync/now")
        assert resp.status_code == 303
        assert resp.headers["location"] == "/"

    def test_sync_now_sets_up_playwright_before_default_client(
        self, db_path, conn, monkeypatch
    ):
        from weiback import writer

        writer.add_monitored_user(conn, "123456", "测试")
        events = []
        monkeypatch.setattr(
            "weiback.browser.setup_playwright", lambda: events.append("setup")
        )

        class Client:
            def __init__(self):
                events.append("client")

        monkeypatch.setattr("crawl4weibo.WeiboClient", Client)
        monkeypatch.setattr("weiback.collector.sync_user", lambda *args: 0)

        from web.main import create_app
        resp = TestClient(create_app(db_path), follow_redirects=False).post("/sync/now")

        assert resp.status_code == 303
        assert events == ["setup", "client"]

    def test_task_status_does_not_consume_errors(self, db_path):
        from web.main import create_app
        from weiback.task_manager import TaskManager

        task_manager = TaskManager()
        task_manager.get_and_clear_errors()
        task_manager.report_error("rate_limit", "请求受限", "post-1")
        client = TestClient(create_app(db_path))

        try:
            first = client.get("/api/task/status")
            second = client.get("/api/task/status")

            expected = [{"type": "rate_limit", "message": "请求受限", "item": "post-1"}]
            assert first.json()["errors"] == expected
            assert second.json()["errors"] == expected
        finally:
            task_manager.get_and_clear_errors()


class TestManualBackup:
    def test_backup_page_returns_html(self, db_path):
        from web.main import create_app
        resp = TestClient(create_app(db_path)).get("/backup")
        assert resp.status_code == 200
        assert "手动抓取" in resp.text
        assert "开始备份" in resp.text

    def test_backup_page_lists_monitored_users(self, db_path, conn):
        from weiback.writer import add_monitored_user

        add_monitored_user(conn, "123456", "测试用户")
        from web.main import create_app
        resp = TestClient(create_app(db_path)).get("/backup")

        assert resp.status_code == 200
        assert "测试用户" in resp.text
        assert 'value="123456"' in resp.text

    def test_backup_start_runs_sync_with_selected_options(self, db_path, conn, monkeypatch):
        import time
        from web.main import create_app

        captured = {}

        def fake_sync(conn, client, uid, **kw):
            captured["uid"] = uid
            captured.update(kw)
            return 3

        monkeypatch.setattr("weiback.browser.setup_playwright", lambda: None)
        monkeypatch.setattr("weiback.collector.sync_user", fake_sync)

        class FakeClient:
            pass

        monkeypatch.setattr("crawl4weibo.WeiboClient", FakeClient)

        resp = TestClient(create_app(db_path), follow_redirects=False).post(
            "/backup/start",
            data={
                "uid": "999",
                "content_type": "original",
                "pages": "3",
                "with_comments": "1",
                "comment_limit": "20",
            },
        )
        assert resp.status_code == 303
        assert resp.headers["location"] == "/backup"

        deadline = time.time() + 5
        while time.time() < deadline and "content_type" not in captured:
            time.sleep(0.05)
        assert captured["uid"] == "999"
        assert captured["content_type"] == "original"
        assert captured["max_pages"] == 3
        assert captured["with_comments"] is True
        assert captured["comment_limit"] == 20

    def test_fetch_post_comments_runs_backfill(self, db_path, conn, monkeypatch):
        import time
        from web.main import create_app

        _save_post(conn, id=77, uid=100, text="待回补评论")
        captured = {}

        def fake_backfill(conn, client, post_id, **kw):
            captured["post_id"] = post_id
            return True

        monkeypatch.setattr("weiback.browser.setup_playwright", lambda: None)
        monkeypatch.setattr("weiback.collector.backfill_post_comments", fake_backfill)

        class FakeClient:
            pass

        monkeypatch.setattr("crawl4weibo.WeiboClient", FakeClient)

        resp = TestClient(create_app(db_path), follow_redirects=False).post(
            "/posts/77/fetch-comments"
        )
        assert resp.status_code == 303
        assert resp.headers["location"] == "/posts/77"

        deadline = time.time() + 5
        while time.time() < deadline and "post_id" not in captured:
            time.sleep(0.05)
        assert captured["post_id"] == 77

    def test_fetch_post_comments_missing_post_404(self, db_path):
        from web.main import create_app
        resp = TestClient(create_app(db_path)).post("/posts/8888/fetch-comments")
        assert resp.status_code == 404


class TestCommentTreeRendering:
    def _save_tree(self, conn):
        from weiback.writer import save_comments

        _save_post(conn, id=41, uid=100, text="带评论树的微博")
        save_comments(conn, [
            {
                "id": "root-1",
                "post_id": 41,
                "user_screen_name": "根评论者",
                "text": "根评论",
                "root_id": "root-1",
                "parent_id": None,
                "depth": 0,
            },
            {
                "id": "child-1",
                "post_id": 41,
                "user_screen_name": "子回复者",
                "text": "子回复",
                "root_id": "root-1",
                "parent_id": "root-1",
                "depth": 1,
            },
            {
                "id": "grandchild-1",
                "post_id": 41,
                "user_screen_name": "孙回复者",
                "text": "孙回复",
                "root_id": "root-1",
                "parent_id": "child-1",
                "depth": 2,
            },
            {
                "id": "root-2",
                "post_id": 41,
                "user_screen_name": "第二个根评论者",
                "text": "第二个根评论",
                "root_id": "root-2",
                "parent_id": None,
                "depth": 0,
            },
        ])
        conn.commit()

    def test_renders_multilevel_comment_tree(self, db_path, conn):
        self._save_tree(conn)

        from web.main import create_app
        resp = TestClient(create_app(db_path)).get("/posts/41")

        assert resp.status_code == 200
        html = resp.text
        assert "根评论" in html
        assert "子回复" in html
        assert "孙回复" in html
        assert "第二个根评论" in html
        # 孙回复应嵌套在子回复之下（按出现顺序验证层级）
        assert html.index("根评论") < html.index("子回复") < html.index("孙回复")
        # 缩进层级体现递归嵌套
        assert "孙回复" in html
        assert html.count("comment-replies") >= 2

    def test_comment_tree_groups_under_root(self, db_path, conn):
        self._save_tree(conn)

        from web.main import _build_comment_tree
        conn2 = __import__("weiback.writer", fromlist=["connect"]).connect(db_path)
        try:
            comments = [dict(r) for r in conn2.execute(
                "SELECT * FROM comments WHERE post_id=41 ORDER BY rowid"
            ).fetchall()]
            media = {}
            tree = _build_comment_tree(comments, media)
        finally:
            conn2.close()

        assert [c["id"] for c in tree] == ["root-1", "root-2"]
        root = tree[0]
        assert [c["id"] for c in root["replies"]] == ["child-1"]
        child = root["replies"][0]
        assert [c["id"] for c in child["replies"]] == ["grandchild-1"]
        assert tree[1]["replies"] == []
