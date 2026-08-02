"""事件抽取器测试：规范化 user/post/comment/media_reference，两种 hotFlowChild 格式。"""

import unittest

from weiback_collector import extract


def raw_user(**overrides):
    user = {
        "id": "1234567890",
        "screen_name": "测试用户",
        "avatar_hd": "https://tva1.sinaimg.cn/hd.jpg",
        "avatar_large": "https://tva1.sinaimg.cn/large.jpg",
        "profile_image_url": "https://tva1.sinaimg.cn/default.jpg",
        "domain": "testuser",
        "following": True,
        "follow_me": False,
    }
    user.update(overrides)
    return user


def raw_post(**overrides):
    post = {
        "id": "4876543210987654321",
        "text": "这是一条<b>测试</b>微博 &amp; 实体",
        "created_at": "Sat Aug 01 03:00:00 +0800 2026",
        "user": raw_user(),
        "attitudes_count": 1234,
        "comments_count": 56,
        "reposts_count": 789,
        "repost_type": 0,
        "pic_ids": ["2309578a1b2c3d4e5f6a7b8c9d0e1f2a3b"],
        "pic_infos": {
            "2309578a1b2c3d4e5f6a7b8c9d0e1f2a3b": {
                "original_pic": {
                    "url": "https://wx1.sinaimg.cn/orj360/original.jpg",
                    "width": 1080,
                    "height": 1920,
                }
            }
        },
        "pic_num": 1,
        "mblogid": "ABCDef123",
        "bid": "ABCDef123",
        "isLongText": True,
        "region_name": "北京",
        "source": "微博 weibo.com",
        "topic_ids": ["101", "102"],
        "at_users": ["user_b"],
        "video_url": None,
        "status": 200,
    }
    post.update(overrides)
    return post


class ExtractUserTest(unittest.TestCase):
    def test_full_user(self):
        dto = extract.extract_user(raw_user())
        self.assertEqual(dto["id"], "1234567890")
        self.assertEqual(dto["screen_name"], "测试用户")
        self.assertEqual(dto["avatar_hd"], "https://tva1.sinaimg.cn/hd.jpg")
        self.assertTrue(dto["following"])
        self.assertFalse(dto["follow_me"])

    def test_avatar_fallback_to_avatar_hd(self):
        user = raw_user(avatar_hd=None, avatar_large="https://x/la.jpg")
        dto = extract.extract_user(user)
        self.assertEqual(dto["avatar_hd"], "https://x/la.jpg")

    def test_non_dict_input(self):
        dto = extract.extract_user(None)
        self.assertIsNone(dto["id"])

    def test_user_avatar_reference_uses_one_best_url(self):
        dto = extract.extract_user(raw_user())
        ref = extract.media_reference_from_user(dto)
        self.assertIsNotNone(ref)
        assert ref is not None
        self.assertEqual(ref["owner_type"], "user")
        self.assertEqual(ref["owner_id"], "1234567890")
        self.assertEqual(ref["media_type"], "avatar")
        self.assertEqual(ref["url"], "https://tva1.sinaimg.cn/hd.jpg")

    def test_user_avatar_reference_skips_sensitive_url(self):
        dto = extract.extract_user(raw_user(
            avatar_hd="https://x/hd.jpg?token=secret",
            avatar_large="https://x/large.jpg",
        ))
        ref = extract.media_reference_from_user(dto)
        self.assertIsNotNone(ref)
        assert ref is not None
        self.assertEqual(ref["url"], "https://x/large.jpg")

    def test_user_avatar_reference_skips_fuzzy_credentials_and_fragment(self):
        for url in (
            "https://x/avatar.jpg?access-token=secret",
            "https://x/avatar.jpg?session_id=secret",
            "https://x/avatar.jpg?x-signature=secret",
            "https://x/avatar.jpg#credential",
            "https://x/avatar.jpg?token=",
        ):
            with self.subTest(url=url):
                dto = extract.extract_user(raw_user(
                    avatar_hd=url,
                    avatar_large="https://x/large.jpg",
                ))
                ref = extract.media_reference_from_user(dto)
                self.assertIsNotNone(ref)
                assert ref is not None
                self.assertEqual(ref["url"], "https://x/large.jpg")

    def test_user_avatar_reference_skips_http_and_uses_https_fallback(self):
        dto = extract.extract_user(raw_user(
            avatar_hd="http://x/hd.jpg",
            avatar_large="https://x/large.jpg",
        ))
        ref = extract.media_reference_from_user(dto)
        self.assertIsNotNone(ref)
        assert ref is not None
        self.assertEqual(ref["url"], "https://x/large.jpg")


