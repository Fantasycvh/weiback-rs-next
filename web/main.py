import logging
from pathlib import Path
from datetime import datetime, timezone, timedelta

from fastapi import FastAPI, Request, Form
from fastapi.responses import HTMLResponse, RedirectResponse
from fastapi.staticfiles import StaticFiles
from fastapi.templating import Jinja2Templates

from weiback import writer, collector

logger = logging.getLogger("web")
TEMPLATE_DIR = Path(__file__).resolve().parent / "templates"
STATIC_DIR = Path(__file__).resolve().parent / "static"


def create_app(db_path: str) -> FastAPI:
    app = FastAPI(title="WeiBack", version="0.4.0")

    static_dir = STATIC_DIR
    static_dir.mkdir(parents=True, exist_ok=True)
    app.mount("/static", StaticFiles(directory=str(static_dir)), name="static")

    templates = Jinja2Templates(directory=str(TEMPLATE_DIR))

    def get_conn():
        return writer.connect(db_path)

    @app.get("/", response_class=HTMLResponse)
    async def dashboard(request: Request):
        conn = get_conn()
        status = writer.get_sync_status(conn)
        users = writer.get_all_monitored_users(conn)
        conn.close()
        return templates.TemplateResponse(
            request,
            "dashboard.html",
            {"status": status, "users": users},
        )

    @app.get("/users", response_class=HTMLResponse)
    async def users_page(request: Request):
        conn = get_conn()
        users = writer.get_all_monitored_users(conn)
        conn.close()
        return templates.TemplateResponse(
            request,
            "users.html",
            {"users": users},
        )

    @app.post("/users/add")
    async def add_user(uid: str = Form(...), screen_name: str = Form("")):
        conn = get_conn()
        writer.add_monitored_user(conn, uid.strip(), screen_name.strip())
        conn.close()
        return RedirectResponse("/users", status_code=303)

    @app.post("/users/remove")
    async def remove_user(uid: str = Form(...)):
        conn = get_conn()
        writer.remove_monitored_user(conn, uid.strip())
        conn.close()
        return RedirectResponse("/users", status_code=303)

    @app.get("/posts", response_class=HTMLResponse)
    async def posts_page(request: Request, uid: str = "", page: int = 1, limit: int = 50):
        conn = get_conn()
        users = writer.get_all_monitored_users(conn)

        where = ""
        params: list = []
        if uid:
            where = "WHERE p.uid = ?"
            params.append(int(uid) if uid.isdigit() else uid)

        total = conn.execute(
            f"SELECT COUNT(*) FROM posts p {where}", params
        ).fetchone()[0]

        offset = (page - 1) * limit
        rows = conn.execute(
            f"""SELECT p.id, p.uid, p.text, p.created_at, p.attitudes_count,
                       p.comments_count, p.reposts_count, p.pic_num, u.screen_name
                FROM posts p LEFT JOIN users u ON p.uid = u.id
                {where}
                ORDER BY p.id DESC LIMIT ? OFFSET ?""",
            params + [limit, offset],
        ).fetchall()

        total_pages = max(1, (total + limit - 1) // limit)
        conn.close()
        return templates.TemplateResponse(
            request,
            "posts.html",
            {
                "users": users,
                "posts": [dict(r) for r in rows],
                "total": total,
                "page": page,
                "total_pages": total_pages,
                "current_uid": uid,
            },
        )

    @app.post("/sync/now")
    async def sync_now():
        from crawl4weibo import WeiboClient
        conn = get_conn()
        users = writer.get_monitored_users(conn)
        total = 0
        if users:
            client = WeiboClient()
            for user in users:
                uid = user["uid"]
                try:
                    count = collector.sync_user(conn, client, uid)
                    writer.write_sync_history(conn, uid, _now_str(), count, "success")
                    total += count
                except Exception as e:
                    writer.write_sync_history(conn, uid, _now_str(), 0, "failed", str(e))
            client = None
        conn.close()
        logger.info("手动触发同步完成，共新增 %d 条", total)
        return RedirectResponse("/", status_code=303)

    return app


def _now_str() -> str:
    return datetime.now(tz=timezone(timedelta(hours=8))).isoformat()
