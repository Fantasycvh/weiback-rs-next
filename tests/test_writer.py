import pytest


class TestConnect:
    def test_connect_creates_tables(self, db_path):
        from weiback.writer import connect
        conn = connect(db_path)
        tables = conn.execute(
            "SELECT name FROM sqlite_master WHERE type='table' ORDER BY name"
        ).fetchall()
        names = [r[0] for r in tables]
        assert "users" in names
        conn.close()

    def test_connect_wal_mode(self, db_path):
        from weiback.writer import connect
        conn = connect(db_path)
        journal = conn.execute("PRAGMA journal_mode").fetchone()[0]
        assert journal == "wal"
        conn.close()


class TestSaveUser:
    def test_save_and_retrieve(self, conn):
        from weiback.writer import save_user
        save_user(conn, {
            "id": 12345,
            "screen_name": "测试用户",
            "avatar_hd": "https://example.com/hd.jpg",
            "avatar_large": "https://example.com/large.jpg",
            "profile_image_url": "https://example.com/profile.jpg",
            "domain": "testuser",
            "following": True,
            "follow_me": False,
        })
        row = conn.execute("SELECT * FROM users WHERE id=?", (12345,)).fetchone()
        assert row is not None
        assert row["screen_name"] == "测试用户"
        assert row["following"] == 1

    def test_replace_existing(self, conn):
        from weiback.writer import save_user
        save_user(conn, {"id": 1, "screen_name": "old", "avatar_hd": None,
                         "avatar_large": None, "profile_image_url": None,
                         "domain": None, "following": False, "follow_me": False})
        save_user(conn, {"id": 1, "screen_name": "new", "avatar_hd": None,
                         "avatar_large": None, "profile_image_url": None,
                         "domain": None, "following": True, "follow_me": False})
        row = conn.execute("SELECT screen_name, following FROM users WHERE id=1").fetchone()
        assert row["screen_name"] == "new"
        assert row["following"] == 1


class TestSavePosts:
    def test_save_and_get_last_id(self, conn):
        from weiback.writer import save_posts, get_last_post_id
        posts = [
            {"id": 1001, "uid": 12345, "text": "第一条", "created_at": "2024-01-01T00:00:00+08:00"},
            {"id": 1002, "uid": 12345, "text": "第二条", "created_at": "2024-01-02T00:00:00+08:00"},
        ]
        save_posts(conn, posts)
        last = get_last_post_id(conn, 12345)
        assert last == 1002

    def test_empty_posts(self, conn):
        from weiback.writer import save_posts
        save_posts(conn, [])
        count = conn.execute("SELECT COUNT(*) FROM posts").fetchone()[0]
        assert count == 0

    def test_multiple_users(self, conn):
        from weiback.writer import save_posts, get_last_post_id
        save_posts(conn, [{"id": 10, "uid": 1, "text": "a", "created_at": ""}])
        save_posts(conn, [{"id": 20, "uid": 2, "text": "b", "created_at": ""}])
        assert get_last_post_id(conn, 1) == 10
        assert get_last_post_id(conn, 2) == 20
        assert get_last_post_id(conn, 3) == 0

    def test_refresh_preserves_local_state(self, conn):
        from weiback.writer import save_posts

        save_posts(conn, [{"id": 1, "uid": 2, "text": "old", "created_at": ""}])
        conn.execute("UPDATE posts SET deleted=1, favorited=1, edit_count=4 WHERE id=1")
        save_posts(conn, [{
            "id": 1,
            "uid": 2,
            "text": "new",
            "created_at": "",
            "comments_count": 9,
        }])

        row = conn.execute(
            "SELECT text, comments_count, deleted, favorited, edit_count FROM posts WHERE id=1"
        ).fetchone()
        assert dict(row) == {
            "text": "new",
            "comments_count": 9,
            "deleted": 1,
            "favorited": 1,
            "edit_count": 4,
        }


