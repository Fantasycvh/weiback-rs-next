from datetime import datetime, timezone, timedelta

import pytest


class TestToRfc3339:
    def test_naive_datetime(self):
        from weiback.collector import _to_rfc3339
        dt = datetime(2024, 1, 1, 12, 0, 0)
        result = _to_rfc3339(dt)
        assert result.endswith("+08:00")

    def test_aware_datetime(self):
        from weiback.collector import _to_rfc3339
        dt = datetime(2024, 1, 1, 12, 0, 0, tzinfo=timezone.utc)
        result = _to_rfc3339(dt)
        assert "+00:00" in result


class MockPost:
    def __init__(self, **kwargs):
        for k, v in kwargs.items():
            setattr(self, k, v)


def _mock_user():
    return MockPost(
        screen_name="作者",
        avatar_hd=None,
        avatar_large=None,
        profile_image_url=None,
        domain=None,
        following=False,
        follow_me=False,
    )


class TestPicUrlsToList:
    def test_with_urls(self):
        from weiback.collector import _pic_urls_to_list
        post = MockPost(pic_urls=["url1", "url2"])
        result = _pic_urls_to_list(post)
        assert result == ["url1", "url2"]

    def test_no_pic_urls_attr(self):
        from weiback.collector import _pic_urls_to_list
        post = MockPost()
        result = _pic_urls_to_list(post)
        assert result == []

    def test_empty_pic_urls(self):
        from weiback.collector import _pic_urls_to_list
        post = MockPost(pic_urls=[])
        result = _pic_urls_to_list(post)
        assert result == []


class TestPostToDict:
    def test_basic_fields(self):
        from weiback.collector import _post_to_dict
        post = MockPost(
            id="987654",
            text="测试微博内容",
            created_at=datetime(2024, 6, 1, 10, 0, 0, tzinfo=timezone(timedelta(hours=8))),
            attitudes_count=10,
            comments_count=5,
            reposts_count=3,
            mblogid="abc123",
            region_name="广东",
            source="微博 weibo.com",
        )
        d = _post_to_dict(post, uid=12345)
        assert d["id"] == 987654
        assert d["uid"] == 12345
        assert d["text"] == "测试微博内容"
        assert d["attitudes_count"] == 10
        assert d["comments_count"] == 5
        assert d["reposts_count"] == 3
        assert d["mblogid"] == "abc123"
        assert d["region_name"] == "广东"
        assert d["source"] == "微博 weibo.com"
        assert "+08:00" in d["created_at"]

    def test_retweeted_id(self):
        from weiback.collector import _post_to_dict
        retweeted = MockPost(id="111222")
        post = MockPost(id="1", text="转发", created_at=None, retweeted_status=retweeted)
        d = _post_to_dict(post, uid=1)
        assert d["retweeted_id"] == 111222

    def test_retweeted_id_none(self):
        from weiback.collector import _post_to_dict
        post = MockPost(id="1", text="原创", created_at=None, retweeted_status=None)
        d = _post_to_dict(post, uid=1)
        assert d["retweeted_id"] is None

    def test_pic_num(self):
        from weiback.collector import _post_to_dict
        post = MockPost(id="1", text="带图", created_at=None,
                        pic_urls=["a.jpg", "b.jpg", "c.jpg"])
        d = _post_to_dict(post, uid=1)
        assert d["pic_num"] == 3

    def test_zero_counts_default(self):
        from weiback.collector import _post_to_dict
        post = MockPost(id="1", text="无数据", created_at=None)
        d = _post_to_dict(post, uid=1)
        assert d["attitudes_count"] == 0
        assert d["comments_count"] == 0
        assert d["reposts_count"] == 0

    def test_complete_fields_are_serialized(self):
        import json
        from weiback.collector import _post_to_dict

        post = MockPost(
            id="1",
            bid="bid-1",
            user_id="22",
            text="完整内容",
            created_at=None,
            pic_urls=["https://example.com/a.jpg"],
            video_url="https://example.com/a.mp4",
            location="北京",
            topic_ids=["话题A"],
            at_users=["用户B"],
            is_long_text=True,
            raw_data={"source": "crawl4weibo", "nested": {"ok": True}},
        )

        result = _post_to_dict(post, uid=22)

        assert result["bid"] == "bid-1"
        assert result["video_url"] == "https://example.com/a.mp4"
        assert result["location"] == "北京"
        assert json.loads(result["topic_ids"]) == ["话题A"]
        assert json.loads(result["at_users"]) == ["用户B"]
        assert json.loads(result["raw_data"])["nested"]["ok"] is True
        assert result["content_status"] == "complete"


