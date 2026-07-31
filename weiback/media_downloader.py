import logging
import mimetypes
import threading
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass, field
from pathlib import Path
from urllib.parse import unquote, urlparse

import requests

from . import writer
from .task_manager import TaskManager

logger = logging.getLogger(__name__)

MAX_CONCURRENT_DOWNLOADS = 5

DOWNLOAD_HEADERS = {
    "User-Agent": (
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 "
        "(KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36"
    ),
    "Referer": "https://weibo.com/",
}


@dataclass
class DownloaderStatus:
    active_downloads: list[str] = field(default_factory=list)
    queue_length: int = 0
    completed: int = 0
    failed: int = 0


class MediaDownloaderStatusListener:
    def on_status_updated(self, status: DownloaderStatus):
        pass


class MediaDownloader:
    def __init__(
        self,
        download_dir: str | Path,
        max_workers: int = MAX_CONCURRENT_DOWNLOADS,
        session=None,
    ):
        self.download_dir = Path(download_dir)
        self.download_dir.mkdir(parents=True, exist_ok=True)
        self._executor = ThreadPoolExecutor(max_workers=max_workers, thread_name_prefix="media-dl")
        self._status_lock = threading.Lock()
        self._db_lock = threading.Lock()
        self._status = DownloaderStatus()
        self._listeners: list[MediaDownloaderStatusListener] = []
        self._session = session or requests.Session()

    def add_status_listener(self, listener: MediaDownloaderStatusListener):
        self._listeners.append(listener)

    def get_status(self) -> DownloaderStatus:
        with self._status_lock:
            return DownloaderStatus(**self._status.__dict__)

    def _notify(self):
        status = self.get_status()
        for listener in self._listeners:
            try:
                listener.on_status_updated(status)
            except Exception:
                logger.exception("status listener failed")

    def _mark_start(self, url: str):
        with self._status_lock:
            self._status.active_downloads.append(url)
            self._status.queue_length = max(0, self._status.queue_length - 1)
        self._notify()

    def _mark_done(self, url: str, ok: bool):
        with self._status_lock:
            if url in self._status.active_downloads:
                self._status.active_downloads.remove(url)
            if ok:
                self._status.completed += 1
            else:
                self._status.failed += 1
        self._notify()

    def enqueue_picture(self, conn, url: str, post_id: int, user_id: int | None = None,
                        task_manager: TaskManager | None = None):
        row = conn.execute(
            """SELECT * FROM media
               WHERE owner_type='post' AND owner_id=? AND media_type='image' AND url=?""",
            (str(post_id), url),
        ).fetchone()
        if row is None:
            writer.save_media(
                conn,
                owner_type="post",
                owner_id=str(post_id),
                media_type="image",
                url=url,
                post_id=post_id,
                user_id=str(user_id) if user_id is not None else None,
            )
            row = conn.execute(
                """SELECT * FROM media
                   WHERE owner_type='post' AND owner_id=? AND media_type='image' AND url=?""",
                (str(post_id), url),
            ).fetchone()
        self.enqueue_media(conn, dict(row), task_manager)

    def enqueue_media(self, conn, media: dict, task_manager: TaskManager | None = None):
        with self._status_lock:
            self._status.queue_length += 1
        self._notify()
        self._executor.submit(self._download_media, conn, media, task_manager)

    def _download_media(self, conn, media: dict, task_manager: TaskManager | None):
        url = media["url"]
        self._mark_start(url)
        ok = False
        temp_path = None
        try:
            resp = self._session.get(url, headers=DOWNLOAD_HEADERS, timeout=60, stream=True)
            resp.raise_for_status()
            headers = getattr(resp, "headers", {})
            rel_path = _media_path(media, headers.get("Content-Type", ""))
            dest = self.download_dir / rel_path
            dest.parent.mkdir(parents=True, exist_ok=True)
            temp_path = dest.with_name(f"{dest.name}.part")
            with open(temp_path, "wb") as f:
                for chunk in resp.iter_content(chunk_size=8192):
                    if chunk:
                        f.write(chunk)
            temp_path.replace(dest)
            temp_path = None
            with self._db_lock:
                writer.mark_media_complete(conn, media, rel_path.as_posix())
                conn.commit()
            ok = True
        except Exception as e:
            if temp_path is not None:
                temp_path.unlink(missing_ok=True)
            logger.warning("媒体下载失败: %s (%s)", url, e)
            with self._db_lock:
                writer.mark_media_failed(conn, media["id"], str(e))
                conn.commit()
            if task_manager:
                task_manager.report_error("download_media", str(e), url)
        finally:
            self._mark_done(url, ok)

    def shutdown(self, wait: bool = True):
        self._executor.shutdown(wait=wait)


def _media_path(media: dict, content_type: str) -> Path:
    parsed = urlparse(media["url"])
    url_name = Path(unquote(parsed.path)).name
    stem = Path(url_name).stem if url_name else media["id"][:16]
    extension = _extension_for(content_type, Path(url_name).suffix)
    filename = f"{stem}{extension}"
    if media["post_id"] is not None:
        return Path(str(media["post_id"])) / filename
    return Path("avatars") / str(media["owner_id"]) / filename


def _extension_for(content_type: str, url_extension: str) -> str:
    media_type = content_type.partition(";")[0].strip().lower()
    if media_type and media_type != "application/octet-stream":
        extension = mimetypes.guess_extension(media_type, strict=False)
        if extension:
            return ".jpg" if extension in {".jpe", ".jpeg"} else extension
    if url_extension and url_extension[1:].isalnum():
        return url_extension.lower()
    return ".bin"


def download_all_pending(conn, download_dir: str | Path, task_manager: TaskManager | None = None,
                         max_workers: int = MAX_CONCURRENT_DOWNLOADS, limit: int | None = None,
                         session=None):
    pending = writer.get_pending_media(conn, limit)
    if not pending:
        logger.info("没有待下载的媒体")
        return 0

    logger.info("开始下载 %d 个媒体文件 -> %s", len(pending), download_dir)
    dl = MediaDownloader(download_dir, max_workers=max_workers, session=session)
    for row in pending:
        dl.enqueue_media(conn, row, task_manager=task_manager)
    dl.shutdown(wait=True)
    status = dl.get_status()
    logger.info(
        "媒体下载完成: 成功 %d, 失败 %d, 队列 %d",
        status.completed, status.failed, status.queue_length,
    )
    return status.completed
