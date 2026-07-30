from fastapi.testclient import TestClient


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