class TestCommentToDict:
    def test_basic_fields(self):
        from weiback.collector import _comment_to_dict
        comment = MockPost(
            id="c_001",
            user_id="999",
            user_screen_name="评论者",
            text="说得好",
            created_at=datetime(2024, 1, 1, 12, 0, 0),
            like_counts=3,
            reply_id=None,
        )
        d = _comment_to_dict(comment, post_id=1001)
        assert d["id"] == "c_001"
        assert d["post_id"] == 1001
        assert d["user_id"] == "999"
        assert d["text"] == "说得好"
        assert d["like_count"] == 3

    def test_with_reply_id(self):
        from weiback.collector import _comment_to_dict
        comment = MockPost(id="c_reply", text="回复", user_screen_name="A",
                           reply_id="c_001")
        d = _comment_to_dict(comment, post_id=1)
        assert d["reply_id"] == "c_001"

    def test_fallback_id(self):
        from weiback.collector import _comment_to_dict
        comment = MockPost(id=None, text="无ID")
        d = _comment_to_dict(comment, post_id=1)
        assert d["id"].startswith("1_")

    def test_missing_attrs(self):
        from weiback.collector import _comment_to_dict
        comment = MockPost(id="c1")
        d = _comment_to_dict(comment, post_id=1)
        assert d["user_id"] is None
        assert d["text"] is None
        assert d["like_count"] == 0

    def test_complete_comment_fields_and_media(self):
        import json
        from weiback.collector import _comment_to_dict

        comment = MockPost(
            id="child-1",
            user_id="9",
            user_screen_name="回复者",
            user_avatar_url="https://example.com/avatar.jpg",
            user_verified=True,
            text="二级回复",
            created_at=None,
            source="Android",
            like_counts=2,
            liked=True,
            reply_id="other-child",
            reply_text="被回复内容",
            pic_url="https://example.com/comment.jpg",
            raw_data={"id": "child-1"},
        )

        result = _comment_to_dict(
            comment,
            post_id=100,
            root_id="root-1",
            parent_id="other-child",
            depth=1,
        )

        assert result["root_id"] == "root-1"
        assert result["parent_id"] == "other-child"
        assert result["depth"] == 1
        assert result["user_avatar_url"].endswith("avatar.jpg")
        assert result["pic_url"].endswith("comment.jpg")
        assert json.loads(result["raw_data"])["id"] == "child-1"


class TestSyncUser:
    def test_refreshes_existing_post_and_saves_retweet_media(self, conn):
        from weiback.collector import sync_user
        from weiback.writer import save_posts

        save_posts(conn, [{"id": 100, "uid": 7, "text": "旧正文", "created_at": ""}])
        retweet = MockPost(
            id="90",
            user_id="8",
            text="转发原文",
            created_at=None,
            pic_urls=["https://example.com/retweet.jpg"],
            video_url="https://example.com/retweet.mp4",
            retweeted_status=None,
        )
        post = MockPost(
            id="100",
            user_id="7",
            text="更新正文",
            created_at=None,
            pic_urls=["https://example.com/post.jpg"],
            video_url="https://example.com/post.mp4",
            retweeted_status=retweet,
        )
        user = MockPost(
            screen_name="作者",
            avatar_hd="https://example.com/avatar.jpg",
            avatar_large=None,
            profile_image_url=None,
            domain=None,
            following=False,
            follow_me=False,
        )

        class Client:
            def get_user_by_uid(self, uid):
                return user

            def get_user_posts(self, **kwargs):
                return [post] if kwargs["page"] == 1 else []

        count = sync_user(conn, Client(), "7", max_pages=2)

        assert count == 0
        outer = conn.execute("SELECT text, retweeted_id, video_url FROM posts WHERE id=100").fetchone()
        assert dict(outer) == {
            "text": "更新正文",
            "retweeted_id": 90,
            "video_url": "https://example.com/post.mp4",
        }
        assert conn.execute("SELECT text FROM posts WHERE id=90").fetchone()[0] == "转发原文"
        media = conn.execute(
            "SELECT owner_id, media_type, url FROM media ORDER BY owner_id, media_type"
        ).fetchall()
        assert {(r["owner_id"], r["media_type"], r["url"]) for r in media} == {
            ("90", "image", "https://example.com/retweet.jpg"),
            ("90", "video", "https://example.com/retweet.mp4"),
            ("100", "image", "https://example.com/post.jpg"),
            ("100", "video", "https://example.com/post.mp4"),
            ("7", "avatar", "https://example.com/avatar.jpg"),
        }