class ExtractPostTest(unittest.TestCase):
    def test_full_post(self):
        dto = extract.extract_post(raw_post(), "1234567890")
        self.assertEqual(dto["id"], "4876543210987654321")
        self.assertEqual(dto["uid"], 1234567890)
        self.assertEqual(dto["text"], "这是一条测试微博 & 实体")
        self.assertEqual(dto["created_at"], "2026-07-31T19:00:00.000Z")
        self.assertEqual(dto["attitudes_count"], 1234)
        self.assertEqual(dto["comments_count"], 56)
        self.assertEqual(dto["reposts_count"], 789)
        self.assertEqual(dto["mblogid"], "ABCDef123")
        self.assertTrue(dto["is_long_text"])
        self.assertEqual(dto["topic_ids"], ["101", "102"])
        self.assertEqual(dto["at_users"], ["user_b"])
        self.assertEqual(dto["content_status"], "complete")
        self.assertEqual(dto["pic_ids"], ["2309578a1b2c3d4e5f6a7b8c9d0e1f2a3b"])

    def test_uid_fallback(self):
        dto = extract.extract_post(raw_post(user=None), None)
        self.assertIsNone(dto["uid"])

    def test_retweeted_id(self):
        post = raw_post(retweeted_status={"id": "111222"})
        dto = extract.extract_post(post, "1234567890")
        self.assertEqual(dto["retweeted_id"], "111222")

    def test_retweeted_id_none(self):
        dto = extract.extract_post(raw_post(), "1234567890")
        self.assertIsNone(dto["retweeted_id"])

    def test_content_status_partial_preserved(self):
        post = raw_post(deleted=True, content_status="partial", fetch_error="EOF")
        dto = extract.extract_post(post, "1234567890")
        self.assertEqual(dto["content_status"], "partial")
        self.assertTrue(dto["deleted"])
        self.assertEqual(dto["fetch_error"], "EOF")

    def test_invalid_content_status_falls_back(self):
        post = raw_post(content_status="bogus")
        dto = extract.extract_post(post, "1234567890")
        self.assertEqual(dto["content_status"], "complete")

    def test_created_at_iso_passthrough(self):
        post = raw_post(created_at="2026-08-01T03:00:00+08:00")
        dto = extract.extract_post(post, "1234567890")
        self.assertEqual(dto["created_at"], "2026-08-01T03:00:00+08:00")

    def test_pic_num_defaults_to_len(self):
        post = raw_post(pic_num=None, pic_ids=["a", "b"])
        dto = extract.extract_post(post, "1234567890")
        self.assertEqual(dto["pic_num"], 2)

    def test_raw_data_redacted_no_secrets(self):
        post = raw_post(extra_secret="token123")
        dto = extract.extract_post(post, "1234567890")
        raw = dto["raw_data"]
        self.assertIn("status", raw)
        self.assertNotIn("extra_secret", raw)


