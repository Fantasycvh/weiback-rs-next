use serde_json::Value;
use weiback::{
    api::ApiClientImpl,
    core::{TaskHandler, task::PostQuery},
    exporter::ExporterImpl,
    media_downloader::create_downloader,
    models::Post,
    storage::{
        Storage, StorageImpl,
        database::create_db_pool_with_url,
        internal::entities::{
            CommentDto, OwnerMediaDto, get_comments, owner_media_wire, save_comment,
        },
    },
};
use weibosdk_rs::{ApiClient as SdkApiClient, mock::MockClient};

fn post(id: i64, pic_num: i64, video_url: Option<&str>, status: &str, source: &str) -> Post {
    Post {
        id,
        idstr: id.to_string(),
        mblogid: format!("m{id}"),
        text: format!("post-{id}"),
        pic_num: Some(pic_num),
        video_url: video_url.map(str::to_owned),
        source: Some(source.into()),
        content_status: Some(status.into()),
        ..Post::default()
    }
}

fn comment(
    id: i64,
    post_id: i64,
    root_id: Option<i64>,
    parent_id: Option<i64>,
    depth: i64,
) -> CommentDto {
    CommentDto {
        id,
        post_id,
        root_id,
        parent_id,
        user_id: Some(i64::MAX),
        text: format!("comment-{id}"),
        created_at: format!("2026-08-02T00:00:0{id}Z"),
        depth,
        child_count: 0,
        like_count: 0,
        source: Some("web".into()),
        media_json: None,
        raw_data: None,
        content_status: "complete".into(),
        deleted: false,
        first_fetched_at: None,
        last_refreshed_at: None,
    }
}

#[tokio::test]
async fn comments_wire_is_root_scoped_paginated_and_uses_decimal_strings() {
    let pool = create_db_pool_with_url(":memory:").await.unwrap();
    for item in [
        comment(1, 99, Some(1), None, 0),
        comment(2, 99, Some(1), Some(1), 1),
        comment(3, 99, Some(1), Some(2), 2),
        comment(4, 99, None, None, 0),
    ] {
        save_comment(&pool, &item).await.unwrap();
    }

    let roots = get_comments(&pool, 99, None, 0, 1).await.unwrap();
    let root_json = serde_json::to_value(&roots).unwrap();
    assert_eq!(root_json["total_items"], "2");
    assert_eq!(root_json["offset"], 0);
    assert_eq!(root_json["limit"], 1);
    assert_eq!(root_json["items"].as_array().unwrap().len(), 1);
    assert!(root_json.get("comments").is_none());

    let replies = get_comments(&pool, 99, Some(1), 0, 100).await.unwrap();
    assert_eq!(replies.items.len(), 1);
    assert_eq!(replies.items[0].id, "2");
    assert_eq!(
        replies.items[0].user_id.as_deref(),
        Some(i64::MAX.to_string().as_str())
    );
}

#[tokio::test]
async fn post_filters_compose_for_count_and_list_and_detail_is_complete() {
    let pool = create_db_pool_with_url(":memory:").await.unwrap();
    let storage = StorageImpl::new(pool.clone());
    for item in [
        post(8_001, 1, None, "complete", "web"),
        post(8_002, 1, None, "partial", "web"),
        post(
            8_003,
            0,
            Some("https://example.invalid/v.mp4"),
            "complete",
            "ios",
        ),
    ] {
        storage.save_post(&item).await.unwrap();
    }

    let query = PostQuery {
        content_type: Some("picture".into()),
        content_status: Some("complete".into()),
        source: Some("web".into()),
        posts_per_page: 100,
        ..PostQuery::default()
    };
    let page = storage.query_posts(query).await.unwrap();
    assert_eq!(page.total_items, 1);
    assert_eq!(
        page.posts.iter().map(|item| item.id).collect::<Vec<_>>(),
        [8_001]
    );

    let api = ApiClientImpl::new(SdkApiClient::from_session(
        MockClient::new(),
        Default::default(),
    ));
    let (downloader, _worker) = create_downloader(1, reqwest::Client::new());
    let handler = TaskHandler::new(api, storage, ExporterImpl::new(), downloader).unwrap();
    assert!(handler.get_post_detail(404).await.unwrap().is_none());
    let detail = handler.get_post_detail(8_001).await.unwrap().unwrap();
    assert_eq!(detail.post.id, 8_001);
    assert_eq!(detail.post.text, "post-8001");
    assert_eq!(detail.post.content_status.as_deref(), Some("complete"));
    assert!(detail.emoji_map.is_empty());
    assert!(detail.inline_map.is_empty());
}

#[test]
fn owner_media_wire_has_exact_safe_shape_and_large_decimal_ids() {
    let wire = owner_media_wire(
        OwnerMediaDto {
            id: i64::MAX,
            owner_type: "post".into(),
            owner_id: Some(9_007_199_254_740_993),
            media_type: "picture".into(),
            url: "https://example.invalid/a.jpg?access-token=secret#credential".into(),
            local_path: Some("private/a.jpg".into()),
            status: "failed".into(),
            retry_count: 7,
            last_error: Some("private failure".into()),
            created_at: "2026-08-02T00:00:00Z".into(),
            updated_at: Some("2026-08-02T00:00:01Z".into()),
            definition: Some("large".into()),
        },
        std::path::Path::new("missing-root"),
    );
    let Value::Object(json) = serde_json::to_value(wire).unwrap() else {
        panic!("object expected")
    };
    let mut keys = json.keys().map(String::as_str).collect::<Vec<_>>();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "created_at",
            "definition",
            "id",
            "local_available",
            "media_type",
            "owner_id",
            "owner_type",
            "remote_url",
            "retry_count",
            "status",
            "updated_at"
        ]
    );
    assert_eq!(json["id"], i64::MAX.to_string());
    assert_eq!(json["owner_id"], "9007199254740993");
    assert_eq!(json["retry_count"], "7");
    assert_eq!(json["local_available"], false);
    assert_eq!(json["remote_url"], "");
    assert!(!json.contains_key("local_path"));
    assert!(!json.contains_key("last_error"));
}

#[test]
fn post_query_accepts_decimal_string_user_id_without_js_precision_loss() {
    let query: PostQuery = serde_json::from_value(serde_json::json!({
        "user_id": "9007199254740993",
        "is_favorited": false,
        "reverse_order": false,
        "page": 1,
        "posts_per_page": 20
    }))
    .unwrap();
    assert_eq!(query.user_id, Some(9_007_199_254_740_993));
}