class TestMatchContentType:
    def test_all_matches_everything(self):
        from weiback.collector import _match_content_type
        assert _match_content_type(MockPost(id="1"), "all") is True
        assert _match_content_type(MockPost(id="1", text="x"), "all") is True

    def test_original_excludes_retweets(self):
        from weiback.collector import _match_content_type
        rt = MockPost(id="9")
        post = MockPost(id="1", retweeted_status=rt)
        assert _match_content_type(post, "original") is False

    def test_original_matches_plain_post(self):
        from weiback.collector import _match_content_type
        post = MockPost(id="1", retweeted_status=None)
        assert _match_content_type(post, "original") is True

    def test_original_ignores_retweet_with_own_picture(self):
        from weiback.collector import _match_content_type
        rt = MockPost(id="9")
        post = MockPost(id="1", retweeted_status=rt, pic_urls=["a.jpg"])
        assert _match_content_type(post, "original") is False

    def test_picture_requires_pic_urls(self):
        from weiback.collector import _match_content_type
        assert _match_content_type(MockPost(id="1", pic_urls=["a.jpg"]), "picture") is True
        assert _match_content_type(MockPost(id="2", pic_urls=[]), "picture") is False
        assert _match_content_type(MockPost(id="3"), "picture") is False

    def test_video_requires_video_url(self):
        from weiback.collector import _match_content_type
        assert _match_content_type(MockPost(id="1", video_url="x.mp4"), "video") is True
        assert _match_content_type(MockPost(id="2"), "video") is False

    def test_article_requires_long_text(self):
        from weiback.collector import _match_content_type
        assert _match_content_type(MockPost(id="1", is_long_text=True), "article") is True
        assert _match_content_type(MockPost(id="2", is_long_text=False), "article") is False
        assert _match_content_type(MockPost(id="3"), "article") is False

    def test_unknown_type_falls_back_to_all(self):
        from weiback.collector import _match_content_type
        assert _match_content_type(MockPost(id="1"), "unknown") is True
        assert _match_content_type(MockPost(id="2"), "") is True


class TestSyncUserContentType:
    def test_filters_posts_by_content_type(self, conn):
        from weiback.collector import sync_user

        picture = MockPost(id="201", user_id="7", text="图片微博", created_at=None,
                           pic_urls=["a.jpg"], video_url=None, is_long_text=False,
                           retweeted_status=None)
        video = MockPost(id="202", user_id="7", text="视频微博", created_at=None,
                         pic_urls=[], video_url="v.mp4", is_long_text=False,
                         retweeted_status=None)
        article = MockPost(id="203", user_id="7", text="长文微博", created_at=None,
                           pic_urls=[], video_url=None, is_long_text=True,
                           retweeted_status=None)

        class Client:
            def get_user_by_uid(self, uid):
                return _mock_user()

            def get_user_posts(self, **kwargs):
                return [picture, video, article] if kwargs["page"] == 1 else []

        count = sync_user(conn, Client(), "7", max_pages=2, content_type="picture")

        assert count == 1
        ids = {r[0] for r in conn.execute("SELECT id FROM posts").fetchall()}
        assert ids == {201}

    def test_original_skips_retweet_and_its_chain(self, conn):
        from weiback.collector import sync_user

        retweeted = MockPost(id="301", user_id="8", text="原文", created_at=None,
                             retweeted_status=None)
        repost = MockPost(id="300", user_id="7", text="转发", created_at=None,
                          retweeted_status=retweeted)
        original = MockPost(id="302", user_id="7", text="原创", created_at=None,
                            retweeted_status=None)

        class Client:
            def get_user_by_uid(self, uid):
                return _mock_user()

            def get_user_posts(self, **kwargs):
                return [repost, original] if kwargs["page"] == 1 else []

        count = sync_user(conn, Client(), "7", max_pages=2, content_type="original")

        assert count == 1
        ids = {r[0] for r in conn.execute("SELECT id FROM posts").fetchall()}
        assert ids == {302}

    def test_comment_limit_passed_to_client(self, conn):
        from weiback.collector import sync_user

        post = MockPost(id="401", user_id="7", text="正文", created_at=None,
                        retweeted_status=None)
        seen = {}

        class Client:
            def get_user_by_uid(self, uid):
                return _mock_user()

            def get_user_posts(self, **kwargs):
                seen.update(kwargs)
                return [post] if kwargs["page"] == 1 else []

        sync_user(conn, Client(), "7", max_pages=2, with_comments=True, comment_limit=30)

        assert seen["with_comments"] is True
        assert seen["comment_limit"] == 30