class ExtractCommentTest(unittest.TestCase):
    def _raw_comment(self, **overrides):
        comment = {
            "id": "c100000000000000001",
            "text": "一楼评论<b>正文</b>",
            "created_at": "Sat Aug 01 03:00:00 +0800 2026",
            "user": {
                "id": "2000000001",
                "screen_name": "一楼用户",
                "avatar_hd": "https://tva1.sinaimg.cn/avatar1.jpg",
                "verified": False,
            },
            "like_counts": 12,
            "source": "微博 weibo.com",
            "total_number": 3,
        }
        comment.update(overrides)
        return comment

    def test_top_level_comment(self):
        dto = extract.extract_comment(self._raw_comment(), "5550000000000000100")
        self.assertEqual(dto["id"], "c100000000000000001")
        self.assertEqual(dto["post_id"], "5550000000000000100")
        self.assertEqual(dto["user_id"], "2000000001")
        self.assertEqual(dto["user_screen_name"], "一楼用户")
        self.assertEqual(dto["text"], "一楼评论正文")
        self.assertEqual(dto["like_count"], 12)
        self.assertEqual(dto["depth"], 0)
        self.assertIsNone(dto["parent_id"])
        self.assertEqual(dto["root_id"], "c100000000000000001")
        self.assertEqual(dto["child_count"], 3)

    def test_reply_comment(self):
        dto = extract.extract_comment(
            self._raw_comment(
                reply_id="c100000000000000001",
                reply_text="一楼评论正文",
            ),
            "5550000000000000100",
            root_id="c100000000000000001",
            parent_id="c100000000000000001",
            depth=1,
        )
        self.assertEqual(dto["depth"], 1)
        self.assertEqual(dto["parent_id"], "c100000000000000001")
        self.assertEqual(dto["reply_id"], "c100000000000000001")
        self.assertEqual(dto["reply_text"], "一楼评论正文")

    def test_pic_url_extracted(self):
        comment = self._raw_comment(
            pic={"large": {"url": "https://wx1.sinaimg.cn/comment_pic.jpg"}}
        )
        dto = extract.extract_comment(comment, "5550000000000000100")
        self.assertEqual(dto["pic_url"], "https://wx1.sinaimg.cn/comment_pic.jpg")

    def test_non_dict_input(self):
        dto = extract.extract_comment(None, "5550000000000000100")
        self.assertIsNotNone(dto["id"])


class UnpackHotFlowChildTest(unittest.TestCase):
    def _comment(self, cid):
        return {
            "id": cid,
            "text": f"评论{cid}",
            "user": {"id": f"user{cid}", "screen_name": f"用户{cid}"},
        }

    def test_envelope_a_data_is_list(self):
        response = {
            "ok": 1,
            "data": [self._comment("1"), self._comment("2")],
            "max_id": "100",
            "max_id_type": 0,
        }
        items, max_id, max_id_type = extract.unpack_child_comment_page(response)
        self.assertEqual(len(items), 2)
        self.assertEqual(max_id, "100")
        self.assertEqual(max_id_type, 0)

    def test_envelope_b_data_has_comments_list(self):
        response = {
            "ok": 1,
            "data": {
                "comments": [self._comment("3")],
                "max_id": "200",
                "max_id_type": 1,
            },
        }
        items, max_id, max_id_type = extract.unpack_child_comment_page(response)
        self.assertEqual(len(items), 1)
        self.assertEqual(max_id, "200")
        self.assertEqual(max_id_type, 1)

    def test_envelope_b_data_has_nested_data_list(self):
        response = {
            "ok": 1,
            "data": {"data": [self._comment("4")], "max_id": "300", "max_id_type": 0},
        }
        items, max_id, max_id_type = extract.unpack_child_comment_page(response)
        self.assertEqual(len(items), 1)
        self.assertEqual(max_id, "300")

    def test_max_id_missing_defaults_zero(self):
        response = {"ok": 1, "data": [self._comment("5")]}
        items, max_id, max_id_type = extract.unpack_child_comment_page(response)
        self.assertEqual(max_id, "0")
        self.assertEqual(max_id_type, 0)

    def test_bad_shape_returns_empty(self):
        items, max_id, max_id_type = extract.unpack_child_comment_page({"data": 42})
        self.assertEqual(items, [])
        self.assertEqual(max_id, "0")


