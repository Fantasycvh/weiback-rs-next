use std::collections::HashMap;

use chrono::{DateTime, FixedOffset};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{MixMediaInfo, PageInfo, PicInfoItem, TagStruct, UrlStruct, User};

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct Post {
    pub attitudes_count: Option<i64>,
    pub attitudes_status: i64,
    pub created_at: DateTime<FixedOffset>,
    pub comments_count: Option<i64>,
    pub deleted: bool,
    pub edit_count: Option<i64>,
    pub favorited: bool,
    pub geo: Option<Value>,
    pub id: i64,
    pub idstr: String,
    pub mblogid: String,
    pub mix_media_ids: Option<Vec<String>>,
    pub mix_media_info: Option<MixMediaInfo>,
    pub page_info: Option<PageInfo>,
    pub pic_ids: Option<Vec<String>>,
    pub pic_infos: Option<HashMap<String, PicInfoItem>>,
    pub pic_num: Option<i64>,
    pub region_name: Option<String>,
    pub reposts_count: Option<i64>,
    pub repost_type: Option<i64>,
    pub retweeted_status: Option<Box<Post>>,
    pub source: Option<String>,
    pub tag_struct: Option<TagStruct>,
    pub text: String,
    pub url_struct: Option<UrlStruct>,
    pub user: Option<User>,
    /// 微博短 ID，采集时补齐。
    #[serde(default)]
    pub bid: Option<String>,
    /// 位置/地标文本。
    #[serde(default)]
    pub location: Option<String>,
    /// 话题 ID 列表。
    #[serde(default)]
    pub topic_ids: Option<Vec<String>>,
    /// @ 提及的用户 ID 列表。
    #[serde(default)]
    pub at_users: Option<Vec<String>>,
    /// 是否为长文。
    #[serde(default)]
    pub is_long_text: Option<bool>,
    /// 长文视频地址。
    #[serde(default)]
    pub video_url: Option<String>,
    /// 上游原始响应体（JSON）。
    #[serde(default)]
    pub raw_data: Option<Value>,
    /// 内容完整性状态：`complete` 或 `partial`。
    #[serde(default)]
    pub content_status: Option<String>,
    /// 上次采集失败的错误描述。
    #[serde(default)]
    pub fetch_error: Option<String>,
    /// 首次采集时间。
    #[serde(default)]
    pub first_fetched_at: Option<DateTime<FixedOffset>>,
    /// 最近刷新时间。
    #[serde(default)]
    pub last_refreshed_at: Option<DateTime<FixedOffset>>,
}

#[cfg(test)]
mod local_tests {
    use std::fs::read_to_string;
    use std::path::Path;

    use serde_json::from_str;

    use super::*;
    use crate::error::Result;

    fn create_reponse_str() -> String {
        read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/favorites.json"))
            .unwrap()
    }

    #[test]
    fn test_deserialize_post() {
        let response = create_reponse_str();
        let favorites = from_str::<crate::api::favorites::FavoritesSucc>(&response).unwrap();

        assert!(!favorites.favorites.is_empty());
    }

    #[test]
    fn test_post_serde_roundtrip() {
        let json_data = create_reponse_str();

        let parsed_favorites: crate::api::favorites::FavoritesSucc =
            serde_json::from_str(&json_data).expect("Failed to parse favorites.json");
        let posts = parsed_favorites
            .favorites
            .into_iter()
            .map(|f| f.status.try_into())
            .collect::<Result<Vec<Post>>>()
            .unwrap();

        for post in posts {
            let value_from_struct =
                serde_json::to_value(&post).expect("Failed to serialize Post to Value");

            let post_roundtrip: Post = serde_json::from_value(value_from_struct)
                .expect("Failed to deserialize Post from roundtrip Value");
            assert_eq!(post, post_roundtrip);
        }
    }
}