class TestSaveComments:
    def test_save_and_count(self, conn):
        from weiback.writer import save_comments
        comments = [
            {"id": "c1", "post_id": 1001, "user_id": "999", "user_screen_name": "评论者A",
             "text": "好！", "created_at": "2024-01-01T00:00:00+08:00", "like_count": 5, "reply_id": None},
            {"id": "c2", "post_id": 1001, "user_id": "888", "user_screen_name": "评论者B",
             "text": "赞", "created_at": "2024-01-02T00:00:00+08:00", "like_count": 1, "reply_id": None},
        ]
        save_comments(conn, comments)
        count = conn.execute("SELECT COUNT(*) FROM comments WHERE post_id=1001").fetchone()[0]
        assert count == 2

    def test_replace(self, conn):
        from weiback.writer import save_comments
        save_comments(conn, [{"id": "c1", "post_id": 1, "text": "old"}])
        save_comments(conn, [{"id": "c1", "post_id": 1, "text": "new"}])
        row = conn.execute("SELECT text FROM comments WHERE id='c1'").fetchone()
        assert row["text"] == "new"

    def test_saves_comment_media_and_avatar(self, conn):
        from weiback.writer import save_comments

        save_comments(conn, [{
            "id": "c1",
            "post_id": 100,
            "user_id": "9",
            "user_screen_name": "评论者",
            "text": "图评",
            "pic_url": "https://example.com/comment.jpg",
            "user_avatar_url": "https://example.com/avatar.jpg",
            "root_id": "c1",
            "depth": 0,
        }])

        rows = conn.execute(
            "SELECT owner_type, owner_id, media_type, url FROM media ORDER BY media_type"
        ).fetchall()
        assert [dict(row) for row in rows] == [
            {
                "owner_type": "user",
                "owner_id": "9",
                "media_type": "avatar",
                "url": "https://example.com/avatar.jpg",
            },
            {
                "owner_type": "comment",
                "owner_id": "c1",
                "media_type": "image",
                "url": "https://example.com/comment.jpg",
            },
        ]


class TestSavePictures:
    def test_save_pictures(self, conn):
        from weiback.writer import save_pictures
        save_pictures(conn, 1001, [
            "https://example.com/pic1.jpg",
            "https://example.com/pic2.jpg",
        ], user_id=12345)
        rows = conn.execute("SELECT * FROM picture WHERE post_id=1001").fetchall()
        assert len(rows) == 2
        assert rows[0]["url"] == "https://example.com/pic1.jpg"

    def test_empty_urls(self, conn):
        from weiback.writer import save_pictures
        save_pictures(conn, 1001, [])
        count = conn.execute("SELECT COUNT(*) FROM picture WHERE post_id=1001").fetchone()[0]
        assert count == 0

    def test_duplicate_url_keeps_downloaded_path(self, conn):
        from weiback.writer import save_pictures, update_picture_path

        url = "https://example.com/pic.jpg?token=first"
        save_pictures(conn, 1001, [url], user_id=12345)
        update_picture_path(conn, "https://example.com/pic.jpg", "1001/pic.jpg")
        save_pictures(conn, 1001, ["https://example.com/pic.jpg?token=second"], user_id=12345)

        rows = conn.execute(
            "SELECT url, path FROM picture WHERE post_id=1001"
        ).fetchall()
        assert len(rows) == 1
        assert rows[0]["path"] == "1001/pic.jpg"


class TestMonitoredUsers:
    def test_add_and_get(self, conn):
        from weiback.writer import add_monitored_user, get_monitored_users, get_all_monitored_users
        add_monitored_user(conn, "123456", "测试用户")
        active = get_monitored_users(conn)
        all_ = get_all_monitored_users(conn)
        assert len(active) == 1
        assert active[0]["uid"] == "123456"
        assert all_[0]["screen_name"] == "测试用户"

    def test_remove(self, conn):
        from weiback.writer import add_monitored_user, remove_monitored_user, get_monitored_users
        add_monitored_user(conn, "u1", "用户1")
        add_monitored_user(conn, "u2", "用户2")
        remove_monitored_user(conn, "u1")
        users = get_monitored_users(conn)
        assert len(users) == 1
        assert users[0]["uid"] == "u2"

    def test_remove_nonexistent(self, conn):
        from weiback.writer import remove_monitored_user
        remove_monitored_user(conn, "nonexistent")

    def test_add_duplicate(self, conn):
        from weiback.writer import add_monitored_user, get_monitored_users
        add_monitored_user(conn, "u1")
        add_monitored_user(conn, "u1")
        users = get_monitored_users(conn)
        assert len(users) == 1