class MediaReferenceTest(unittest.TestCase):
    def test_post_media_references(self):
        post = raw_post()
        dto = extract.extract_post(post, "1234567890")
        assert dto is not None
        refs = extract.media_references_from_post(dto, post)
        self.assertEqual(len(refs), 1)
        ref = refs[0]
        self.assertEqual(ref["owner_type"], "post")
        self.assertEqual(ref["owner_id"], "4876543210987654321")
        self.assertEqual(ref["post_id"], "4876543210987654321")
        self.assertEqual(ref["user_id"], "1234567890")
        self.assertEqual(ref["media_type"], "picture")
        self.assertEqual(ref["url"], "https://wx1.sinaimg.cn/orj360/original.jpg")
        self.assertEqual(ref["definition"], "original")

    def test_post_video_reference(self):
        post = raw_post(video_url="https://video.weibo.com/x.mp4")
        dto = extract.extract_post(post, "1234567890")
        assert dto is not None
        refs = extract.media_references_from_post(dto, post)
        video = [r for r in refs if r["media_type"] == "video"]
        self.assertEqual(len(video), 1)
        self.assertEqual(video[0]["url"], "https://video.weibo.com/x.mp4")

    def test_post_media_reference_skips_http_url(self):
        post = raw_post(video_url="http://video.weibo.com/x.mp4")
        dto = extract.extract_post(post, "1234567890")
        refs = extract.media_references_from_post(dto, post)
        self.assertFalse(any(ref["media_type"] == "video" for ref in refs))

    def test_post_picture_reference_selects_one_url_per_picture(self):
        post = raw_post(pic_infos={
            "p1": {
                "original_pic": {"url": "https://x/original.jpg"},
                "large_pic": {"url": "https://x/large.jpg"},
                "bmiddle_pic": {"url": "https://x/bmiddle.jpg"},
            },
            "p2": {
                "large_pic": {"url": "https://x/large-2.jpg"},
                "bmiddle_pic": {"url": "https://x/bmiddle-2.jpg"},
            },
        })
        dto = extract.extract_post(post, "1234567890")
        refs = [r for r in extract.media_references_from_post(dto, post)
                if r["media_type"] == "picture"]
        self.assertEqual([r["url"] for r in refs], [
            "https://x/original.jpg", "https://x/large-2.jpg"
        ])
        self.assertEqual([r["definition"] for r in refs], ["original", "large"])

    def test_comment_media_reference(self):
        comment = {
            "id": "100000000000000001",
            "text": "带图",
            "pic": {"large": {"url": "https://wx1.sinaimg.cn/comment_pic.jpg"}},
        }
        dto = extract.extract_comment(comment, "5550000000000000100")
        ref = extract.media_reference_from_comment(dto)
        self.assertIsNotNone(ref)
        assert ref is not None
        self.assertEqual(ref["owner_type"], "comment")
        self.assertEqual(ref["owner_id"], "100000000000000001")
        self.assertEqual(ref["media_type"], "picture")
        self.assertEqual(ref["url"], "https://wx1.sinaimg.cn/comment_pic.jpg")

    def test_comment_without_pic_no_reference(self):
        comment = {"id": "c2", "text": "无图"}
        dto = extract.extract_comment(comment, "5550000000000000100")
        self.assertIsNone(extract.media_reference_from_comment(dto))

    def test_comment_media_reference_requires_decimal_owner_id(self):
        comment = {"id": "c2", "pic": {"large": {"url": "https://x/p.jpg"}}}
        dto = extract.extract_comment(comment, "5550000000000000100")
        self.assertIsNone(extract.media_reference_from_comment(dto))


if __name__ == "__main__":
    unittest.main()
