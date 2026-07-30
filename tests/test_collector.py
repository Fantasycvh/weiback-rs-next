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
