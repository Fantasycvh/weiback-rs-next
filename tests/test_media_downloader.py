import tempfile
from pathlib import Path

import pytest


class TestMediaDownloader:
    def test_downloads_pending_media_types_and_updates_legacy_picture(self, conn, tmp_path):
        from weiback.media_downloader import download_all_pending
        from weiback.writer import save_media, save_pictures

        image_url = "https://example.com/photo?size=large"
        video_url = "https://example.com/clip.bin?token=1"
        avatar_url = "https://example.com/avatar.webp"
        save_pictures(conn, 1001, [image_url])
        save_media(
            conn,
            owner_type="post",
            owner_id="1001",
            media_type="video",
            url=video_url,
            post_id=1001,
        )
        save_media(
            conn,
            owner_type="user",
            owner_id="9",
            media_type="avatar",
            url=avatar_url,
            user_id="9",
        )
        conn.commit()

        responses = {
            image_url.split("?")[0]: ("image/png; charset=binary", b"png"),
            video_url: ("application/octet-stream", b"mp4"),
            avatar_url: ("image/jpeg", b"jpeg"),
        }

        class FakeResponse:
            def __init__(self, content_type, body):
                self.headers = {"Content-Type": content_type}
                self._body = body

            def raise_for_status(self):
                pass

            def iter_content(self, chunk_size=8192):
                yield self._body

        class FakeSession:
            def get(self, url, headers=None, timeout=None, stream=None):
                content_type, body = responses[url]
                return FakeResponse(content_type, body)

        completed = download_all_pending(
            conn, tmp_path, max_workers=1, session=FakeSession()
        )

        assert completed == 3
        rows = conn.execute(
            "SELECT media_type, path, status, error_message, retry_count "
            "FROM media ORDER BY media_type"
        ).fetchall()
        by_type = {row["media_type"]: dict(row) for row in rows}
        assert Path(by_type["image"]["path"]).suffix == ".png"
        assert Path(by_type["video"]["path"]).suffix == ".bin"
        assert Path(by_type["avatar"]["path"]).suffix == ".jpg"
        for row in rows:
            assert row["status"] == "complete"
            assert row["error_message"] is None
            assert row["retry_count"] == 0
            assert (tmp_path / row["path"]).read_bytes()

        legacy = conn.execute(
            "SELECT path FROM picture WHERE post_id=1001"
        ).fetchone()
        assert legacy["path"] == by_type["image"]["path"]

    def test_download_failure_updates_media_retry_state(self, conn, tmp_path):
        from weiback.media_downloader import download_all_pending
        from weiback.writer import save_media

        save_media(
            conn,
            owner_type="comment",
            owner_id="c1",
            media_type="image",
            url="https://example.com/broken",
            post_id=2001,
        )
        conn.commit()

        class FailingSession:
            def get(self, url, headers=None, timeout=None, stream=None):
                raise RuntimeError("connection refused")

        completed = download_all_pending(
            conn, tmp_path, max_workers=1, session=FailingSession()
        )

        assert completed == 0
        row = conn.execute(
            "SELECT path, status, error_message, retry_count FROM media"
        ).fetchone()
        assert dict(row) == {
            "path": "",
            "status": "failed",
            "error_message": "connection refused",
            "retry_count": 1,
        }

    def test_download_all_pending_success(self, conn, monkeypatch):
        from weiback.writer import save_pictures
        from weiback.media_downloader import download_all_pending

        save_pictures(conn, 1001, [
            "https://example.com/pic1.jpg",
            "https://example.com/pic2.jpg",
        ], user_id=12345)
        conn.commit()

        responses = {
            "https://example.com/pic1.jpg": b"fake-image-1",
            "https://example.com/pic2.jpg": b"fake-image-2",
        }

        class FakeResponse:
            def __init__(self, url):
                self._body = responses[url]

            def raise_for_status(self):
                pass

            def iter_content(self, chunk_size=8192):
                yield self._body

        class FakeSession:
            def get(self, url, headers=None, timeout=None, stream=None):
                return FakeResponse(url)

        monkeypatch.setattr(
            "weiback.media_downloader.requests.Session", lambda: FakeSession()
        )

        with tempfile.TemporaryDirectory() as tmp:
            completed = download_all_pending(conn, tmp, max_workers=2)
            assert completed == 2

            restored = conn.execute(
                "SELECT path FROM picture WHERE post_id=1001"
            ).fetchall()
            paths = {r["path"] for r in restored}
            assert len(paths) == 2
            for p in paths:
                assert (Path(tmp) / p).exists()
                assert p.startswith("1001/")

            pending = conn.execute(
                "SELECT COUNT(*) FROM picture WHERE path IS NULL OR path=''"
            ).fetchone()[0]
            assert pending == 0

    def test_download_all_pending_none(self, conn, tmp_path):
        from weiback.media_downloader import download_all_pending
        completed = download_all_pending(conn, tmp_path)
        assert completed == 0

    def test_download_failure_keeps_path_empty(self, conn, monkeypatch):
        from weiback.writer import save_pictures
        from weiback.media_downloader import download_all_pending

        save_pictures(conn, 2001, ["https://example.com/broken.jpg"], user_id=1)
        conn.commit()

        class FakeResponse:
            def raise_for_status(self):
                raise RuntimeError("connection refused")

        class FakeSession:
            def get(self, url, headers=None, timeout=None, stream=None):
                return FakeResponse()

        monkeypatch.setattr(
            "weiback.media_downloader.requests.Session", lambda: FakeSession()
        )

        with tempfile.TemporaryDirectory() as tmp:
            completed = download_all_pending(conn, tmp, max_workers=1)
            assert completed == 0

            row = conn.execute(
                "SELECT path FROM picture WHERE post_id=2001"
            ).fetchone()
            assert row["path"] == ""

    def test_get_pictures_without_path(self, conn):
        from weiback.writer import save_pictures, get_pictures_without_path

        save_pictures(conn, 1, ["https://example.com/a.jpg"], user_id=1)
        conn.commit()
        pending = get_pictures_without_path(conn)
        assert len(pending) == 1
        assert pending[0]["url"] == "https://example.com/a.jpg"

        from weiback.writer import update_picture_path
        update_picture_path(conn, "https://example.com/a.jpg", "1/a.jpg")
        conn.commit()
        assert get_pictures_without_path(conn) == []

    def test_limit_applied(self, conn):
        from weiback.writer import save_pictures, get_pictures_without_path

        for i in range(3):
            save_pictures(conn, i, [f"https://example.com/pic{i}.jpg"], user_id=1)
        conn.commit()

        pending = get_pictures_without_path(conn, limit=2)
        assert len(pending) == 2


class TestDownloaderStatus:
    def test_status_queue_increments(self, conn, monkeypatch):
        from weiback.media_downloader import MediaDownloader, DownloaderStatus

        import time
        import threading

        class BlockingSession:
            def get(self, url, headers=None, timeout=None, stream=None):
                time.sleep(0.2)
                resp = FakeResponse(b"x")
                return resp

        class FakeResponse:
            def __init__(self, body):
                self._body = body

            def raise_for_status(self):
                pass

            def iter_content(self, chunk_size=8192):
                yield self._body

        monkeypatch.setattr(
            "weiback.media_downloader.requests.Session", lambda: BlockingSession()
        )

        from weiback.writer import save_pictures
        save_pictures(conn, 5, ["https://example.com/s.jpg"], user_id=1)
        conn.commit()

        dl = MediaDownloader(tempfile.mkdtemp(), max_workers=1)
        dl.enqueue_picture(conn, "https://example.com/s.jpg", 5, 1)
        time.sleep(0.05)
        status = dl.get_status()
        assert status.queue_length >= 0
        dl.shutdown(wait=True)
        assert dl.get_status().completed == 1