class TestSyncHistory:
    def test_write_and_get_status(self, conn):
        from weiback.writer import write_sync_history, get_sync_status
        write_sync_history(conn, "123", "2024-01-01T00:00:00+08:00", 10, "success")
        status = get_sync_status(conn)
        assert status["latest"]["new_posts_count"] == 10
        assert status["total_syncs"] == 1
        assert status["total_posts"] == 10

    def test_get_status_empty(self, conn):
        from weiback.writer import get_sync_status
        status = get_sync_status(conn)
        assert status["latest"] is None
        assert status["total_syncs"] == 0
        assert status["total_posts"] == 0

    def test_multiple_syncs_summary(self, conn):
        from weiback.writer import write_sync_history, get_sync_status
        for i in range(3):
            write_sync_history(conn, "123", f"2024-01-0{i+1}T00:00:00+08:00", (i + 1) * 5, "success")
        status = get_sync_status(conn)
        assert status["total_syncs"] == 3
        assert status["total_posts"] == 5 + 10 + 15

    def test_failed_sync(self, conn):
        from weiback.writer import write_sync_history, get_sync_status
        write_sync_history(conn, "123", "2024-01-01T00:00:00+08:00", 0, "failed", "网络错误")
        status = get_sync_status(conn)
        assert status["latest"]["status"] == "failed"
        assert status["latest"]["error_message"] == "网络错误"


class TestCommentsProgress:
    def test_get_uncommented_posts(self, conn):
        from weiback.writer import save_posts, get_uncommented_posts, mark_comments_synced
        save_posts(conn, [{"id": 1, "uid": 123, "text": "a", "created_at": ""}])
        save_posts(conn, [{"id": 2, "uid": 123, "text": "b", "created_at": ""}])
        save_posts(conn, [{"id": 3, "uid": 123, "text": "c", "created_at": ""}])

        uncommented = get_uncommented_posts(conn, limit=10)
        assert len(uncommented) == 3
        ids = {r["id"] for r in uncommented}
        assert ids == {1, 2, 3}

        mark_comments_synced(conn, 2)
        uncommented = get_uncommented_posts(conn, limit=10)
        assert len(uncommented) == 2
        assert 2 not in {r["id"] for r in uncommented}

        conn.execute(
            "UPDATE comments_sync_progress SET status='failed' WHERE post_id=2"
        )
        uncommented = get_uncommented_posts(conn, limit=10)
        assert 2 in {r["id"] for r in uncommented}

    def test_empty_uncommented(self, conn):
        from weiback.writer import get_uncommented_posts
        result = get_uncommented_posts(conn, limit=10)
        assert result == []


class TestSaveVideo:
    def test_save_video(self, conn):
        from weiback.writer import save_video
        save_video(conn, "https://example.com/video.mp4", "/local/video.mp4", 1001)
        row = conn.execute("SELECT * FROM video WHERE post_id=1001").fetchone()
        assert row is not None
        assert row["url"] == "https://example.com/video.mp4"

    def test_duplicate_video_keeps_downloaded_path(self, conn):
        from weiback.writer import save_video

        url = "https://example.com/video.mp4"
        save_video(conn, url, "/local/video.mp4", 1001)
        save_video(conn, url, "", 1001)

        rows = conn.execute(
            "SELECT url, path FROM video WHERE post_id=? AND url=?",
            (1001, url),
        ).fetchall()
        assert len(rows) == 1
        assert rows[0]["path"] == "/local/video.mp4"