class TestBackfillPostComments:
    def test_backfills_single_post_comments(self, conn):
        from weiback.collector import backfill_post_comments
        from weiback.writer import save_posts

        save_posts(conn, [{"id": 410, "uid": 7, "text": "正文", "created_at": ""}])

        class Client:
            def _request(self, url, params, use_proxy=True, headers=None):
                return {
                    "ok": 1,
                    "data": [{
                        "id": "r1",
                        "text": "一级评论",
                        "created_at": "Fri Jul 31 12:00:00 +0800 2026",
                        "user": {"id": "9", "screen_name": "评论者"},
                    }],
                    "max_id": 0,
                }

        assert backfill_post_comments(conn, Client(), 410) is True
        rows = conn.execute("SELECT id, depth FROM comments").fetchall()
        assert {r["id"]: r["depth"] for r in rows} == {"r1": 0}

    def test_backfill_missing_post_returns_false(self, conn):
        from weiback.collector import backfill_post_comments

        class Client:
            pass

        assert backfill_post_comments(conn, Client(), 9999) is False


class TestParseChildCommentReplyCompat:
    def test_reply_comment_id_fallback(self):
        from weiback.weibo_adapter import parse_child_comment

        parsed = parse_child_comment({"id": "x", "text": "回复", "reply_comment_id": "p1"})
        assert parsed.reply_id == "p1"

    def test_reply_id_takes_precedence(self):
        from weiback.weibo_adapter import parse_child_comment

        parsed = parse_child_comment(
            {"id": "x", "text": "回复", "reply_id": "r1", "reply_comment_id": "p1"}
        )
        assert parsed.reply_id == "r1"


class TestSyncUserLastIdBreakpoint:
    def test_stops_when_page_only_has_old_posts(self, conn):
        from weiback.collector import sync_user
        from weiback.writer import save_posts

        save_posts(conn, [{"id": 1000, "uid": 7, "text": "旧帖", "created_at": ""}])
        old = MockPost(id="900", user_id="7", text="旧", created_at=None,
                       retweeted_status=None)
        seen = []

        class Client:
            def get_user_by_uid(self, uid):
                return _mock_user()

            def get_user_posts(self, **kwargs):
                seen.append(kwargs["page"])
                return [old] if kwargs["page"] == 1 else []

        count = sync_user(conn, Client(), "7")

        assert count == 0
        assert seen == [1]
        saved = {r[0] for r in conn.execute("SELECT id FROM posts").fetchall()}
        assert saved == {1000}

    def test_still_fetches_newer_posts_then_stops(self, conn):
        from weiback.collector import sync_user
        from weiback.writer import save_posts

        save_posts(conn, [{"id": 1000, "uid": 7, "text": "旧帖", "created_at": ""}])
        newer = MockPost(id="1005", user_id="7", text="新帖", created_at=None,
                         retweeted_status=None)
        older = MockPost(id="999", user_id="7", text="旧", created_at=None,
                         retweeted_status=None)
        seen = []

        class Client:
            def get_user_by_uid(self, uid):
                return _mock_user()

            def get_user_posts(self, **kwargs):
                seen.append(kwargs["page"])
                if kwargs["page"] == 1:
                    return [newer, older]
                return []

        count = sync_user(conn, Client(), "7")

        assert count == 1
        assert seen == [1, 2]
        saved = {r[0] for r in conn.execute("SELECT id FROM posts").fetchall()}
        assert saved == {1000, 1005}

    def test_manual_pages_ignores_last_id(self, conn):
        from weiback.collector import sync_user
        from weiback.writer import save_posts

        save_posts(conn, [{"id": 1000, "uid": 7, "text": "旧帖", "created_at": ""}])
        older = MockPost(id="900", user_id="7", text="旧", created_at=None,
                         retweeted_status=None)
        seen = []

        class Client:
            def get_user_by_uid(self, uid):
                return _mock_user()

            def get_user_posts(self, **kwargs):
                seen.append(kwargs["page"])
                return [older] if kwargs["page"] == 1 else []

        count = sync_user(conn, Client(), "7", max_pages=2)

        assert seen == [1, 2]
        assert count == 1


