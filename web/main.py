import logging
import threading
from pathlib import Path
from datetime import datetime, timezone, timedelta
from urllib.parse import quote, urlencode

from fastapi import FastAPI, Request, Form, HTTPException
from fastapi.responses import HTMLResponse, RedirectResponse, JSONResponse
from fastapi.staticfiles import StaticFiles
from fastapi.templating import Jinja2Templates

from weiback import writer, collector
from weiback.task_manager import TaskManager, TaskType

logger = logging.getLogger("web")
TEMPLATE_DIR = Path(__file__).resolve().parent / "templates"
STATIC_DIR = Path(__file__).resolve().parent / "static"

_download_lock = threading.Lock()


def create_app(
    db_path: str,
    download_dir: str | None = None,
    client_factory=None,
) -> FastAPI:
    app = FastAPI(title="WeiBack", version="0.5.1")

    static_dir = STATIC_DIR
    static_dir.mkdir(parents=True, exist_ok=True)
    app.mount("/static", StaticFiles(directory=str(static_dir)), name="static")

    if download_dir:
        Path(download_dir).mkdir(parents=True, exist_ok=True)
        app.mount("/images", StaticFiles(directory=str(download_dir)), name="images")

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
    async def posts_page(
        request: Request,
        q: str = "",
        uid: str = "",
        date_from: str = "",
        date_to: str = "",
        content_status: str = "",
        page: int = 1,
        limit: int = 50,
    ):
        conn = get_conn()
        users = writer.get_all_monitored_users(conn)

        conditions = ["p.deleted = 0"]
        params: list[object] = []
        if q:
            escaped_q = q.replace("\\", "\\\\").replace("%", "\\%").replace("_", "\\_")
            conditions.append("p.text LIKE ? ESCAPE '\\'")
            params.append(f"%{escaped_q}%")
        if uid:
            conditions.append("CAST(p.uid AS TEXT) = ?")
            params.append(uid)
        if date_from:
            conditions.append("substr(p.created_at, 1, 10) >= ?")
            params.append(date_from)
        if date_to:
            conditions.append("substr(p.created_at, 1, 10) <= ?")
            params.append(date_to)
        if content_status:
            conditions.append("p.content_status = ?")
            params.append(content_status)
        where = "WHERE " + " AND ".join(conditions)

        total = conn.execute(
            f"SELECT COUNT(*) FROM posts p {where}", params
        ).fetchone()[0]

        page = max(1, page)
        limit = min(max(1, limit), 200)

        total_pages = max(1, (total + limit - 1) // limit)
        page = max(1, min(page, total_pages))
        offset = (page - 1) * limit
        rows = conn.execute(
            f"""SELECT p.id, p.uid, p.text, p.created_at, p.attitudes_count,
                       p.comments_count, p.reposts_count, p.pic_num, u.screen_name
                FROM posts p LEFT JOIN users u ON p.uid = u.id
                {where}
                ORDER BY p.id DESC LIMIT ? OFFSET ?""",
            params + [limit, offset],
        ).fetchall()

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
                "q": q,
                "date_from": date_from,
                "date_to": date_to,
                "content_status": content_status,
                "filter_query": urlencode({
                    key: value
                    for key, value in {
                        "q": q,
                        "uid": uid,
                        "date_from": date_from,
                        "date_to": date_to,
                        "content_status": content_status,
                        "limit": limit if limit != 50 else "",
                    }.items()
                    if value != ""
                }),
            },
        )

    @app.get("/posts/{post_id}", response_class=HTMLResponse)
    async def post_detail(request: Request, post_id: int):
        conn = get_conn()
        try:
            post_row = _get_post(conn, post_id)
            if post_row is None:
                raise HTTPException(status_code=404, detail="微博不存在")

            post = dict(post_row)
            retweeted = None
            if post["retweeted_id"] is not None:
                retweeted_row = _get_post(conn, post["retweeted_id"])
                retweeted = dict(retweeted_row) if retweeted_row else None

            post["media"] = _get_media(conn, "post", str(post_id))
            if retweeted is not None:
                retweeted["media"] = _get_media(conn, "post", str(retweeted["id"]))

            comments = [dict(row) for row in conn.execute(
                """SELECT * FROM comments
                   WHERE post_id=? ORDER BY created_at, rowid""",
                (post_id,),
            ).fetchall()]
            comment_media = {
                comment["id"]: _get_media(conn, "comment", str(comment["id"]))
                for comment in comments
            }
            comment_tree = _build_comment_tree(comments, comment_media)
        finally:
            conn.close()

        return templates.TemplateResponse(
            request,
            "post_detail.html",
            {"post": post, "retweeted": retweeted, "comments": comment_tree},
        )

    @app.post("/comments/{root_comment_id}/replies")
    def fetch_replies(root_comment_id: str):
        conn = get_conn()
        try:
            root = conn.execute(
                "SELECT post_id FROM comments WHERE id=? AND depth=0",
                (root_comment_id,),
            ).fetchone()
            if root is None:
                raise HTTPException(status_code=404, detail="一级评论不存在")

            if client_factory is None:
                from crawl4weibo import WeiboClient
                from weiback.browser import setup_playwright

                setup_playwright()
                client = WeiboClient()
            else:
                client = client_factory()
            collector.fetch_comment_replies(
                conn,
                client,
                root["post_id"],
                root_comment_id,
            )
            return RedirectResponse(f"/posts/{root['post_id']}", status_code=303)
        finally:
            conn.close()

    @app.get("/backup", response_class=HTMLResponse)
    async def backup_page(request: Request):
        conn = get_conn()
        users = writer.get_all_monitored_users(conn)
        conn.close()
        return templates.TemplateResponse(
            request,
            "backup.html",
            {"users": users},
        )

    @app.post("/backup/start")
    async def backup_start(
        uid: str = Form(...),
        content_type: str = Form("all"),
        pages: int = Form(1),
        with_comments: str | None = Form(None),
        comment_limit: int = Form(10),
    ):
        if not uid.strip():
            raise HTTPException(status_code=400, detail="uid 不能为空")
        try:
            TaskManager().start_task(
                TaskType.SYNC_USER,
                f"手动备份用户 {uid.strip()}（{content_type}）",
                total=max(1, pages),
            )
        except RuntimeError as e:
            raise HTTPException(status_code=409, detail=str(e))

        def _run():
            conn = get_conn()
            try:
                if client_factory is not None:
                    client = client_factory()
                else:
                    from crawl4weibo import WeiboClient
                    from weiback.browser import setup_playwright

                    setup_playwright()
                    client = WeiboClient()
                try:
                    count = collector.sync_user(
                        conn,
                        client,
                        uid.strip(),
                        max_pages=max(1, pages),
                        with_comments=bool(with_comments),
                        comment_limit=max(1, min(comment_limit, 100)),
                        content_type=content_type,
                        task_manager=TaskManager(),
                    )
                    TaskManager().finish_task()
                    logger.info("手动备份完成: uid=%s 新增 %d 条", uid, count)
                except Exception as e:
                    TaskManager().fail_task(str(e))
                    logger.exception("手动备份失败 uid=%s", uid)
            finally:
                conn.close()

        threading.Thread(target=_run, daemon=True).start()
        return RedirectResponse("/backup", status_code=303)

    @app.post("/posts/{post_id}/fetch-comments")
    async def fetch_post_comments(post_id: int):
        conn = get_conn()
        try:
            post_row = conn.execute(
                "SELECT 1 FROM posts WHERE id=?", (post_id,)
            ).fetchone()
        finally:
            conn.close()
        if post_row is None:
            raise HTTPException(status_code=404, detail="微博不存在")

        def _run():
            conn = get_conn()
            try:
                if client_factory is not None:
                    client = client_factory()
                else:
                    from crawl4weibo import WeiboClient
                    from weiback.browser import setup_playwright

                    setup_playwright()
                    client = WeiboClient()
                try:
                    got = collector.backfill_post_comments(conn, client, post_id)
                    logger.info("单帖评论回补完成: post=%s got=%s", post_id, got)
                except Exception as e:
                    logger.exception("单帖评论回补失败 post=%s", post_id)
            finally:
                conn.close()

        threading.Thread(target=_run, daemon=True).start()
        return RedirectResponse(f"/posts/{post_id}", status_code=303)

    @app.post("/sync/now")
    async def sync_now():
        from crawl4weibo import WeiboClient
        from weiback.browser import setup_playwright

        conn = get_conn()
        users = writer.get_monitored_users(conn)
        total = 0
        if users:
            setup_playwright()
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

    @app.post("/api/download/images")
    async def download_images():
        if not download_dir:
            return JSONResponse({"ok": False, "error": "未配置下载目录"}, status_code=400)

        def _run():
            from weiback.media_downloader import download_all_pending
            conn = get_conn()
            try:
                completed = download_all_pending(conn, download_dir or "", max_workers=5)
                logger.info("后台下载完成，共 %d 张", completed)
            except Exception as e:
                logger.exception("后台下载失败")
            finally:
                conn.close()
                _download_lock.release()

        if not _download_lock.acquire(blocking=False):
            return JSONResponse({"ok": False, "error": "下载任务已在运行"}, status_code=409)
        threading.Thread(target=_run, daemon=True).start()
        return JSONResponse({"ok": True, "message": "下载任务已启动"})

    @app.get("/api/pictures/pending")
    async def pictures_pending():
        conn = get_conn()
        pending = writer.get_pictures_without_path(conn)
        total = conn.execute("SELECT COUNT(*) FROM picture").fetchone()[0]
        conn.close()
        return JSONResponse({"pending": len(pending), "total": total})

    @app.get("/api/task/status")
    async def task_status():
        tm = TaskManager()
        task = tm.get_current_task()
        errors = tm.get_errors()
        return JSONResponse({
            "task": _task_to_dict(task) if task else None,
            "errors": [{"type": e.error_type, "message": e.message, "item": e.item_id} for e in errors],
        })

    return app


def _task_to_dict(task) -> dict:
    return {
        "id": task.id,
        "type": task.task_type.value,
        "description": task.description,
        "status": task.status.value,
        "progress": task.progress,
        "total": task.total,
        "error": task.error,
    }


def _get_post(conn, post_id: int):
    return conn.execute(
        """SELECT p.*, u.screen_name
           FROM posts p LEFT JOIN users u ON p.uid = u.id
           WHERE p.id=?""",
        (post_id,),
    ).fetchone()


def _get_media(conn, owner_type: str, owner_id: str) -> list[dict]:
    rows = conn.execute(
        """SELECT media_type, url, path, status
           FROM media WHERE owner_type=? AND owner_id=?
           ORDER BY created_at, id""",
        (owner_type, owner_id),
    ).fetchall()
    result = []
    for row in rows:
        item = dict(row)
        path = (item["path"] or "").replace("\\", "/").lstrip("/")
        item["display_url"] = f"/images/{quote(path, safe='/')}" if path else item["url"]
        result.append(item)
    return result


def _build_comment_tree(comments: list[dict], media: dict[str, list[dict]]) -> list[dict]:
    by_id: dict[str, dict] = {}
    for comment in comments:
        comment["media"] = media.get(comment["id"], [])
        comment["replies"] = []
        by_id[comment["id"]] = comment

    roots: list[dict] = []
    for comment in comments:
        parent_id = comment.get("parent_id")
        parent = by_id.get(parent_id) if parent_id else None
        if parent is None or parent is comment:
            roots.append(comment)
        else:
            parent["replies"].append(comment)
    return roots


def _now_str() -> str:
    return datetime.now(tz=timezone(timedelta(hours=8))).isoformat()