class TestSyncUserFilteredEmptyContinues:
    def test_continues_when_page_fully_filtered(self, conn):
        from weiback.collector import sync_user

        text_post = MockPost(id="3001", user_id="7", text="纯文字", created_at=None,
                             retweeted_status=None, pic_urls=[])
        pic_post = MockPost(id="3002", user_id="7", text="有图", created_at=None,
                            retweeted_status=None, pic_urls=["a.jpg"])
        seen = []

        class Client:
            def get_user_by_uid(self, uid):
                return MockPost(screen_name="作者", avatar_hd=None, avatar_large=None,
                                profile_image_url=None, domain=None,
                                following=False, follow_me=False)

            def get_user_posts(self, **kwargs):
                seen.append(kwargs["page"])
                if kwargs["page"] == 1:
                    return [text_post]
                if kwargs["page"] == 2:
                    return [pic_post]
                return []

        count = sync_user(conn, Client(), "7", max_pages=5, content_type="picture")

        assert seen == [1, 2, 3]
        assert count == 1
        saved = {r[0] for r in conn.execute("SELECT id FROM posts").fetchall()}
        assert saved == {3002}


class TestBackfillBuildComments:
    @staticmethod
    def _raw_comment(comment_id):
        return {
            "id": comment_id,
            "text": f"评论{comment_id}",
            "created_at": "Fri Jul 31 12:00:00 +0800 2026",
            "user": {"id": "9", "screen_name": "评论者"},
        }

    def test_backfill_uses_buildcomments_and_follows_max_id(self, conn):
        from weiback.collector import backfill_post_comments
        from weiback.writer import save_posts

        save_posts(conn, [{"id": 500, "uid": 7, "text": "正文", "created_at": ""}])
        responses = [
            {"ok": 1, "data": [self._raw_comment("c1"), self._raw_comment("c2")],
             "max_id": "abc123"},
            {"ok": 1, "data": [self._raw_comment("c3")], "max_id": 0},
        ]
        seen = []

        class Client:
            def _request(self, url, params, use_proxy=True, headers=None):
                seen.append((url, params, headers))
                return responses.pop(0)

        assert backfill_post_comments(conn, Client(), 500) is True

        assert all("buildComments" in u for u, _, _ in seen)
        assert seen[0][1]["is_reload"] == "1"
        assert "max_id" not in seen[0][1]
        assert seen[1][1]["is_reload"] == "0"
        assert seen[1][1]["max_id"] == "abc123"
        assert seen[0][2] == {"Referer": "https://weibo.com/"}
        rows = conn.execute("SELECT id, depth FROM comments ORDER BY id").fetchall()
        assert {r["id"]: r["depth"] for r in rows} == {"c1": 0, "c2": 0, "c3": 0}
        progress = conn.execute(
            "SELECT status, cursor, fetched_count FROM comments_sync_progress "
            "WHERE post_id=500"
        ).fetchone()
        assert dict(progress) == {
            "status": "complete",
            "cursor": "0",
            "fetched_count": 3,
        }

    def test_business_error_marks_failed(self, conn):
        from weiback.collector import backfill_post_comments
        from weiback.writer import save_posts

        save_posts(conn, [{"id": 501, "uid": 7, "text": "正文", "created_at": ""}])

        class Client:
            def _request(self, url, params, use_proxy=True, headers=None):
                return {"ok": -100, "msg": "登录已过期"}

        assert backfill_post_comments(conn, Client(), 501) is False
        progress = conn.execute(
            "SELECT status, error_message FROM comments_sync_progress WHERE post_id=501"
        ).fetchone()
        assert progress["status"] == "failed"

    def test_empty_page_completes(self, conn):
        from weiback.collector import backfill_post_comments
        from weiback.writer import save_posts

        save_posts(conn, [{"id": 502, "uid": 7, "text": "正文", "created_at": ""}])

        class Client:
            def _request(self, url, params, use_proxy=True, headers=None):
                return {"ok": 1, "data": [], "max_id": 0}

        assert backfill_post_comments(conn, Client(), 502) is False
        progress = conn.execute(
            "SELECT status FROM comments_sync_progress WHERE post_id=502"
        ).fetchone()
        assert progress["status"] == "complete"


class TestParseChildCommentCreatedAt:
    def test_parses_weibo_time_string(self):
        from weiback.collector import _to_rfc3339
        from weiback.weibo_adapter import parse_child_comment

        raw = {
            "id": "c200000000000000001",
            "text": "回复",
            "created_at": "Sat Aug 01 03:10:00 +0800 2026",
            "user": {"id": "2000000011", "screen_name": "回复用户A"},
        }
        parsed = parse_child_comment(raw)
        assert parsed.created_at is not None
        assert "+08:00" in _to_rfc3339(parsed.created_at)

    def test_missing_created_at_is_none(self):
        from weiback.weibo_adapter import parse_child_comment

        parsed = parse_child_comment({"id": "c1", "text": "无时间"})
        assert parsed.created_at is None


class TestCommentToDictStringCreatedAt:
    def test_string_created_at_does_not_crash(self):
        from weiback.collector import _comment_to_dict

        comment = MockPost(
            id="c_001",
            text="说得好",
            created_at="Fri Jul 31 12:00:00 +0800 2026",
        )
        d = _comment_to_dict(comment, post_id=1001)
        assert d["created_at"] is not None
        assert "+08:00" in d["created_at"]

    def test_iso_string_created_at_preserved(self):
        from weiback.collector import _comment_to_dict

        comment = MockPost(id="c_002", text="ISO", created_at="2026-07-31T12:00:00+08:00")
        d = _comment_to_dict(comment, post_id=1001)
        assert d["created_at"].startswith("2026-07-31T12:00:00")


class TestBackfillBuildsTree:
    def test_reply_comments_get_tree_structure(self, conn):
        from weiback.collector import backfill_comments
        from weiback.writer import save_posts

        save_posts(conn, [{"id": 200, "uid": 7, "text": "正文", "created_at": ""}])

        def _raw(comment_id, reply_id=None):
            return {
                "id": comment_id,
                "text": "评论",
                "reply_id": reply_id,
                "created_at": "Fri Jul 31 12:00:00 +0800 2026",
                "user": {"id": "9", "screen_name": "评论者"},
            }

        class Client:
            def _request(self, url, params, use_proxy=True, headers=None):
                return {
                    "ok": 1,
                    "data": [_raw("r1"), _raw("c1", reply_id="r1")],
                    "max_id": 0,
                }

        backfill_comments(conn, Client(), limit=1, post_delay=(0, 0), max_pages=None)

        rows = conn.execute(
            "SELECT id, root_id, parent_id, depth FROM comments ORDER BY id"
        ).fetchall()
        assert [dict(row) for row in rows] == [
            {"id": "c1", "root_id": "r1", "parent_id": "r1", "depth": 1},
            {"id": "r1", "root_id": "r1", "parent_id": None, "depth": 0},
        ]

    def test_tree_rows_resolves_forward_and_nested_reply_chains(self, conn):
        from weiback.collector import _comment_tree_rows

        root = MockPost(id="r1", text="根评论", reply_id=None)
        child = MockPost(id="c1", text="回复根", reply_id="r1")
        grandchild = MockPost(id="g1", text="回复回复", reply_id="c1")
        rows = _comment_tree_rows([root, child, grandchild], 200)

        by_id = {r["id"]: r for r in rows}
        assert by_id["r1"]["root_id"] == "r1"
        assert by_id["r1"]["parent_id"] is None
        assert by_id["r1"]["depth"] == 0
        assert by_id["c1"]["root_id"] == "r1"
        assert by_id["c1"]["parent_id"] == "r1"
        assert by_id["c1"]["depth"] == 1
        assert by_id["g1"]["root_id"] == "r1"
        assert by_id["g1"]["parent_id"] == "c1"
        assert by_id["g1"]["depth"] == 2

    def test_tree_rows_child_before_parent_still_resolves(self, conn):
        from weiback.collector import _comment_tree_rows

        child = MockPost(id="c1", text="先出现的回复", reply_id="r1")
        root = MockPost(id="r1", text="后出现的根", reply_id=None)
        rows = _comment_tree_rows([child, root], 200)

        by_id = {r["id"]: r for r in rows}
        assert by_id["c1"]["parent_id"] == "r1"
        assert by_id["c1"]["depth"] == 1
        assert by_id["c1"]["root_id"] == "r1"


class TestBackfillAutoFetchReplies:
    def test_auto_fetches_replies_for_roots_with_children(self, conn):
        from weiback.collector import backfill_comments
        from weiback.writer import save_posts

        save_posts(conn, [{"id": 300, "uid": 7, "text": "正文", "created_at": ""}])

        class Client:
            def _request(self, url, params, use_proxy=True, headers=None):
                if "hotFlowChild" in url:
                    assert params["cid"] == "r1"
                    return {
                        "data": [{
                            "id": "re-1",
                            "text": "自动抓的二级回复",
                            "created_at": "Sat Aug 01 03:10:00 +0800 2026",
                            "user": {"id": "9", "screen_name": "回复者"},
                        }],
                        "max_id": 0,
                        "max_id_type": 0,
                    }
                return {
                    "ok": 1,
                    "data": [{
                        "id": "r1",
                        "text": "一级评论",
                        "total_number": 3,
                        "created_at": "Sat Aug 01 03:00:00 +0800 2026",
                        "user": {"id": "9", "screen_name": "评论者"},
                    }],
                    "max_id": 0,
                }

        backfill_comments(conn, Client(), limit=1, post_delay=(0, 0), max_pages=None)

        saved = conn.execute(
            "SELECT id, root_id, parent_id, depth, created_at FROM comments WHERE id='re-1'"
        ).fetchone()
        assert saved is not None
        assert dict(saved)["depth"] == 1
        assert dict(saved)["root_id"] == "r1"
        assert dict(saved)["created_at"] is not None

    def test_root_without_children_skips_reply_fetch(self, conn):
        from weiback.collector import backfill_comments
        from weiback.writer import save_posts

        save_posts(conn, [{"id": 301, "uid": 7, "text": "正文", "created_at": ""}])

        class Client:
            def _request(self, url, params, use_proxy=True, headers=None):
                if "hotFlowChild" in url:
                    raise AssertionError("不应抓取无回复评论的二级回复")
                return {
                    "ok": 1,
                    "data": [{
                        "id": "r2",
                        "text": "无回复",
                        "created_at": "Sat Aug 01 03:00:00 +0800 2026",
                        "user": {"id": "9", "screen_name": "评论者"},
                    }],
                    "max_id": 0,
                }

        backfill_comments(conn, Client(), limit=1, post_delay=(0, 0), max_pages=None)


class TestFetchCommentReplies:
    def test_follows_cursor_and_saves_comment_tree(self, conn):
        from weiback.collector import fetch_comment_replies

        responses = [
            {
                "data": [self._raw_comment("child-1", reply_id="root-1")],
                "max_id": 12,
                "max_id_type": 0,
            },
            {
                "data": [self._raw_comment("child-2", reply_id="child-1")],
                "max_id": 0,
                "max_id_type": 0,
            },
        ]

        class Client:
            def __init__(self):
                self.params = []

            def _request(self, url, params, use_proxy=True):
                self.params.append(params.copy())
                return responses.pop(0)

        client = Client()
        fetched = fetch_comment_replies(conn, client, 100, "root-1", page_delay=0)

        assert fetched == 2
        assert [p["max_id"] for p in client.params] == [0, 12]
        rows = conn.execute(
            "SELECT id, root_id, parent_id, depth FROM comments ORDER BY id"
        ).fetchall()
        assert [dict(row) for row in rows] == [
            {"id": "child-1", "root_id": "root-1", "parent_id": "root-1", "depth": 1},
            {"id": "child-2", "root_id": "root-1", "parent_id": "child-1", "depth": 2},
        ]
        progress = conn.execute(
            "SELECT status, max_id, fetched_count FROM comment_reply_progress WHERE root_comment_id='root-1'"
        ).fetchone()
        assert dict(progress) == {"status": "complete", "max_id": "0", "fetched_count": 2}

    def test_failure_keeps_next_cursor_for_retry(self, conn):
        from weiback.collector import fetch_comment_replies

        class FailingClient:
            def __init__(self):
                self.calls = 0

            def _request(self, url, params, use_proxy=True):
                self.calls += 1
                if self.calls == 1:
                    return {
                        "data": [TestFetchCommentReplies._raw_comment("child-1")],
                        "max_id": 99,
                        "max_id_type": 1,
                    }
                raise RuntimeError("rate limited")

        assert fetch_comment_replies(
            conn, FailingClient(), 100, "root-1", page_delay=0
        ) == 1
        failed = conn.execute(
            "SELECT status, max_id, max_id_type, fetched_count, error_message "
            "FROM comment_reply_progress WHERE root_comment_id='root-1'"
        ).fetchone()
        assert failed["status"] == "failed"
        assert failed["max_id"] == "99"
        assert failed["max_id_type"] == 1
        assert failed["fetched_count"] == 1
        assert "rate limited" in failed["error_message"]

        seen = []

        class RetryClient:
            def _request(self, url, params, use_proxy=True):
                seen.append(params.copy())
                return {"data": [], "max_id": 0, "max_id_type": 0}

        assert fetch_comment_replies(
            conn, RetryClient(), 100, "root-1", page_delay=0
        ) == 0
        assert seen[0]["max_id"] == 99
        assert seen[0]["max_id_type"] == 1

    def test_reply_to_existing_child_gets_nested_depth(self, conn):
        from weiback.collector import fetch_comment_replies

        responses = [
            {
                "data": [self._raw_comment("child-1", reply_id="root-1")],
                "max_id": 12,
                "max_id_type": 0,
            },
            {
                "data": [self._raw_comment("grand-1", reply_id="child-1")],
                "max_id": 0,
                "max_id_type": 0,
            },
        ]

        class Client:
            def _request(self, url, params, use_proxy=True):
                return responses.pop(0)

        fetch_comment_replies(conn, Client(), 100, "root-1", page_delay=0)
        grand = conn.execute(
            "SELECT root_id, parent_id, depth FROM comments WHERE id='grand-1'"
        ).fetchone()
        assert dict(grand) == {
            "root_id": "root-1",
            "parent_id": "child-1",
            "depth": 2,
        }

    @staticmethod
    def _raw_comment(comment_id, reply_id=None):
        return {
            "id": comment_id,
            "text": "回复内容",
            "created_at": "Fri Jul 31 12:00:00 +0800 2026",
            "source": "Android",
            "like_counts": 3,
            "reply_id": reply_id,
            "user": {
                "id": "9",
                "screen_name": "回复者",
                "profile_image_url": "https://example.com/avatar.jpg",
                "verified": True,
            },
            "pic": {"url": "https://example.com/comment.jpg"},
        }


class TestBackfillComments:
    def test_failure_keeps_next_cursor_and_retry_completes(self, conn):
        from weiback.collector import backfill_comments
        from weiback.writer import save_posts

        save_posts(conn, [{"id": 100, "uid": 7, "text": "正文", "created_at": ""}])

        def _raw(comment_id):
            return {
                "id": comment_id,
                "text": "评论",
                "created_at": "Fri Jul 31 12:00:00 +0800 2026",
                "user": {"id": "9", "screen_name": "评论者"},
            }

        class FailingClient:
            def __init__(self):
                self.cursors = []

            def _request(self, url, params, use_proxy=True, headers=None):
                self.cursors.append(params.get("max_id"))
                if params.get("max_id") in (None, "0"):
                    return {"ok": 1, "data": [_raw("c1")], "max_id": "abc123"}
                raise RuntimeError("rate limited")

        failing = FailingClient()
        assert backfill_comments(
            conn, failing, limit=1, post_delay=(0, 0), max_pages=None
        ) == 1
        assert failing.cursors == [None, "abc123"]
        progress = conn.execute(
            "SELECT status, cursor, fetched_count, error_message "
            "FROM comments_sync_progress WHERE post_id=100"
        ).fetchone()
        assert progress["status"] == "failed"
        assert progress["cursor"] == "abc123"
        assert progress["fetched_count"] == 1
        assert "rate limited" in progress["error_message"]

        class RetryClient:
            def __init__(self):
                self.cursors = []

            def _request(self, url, params, use_proxy=True, headers=None):
                self.cursors.append(params.get("max_id"))
                if params.get("max_id") == "abc123":
                    return {"ok": 1, "data": [_raw("c2")], "max_id": 0}
                return {"ok": 1, "data": [_raw("c1")], "max_id": "abc123"}

        retry = RetryClient()
        assert backfill_comments(
            conn, retry, limit=1, post_delay=(0, 0), max_pages=None
        ) == 1
        assert retry.cursors == ["abc123"]
        progress = conn.execute(
            "SELECT status, cursor, fetched_count, error_message "
            "FROM comments_sync_progress WHERE post_id=100"
        ).fetchone()
        assert dict(progress) == {
            "status": "complete",
            "cursor": "0",
            "fetched_count": 2,
            "error_message": None,
        }

    def test_page_limit_keeps_post_eligible_for_next_run(self, conn):
        from weiback.collector import backfill_comments
        from weiback.writer import save_posts

        save_posts(conn, [{"id": 101, "uid": 7, "text": "正文", "created_at": ""}])

        class Client:
            def __init__(self):
                self.cursors = []

            def _request(self, url, params, use_proxy=True, headers=None):
                self.cursors.append(params.get("max_id"))
                if params.get("max_id") in (None, "0"):
                    return {"ok": 1, "data": [TestBackfillComments._raw("c1")], "max_id": "p1"}
                return {"ok": 1, "data": [TestBackfillComments._raw("c2")], "max_id": 0}

        client = Client()
        backfill_comments(conn, client, limit=1, post_delay=(0, 0), max_pages=1)
        backfill_comments(conn, client, limit=1, post_delay=(0, 0), max_pages=1)

        assert client.cursors == [None, "p1"]
        progress = conn.execute(
            "SELECT status, cursor, fetched_count FROM comments_sync_progress "
            "WHERE post_id=101"
        ).fetchone()
        assert dict(progress) == {
            "status": "complete",
            "cursor": "0",
            "fetched_count": 2,
        }

    @staticmethod
    def _raw(comment_id):
        return {
            "id": comment_id,
            "text": "评论",
            "created_at": "Fri Jul 31 12:00:00 +0800 2026",
            "user": {"id": "9", "screen_name": "评论者"},
        }
