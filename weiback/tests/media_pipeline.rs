use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    path::Path,
    sync::Arc,
    time::Duration,
};

use sqlx::{Row, Sqlite, SqlitePool, migrate::MigrateDatabase};
use weiback::{
    media_pipeline::{
        MediaHostResolver, MediaPipeline, MediaPipelineConfig, MediaWorkerTask, ResolvedMediaSource,
    },
    storage::{
        database::create_db_pool_with_url,
        internal::entities::{
            MediaClaimRequest, MediaDownloadCompletion, MediaReferenceDto, MediaSource,
            claim_next_media, complete_media_download, fail_media_download, get_media_by_url,
            get_owner_media, recover_downloading_media, retry_media,
            save_media_reference_with_definition,
        },
    },
};

fn valid_png() -> Vec<u8> {
    let mut png = Vec::new();
    image::DynamicImage::new_rgba8(1, 1)
        .write_to(
            &mut std::io::Cursor::new(&mut png),
            image::ImageOutputFormat::Png,
        )
        .unwrap();
    png
}

// P3-B exercises the local-only read boundary independently of Tauri.
#[tokio::test]
async fn local_media_blob_reads_downloaded_bytes_with_stored_mime_and_rejects_unsafe_inputs() {
    use weiback::storage::internal::entities::get_media_blob;

    let pool = create_db_pool_with_url(":memory:").await.unwrap();
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(temp.path().join("pictures")).unwrap();
    std::fs::write(temp.path().join("pictures/safe.png"), b"png-bytes").unwrap();
    sqlx::query(
        "INSERT INTO media(id,url,media_type,local_path,status,content_type,content_length,created_at) \
         VALUES(?,'https://example.test/safe.png','picture','pictures/safe.png','downloaded','image/png',9,'now')",
    )
    .bind(i64::MAX)
    .execute(&pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO media_references(media_id,owner_type,owner_id,definition,created_at) \
         VALUES(?,'post',7,'large','now')",
    )
    .bind(i64::MAX)
    .execute(&pool)
    .await
    .unwrap();

    let blob = get_media_blob(&pool, temp.path(), "post", 7, i64::MAX)
        .await
        .unwrap()
        .expect("downloaded local media");
    assert_eq!(blob.content_type, "image/png");
    assert_eq!(blob.bytes, b"png-bytes");

    for status in ["pending", "failed"] {
        sqlx::query("UPDATE media SET status=? WHERE id=?")
            .bind(status)
            .bind(i64::MAX)
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            get_media_blob(&pool, temp.path(), "post", 7, i64::MAX)
                .await
                .unwrap()
                .is_none()
        );
    }

    for unsafe_path in ["../outside.png", "C:/Windows/win.ini"] {
        sqlx::query("UPDATE media SET status='downloaded',local_path=? WHERE id=?")
            .bind(unsafe_path)
            .bind(i64::MAX)
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            get_media_blob(&pool, temp.path(), "post", 7, i64::MAX)
                .await
                .unwrap()
                .is_none()
        );
    }

    std::fs::File::create(temp.path().join("pictures/large.png"))
        .unwrap()
        .set_len(weiback::storage::internal::entities::IMAGE_PREVIEW_MAX_BYTES + 1)
        .unwrap();
    sqlx::query("UPDATE media SET local_path='pictures/large.png' WHERE id=?")
        .bind(i64::MAX)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        get_media_blob(&pool, temp.path(), "post", 7, i64::MAX)
            .await
            .unwrap()
            .is_none()
    );
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn local_media_blob_rejects_symlinked_database_paths() {
    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    #[cfg(windows)]
    use std::os::windows::fs::symlink_file as symlink;
    use weiback::storage::internal::entities::get_media_blob;

    let pool = create_db_pool_with_url(":memory:").await.unwrap();
    let temp = tempfile::tempdir().unwrap();
    let outside = tempfile::NamedTempFile::new().unwrap();
    std::fs::create_dir_all(temp.path().join("pictures")).unwrap();
    symlink(outside.path(), temp.path().join("pictures/link.png")).unwrap();
    let url = "https://example.test/link.png";
    save_media_reference_with_definition(&pool, &media(url, 8, "large"))
        .await
        .unwrap();
    sqlx::query(
        "UPDATE media SET status='downloaded',local_path='pictures/link.png',content_length=1 WHERE url=?",
    )
    .bind(url)
    .execute(&pool)
    .await
    .unwrap();

    assert!(
        get_media_blob(&pool, temp.path(), "post", 8, 1)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn local_media_blob_requires_the_requested_owner_reference() {
    use weiback::storage::internal::entities::get_media_blob;

    let pool = create_db_pool_with_url(":memory:").await.unwrap();
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(temp.path().join("pictures")).unwrap();
    std::fs::write(temp.path().join("pictures/safe.png"), b"png-bytes").unwrap();
    let media = media("https://example.test/shared.png", 11, "large");
    save_media_reference_with_definition(&pool, &media)
        .await
        .unwrap();
    save_media_reference_with_definition(
        &pool,
        &MediaReferenceDto {
            owner_id: Some(22),
            definition: Some("original".into()),
            ..media
        },
    )
    .await
    .unwrap();
    let id: i64 = sqlx::query_scalar("SELECT id FROM media WHERE url=?")
        .bind("https://example.test/shared.png")
        .fetch_one(&pool)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE media SET status='downloaded',local_path='pictures/safe.png',content_type='image/png',content_length=9 WHERE id=?",
    )
    .bind(id)
    .execute(&pool)
    .await
    .unwrap();

    assert!(
        get_media_blob(&pool, temp.path(), "post", 11, id)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        get_media_blob(&pool, temp.path(), "post", 99, id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        get_media_blob(&pool, temp.path(), "user", 11, id)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn local_media_blob_rejects_files_above_the_preview_limit_even_when_media_limit_allows_them()
{
    use weiback::storage::internal::entities::{
        IMAGE_PREVIEW_MAX_BYTES, MEDIA_MAX_BYTES, get_media_blob,
    };

    let pool = create_db_pool_with_url(":memory:").await.unwrap();
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(temp.path().join("pictures")).unwrap();
    let path = temp.path().join("pictures/oversized.jpg");
    std::fs::File::create(&path)
        .unwrap()
        .set_len(IMAGE_PREVIEW_MAX_BYTES + 1)
        .unwrap();
    let media_limit = std::hint::black_box(MEDIA_MAX_BYTES);
    assert!(IMAGE_PREVIEW_MAX_BYTES < media_limit);
    let media = media("https://example.test/oversized.jpg", 12, "large");
    save_media_reference_with_definition(&pool, &media)
        .await
        .unwrap();
    let id: i64 = sqlx::query_scalar("SELECT id FROM media WHERE url=?")
        .bind(&media.url)
        .fetch_one(&pool)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE media SET status='downloaded',local_path='pictures/oversized.jpg',content_length=? WHERE id=?",
    )
    .bind((IMAGE_PREVIEW_MAX_BYTES + 1) as i64)
    .bind(id)
    .execute(&pool)
    .await
    .unwrap();

    assert!(
        get_media_blob(&pool, temp.path(), "post", 12, id)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn remote_preview_falls_back_for_an_authorized_pending_owner_without_writing_state() {
    use weiback::storage::internal::entities::get_media_blob_with_preview;

    let mut server = mockito::Server::new_async().await;
    let png = valid_png();
    let preview = server
        .mock("GET", "/preview.png")
        .with_status(200)
        .with_header("content-type", "image/png")
        .with_body(png.clone())
        .create_async()
        .await;
    let pool = create_db_pool_with_url(":memory:").await.unwrap();
    let temp = tempfile::tempdir().unwrap();
    let url = format!("{}/preview.png", server.url());
    save_media_reference_with_definition(&pool, &media(&url, 17, "large"))
        .await
        .unwrap();
    let id: i64 = sqlx::query_scalar("SELECT id FROM media WHERE url=?")
        .bind(&url)
        .fetch_one(&pool)
        .await
        .unwrap();
    let pipeline = MediaPipeline::new(
        pool.clone(),
        reqwest::Client::new(),
        test_config(temp.path()),
    );

    let blob = get_media_blob_with_preview(&pool, temp.path(), &pipeline, "post", 17, id)
        .await
        .unwrap()
        .expect("authorized remote preview");
    assert_eq!(blob.content_type, "image/png");
    assert_eq!(blob.bytes, png);
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT status FROM media WHERE id=?")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap(),
        "pending"
    );
    preview.assert_async().await;
}

#[tokio::test]
async fn remote_preview_requires_exact_owner_reference_before_fetching() {
    use weiback::storage::internal::entities::get_media_blob_with_preview;

    let mut server = mockito::Server::new_async().await;
    let png = valid_png();
    let preview = server
        .mock("GET", "/preview.png")
        .with_status(200)
        .with_header("content-type", "image/png")
        .with_body(png)
        .expect(0)
        .create_async()
        .await;
    let pool = create_db_pool_with_url(":memory:").await.unwrap();
    let temp = tempfile::tempdir().unwrap();
    let url = format!("{}/preview.png", server.url());
    save_media_reference_with_definition(&pool, &media(&url, 18, "large"))
        .await
        .unwrap();
    let id: i64 = sqlx::query_scalar("SELECT id FROM media WHERE url=?")
        .bind(&url)
        .fetch_one(&pool)
        .await
        .unwrap();
    let pipeline = MediaPipeline::new(
        pool.clone(),
        reqwest::Client::new(),
        test_config(temp.path()),
    );

    assert!(
        get_media_blob_with_preview(&pool, temp.path(), &pipeline, "post", 19, id)
            .await
            .unwrap()
            .is_none()
    );
    preview.assert_async().await;
}

#[tokio::test]
async fn preview_rejects_unsafe_redirect_under_production_policy() {
    let mut server = mockito::Server::new_async().await;
    let redirect = server
        .mock("GET", "/preview")
        .with_status(302)
        .with_header("location", "http://127.0.0.1/private")
        .create_async()
        .await;
    let temp = tempfile::tempdir().unwrap();
    let pipeline = MediaPipeline::new(
        create_db_pool_with_url(":memory:").await.unwrap(),
        reqwest::Client::new(),
        MediaPipelineConfig {
            allow_http: true,
            allow_private_network: true,
            ..test_config(temp.path())
        },
    );

    assert!(
        pipeline
            .fetch_preview(&format!("{}/preview", server.url()), "picture")
            .await
            .is_err()
    );
    redirect.assert_async().await;
}

fn test_config(root: &Path) -> MediaPipelineConfig {
    MediaPipelineConfig {
        media_root: root.into(),
        max_bytes: 1024 * 1024,
        poll_interval: Duration::from_millis(5),
        allow_http: true,
        allow_private_network: true,
        max_redirects: 3,
    }
}

struct FixedResolver(Vec<IpAddr>);

#[async_trait::async_trait]
impl MediaHostResolver for FixedResolver {
    async fn resolve(&self, _host: &str, _port: u16) -> weiback::error::Result<Vec<IpAddr>> {
        Ok(self.0.clone())
    }
}

fn media(url: &str, owner_id: i64, definition: &str) -> MediaReferenceDto {
    MediaReferenceDto {
        owner_type: "post".into(),
        owner_id: Some(owner_id),
        media_type: "picture".into(),
        url: url.into(),
        created_at: "2026-08-02T00:00:00Z".into(),
        definition: Some(definition.into()),
    }
}

#[tokio::test]
async fn migration_backfills_legacy_media_picture_and_video_idempotently() {
    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join("p3-upgrade.sqlite");
    let db_url = db_path.to_str().unwrap();
    let migration_dir = temp.path().join("pre-p3");
    std::fs::create_dir_all(&migration_dir).unwrap();
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    let mut migrations: Vec<_> = std::fs::read_dir(source)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .collect();
    migrations.sort();
    for migration in migrations.iter().filter(|path| {
        let name = path.file_name().unwrap().to_string_lossy();
        !name.contains("p3_media_pipeline") && !name.contains("p4_")
    }) {
        std::fs::copy(
            migration,
            migration_dir.join(migration.file_name().unwrap()),
        )
        .unwrap();
    }
    Sqlite::create_database(db_url).await.unwrap();
    let legacy = SqlitePool::connect(db_url).await.unwrap();
    sqlx::migrate::Migrator::new(migration_dir)
        .await
        .unwrap()
        .run(&legacy)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO media(owner_type,owner_id,media_type,url,status,created_at) \
         VALUES('post',2,'picture','https://legacy.test/shared.jpg','pending','old')",
    )
    .execute(&legacy)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO media(owner_type,owner_id,media_type,url,status,retry_count,created_at) VALUES \
         ('post',5,'picture','https://legacy.test/retry.jpg','failed',2,'old'), \
         ('post',6,'picture','https://legacy.test/terminal.jpg','failed',5,'old')",
    )
    .execute(&legacy)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO media(owner_type,owner_id,media_type,url,status,created_at) VALUES \
         ('post',8,'image','https://legacy.test/image.jpg','pending','old'), \
         ('post',9,'unknown','https://legacy.test/unknown.bin','pending','old')",
    )
    .execute(&legacy)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO media(owner_type,owner_id,media_type,url,status,created_at) \
         VALUES('post',NULL,'picture','https://legacy.test/no-owner.jpg','pending','old')",
    )
    .execute(&legacy)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO picture(id,definition,path,post_id,url,user_id) VALUES \
         ('pic','large','shared.jpg',2,'https://legacy.test/shared.jpg',3)",
    )
    .execute(&legacy)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO picture(id,definition,path,post_id,url) VALUES \
         ('prefixed','large','pictures/prefixed.jpg',7,'https://legacy.test/prefixed.jpg')",
    )
    .execute(&legacy)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO video(url,path,post_id) VALUES \
         ('https://legacy.test/video.mp4','video.mp4',4)",
    )
    .execute(&legacy)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO video(url,path,post_id) VALUES \
         ('https://legacy.test/shared.jpg','shared.mp4',2)",
    )
    .execute(&legacy)
    .await
    .unwrap();
    legacy.close().await;

    let upgraded = create_db_pool_with_url(db_url).await.unwrap();
    let assets: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM media")
        .fetch_one(&upgraded)
        .await
        .unwrap();
    let references: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM media_references")
        .fetch_one(&upgraded)
        .await
        .unwrap();
    assert_eq!(
        assets, 8,
        "all legacy assets must survive URL deduplication"
    );
    assert_eq!(
        references, 8,
        "null owners and shadowed empty definitions must not create references"
    );
    let shared = get_media_by_url(&upgraded, "https://legacy.test/shared.jpg")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(shared.status, "downloaded");
    assert_eq!(shared.media_type, "video");
    assert_eq!(shared.local_path.as_deref(), Some("videos/shared.mp4"));
    let prefixed = get_media_by_url(&upgraded, "https://legacy.test/prefixed.jpg")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        prefixed.local_path.as_deref(),
        Some("pictures/prefixed.jpg")
    );
    let video = get_media_by_url(&upgraded, "https://legacy.test/video.mp4")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(video.local_path.as_deref(), Some("videos/video.mp4"));
    let shared_definitions: Vec<String> = sqlx::query_scalar(
        "SELECT definition FROM media_references r JOIN media m ON m.id=r.media_id \
         WHERE m.url='https://legacy.test/shared.jpg' AND r.owner_type='post' AND r.owner_id=2",
    )
    .fetch_all(&upgraded)
    .await
    .unwrap();
    assert_eq!(shared_definitions, ["large"]);
    let retry: (i64, Option<i64>) = sqlx::query_as(
        "SELECT retry_count,next_retry_at_epoch FROM media WHERE url='https://legacy.test/retry.jpg'",
    )
    .fetch_one(&upgraded)
    .await
    .unwrap();
    assert_eq!(retry, (2, Some(0)));
    let terminal: (i64, Option<i64>) = sqlx::query_as(
        "SELECT retry_count,next_retry_at_epoch FROM media WHERE url='https://legacy.test/terminal.jpg'",
    )
    .fetch_one(&upgraded)
    .await
    .unwrap();
    assert_eq!(terminal, (5, None));
    let legacy_types: Vec<(String, String)> = sqlx::query_as(
        "SELECT url,media_type FROM media \
         WHERE url IN ('https://legacy.test/image.jpg','https://legacy.test/unknown.bin') \
         ORDER BY url",
    )
    .fetch_all(&upgraded)
    .await
    .unwrap();
    assert_eq!(
        legacy_types,
        [
            ("https://legacy.test/image.jpg".into(), "picture".into()),
            ("https://legacy.test/unknown.bin".into(), "picture".into()),
        ]
    );
    for table in ["media_legacy", "picture", "video"] {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?)",
        )
        .bind(table)
        .fetch_one(&upgraded)
        .await
        .unwrap();
        assert!(exists, "legacy table {table} must be retained");
    }
    upgraded.close().await;

    let reopened = create_db_pool_with_url(db_url).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM media_references")
            .fetch_one(&reopened)
            .await
            .unwrap(),
        references
    );
}

#[tokio::test]
async fn asset_is_url_unique_and_references_preserve_all_owners_and_download_state() {
    let pool = create_db_pool_with_url(":memory:").await.unwrap();
    let url = "https://wx.example.test/shared.jpg";
    save_media_reference_with_definition(&pool, &media(url, 11, "large"))
        .await
        .unwrap();

    sqlx::query(
        "UPDATE media SET status='downloaded',local_path='pictures/fixed.jpg',content_length=1,retry_count=2 WHERE url=?",
    )
    .bind(url)
    .execute(&pool)
    .await
    .unwrap();
    save_media_reference_with_definition(&pool, &media(url, 22, "original"))
        .await
        .unwrap();

    let asset = get_media_by_url(&pool, url).await.unwrap().unwrap();
    assert_eq!(asset.status, "downloaded");
    assert_eq!(asset.local_path.as_deref(), Some("pictures/fixed.jpg"));
    assert_eq!(asset.retry_count, 2);
    let references: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM media_references")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(references, 2);

    let owner = get_owner_media(&pool, "post", Some(22)).await.unwrap();
    assert_eq!(owner.len(), 1);
    assert_eq!(owner[0].definition.as_deref(), Some("original"));
    assert_eq!(owner[0].preferred_source(), MediaSource::Remote(url.into()));
}

#[tokio::test]
async fn local_source_resolution_rejects_absolute_and_escaping_database_paths() {
    let pool = create_db_pool_with_url(":memory:").await.unwrap();
    let temp = tempfile::tempdir().unwrap();
    save_media_reference_with_definition(
        &pool,
        &media("https://example.test/path.jpg", 8, "large"),
    )
    .await
    .unwrap();
    for unsafe_path in ["../outside.jpg", "C:/Windows/system.ini", "/etc/passwd"] {
        sqlx::query(
            "UPDATE media SET status='downloaded',local_path=?,content_length=1 WHERE url='https://example.test/path.jpg'",
        )
        .bind(unsafe_path)
        .execute(&pool)
        .await
        .unwrap();
        let item = get_owner_media(&pool, "post", Some(8))
            .await
            .unwrap()
            .remove(0);
        assert_eq!(
            item.resolve_local_source(temp.path()),
            ResolvedMediaSource::Remote(item.url)
        );
    }

    for unsafe_path in ["pictures/missing.jpg", "pictures/not-a-file"] {
        if unsafe_path.ends_with("not-a-file") {
            std::fs::create_dir_all(temp.path().join(unsafe_path)).unwrap();
        }
        sqlx::query("UPDATE media SET local_path=? WHERE url='https://example.test/path.jpg'")
            .bind(unsafe_path)
            .execute(&pool)
            .await
            .unwrap();
        let item = get_owner_media(&pool, "post", Some(8))
            .await
            .unwrap()
            .remove(0);
        assert_eq!(
            item.resolve_local_source(temp.path()),
            ResolvedMediaSource::Remote(item.url)
        );
    }

    std::fs::create_dir_all(temp.path().join("pictures")).unwrap();
    std::fs::write(temp.path().join("pictures/safe.jpg"), b"safe").unwrap();
    sqlx::query(
        "UPDATE media SET local_path='pictures/safe.jpg' WHERE url='https://example.test/path.jpg'",
    )
    .execute(&pool)
    .await
    .unwrap();
    let item = get_owner_media(&pool, "post", Some(8))
        .await
        .unwrap()
        .remove(0);
    assert_eq!(
        item.resolve_local_source(temp.path()),
        ResolvedMediaSource::Local(
            temp.path()
                .join("pictures/safe.jpg")
                .canonicalize()
                .unwrap(),
        )
    );
    let mut no_remote = item;
    no_remote.url.clear();
    no_remote.local_path = Some("pictures/missing.jpg".into());
    assert_eq!(
        no_remote.resolve_local_source(temp.path()),
        ResolvedMediaSource::Unavailable
    );
}

#[cfg(unix)]
#[tokio::test]
async fn local_source_symlink_escape_falls_back_to_remote() {
    use std::os::unix::fs::symlink;

    let pool = create_db_pool_with_url(":memory:").await.unwrap();
    let temp = tempfile::tempdir().unwrap();
    let outside = tempfile::NamedTempFile::new().unwrap();
    std::fs::create_dir_all(temp.path().join("pictures")).unwrap();
    symlink(outside.path(), temp.path().join("pictures/link.jpg")).unwrap();
    let url = "https://example.test/link.jpg";
    save_media_reference_with_definition(&pool, &media(url, 81, "large"))
        .await
        .unwrap();
    sqlx::query("UPDATE media SET status='downloaded',local_path='pictures/link.jpg',content_length=1 WHERE url=?")
        .bind(url).execute(&pool).await.unwrap();
    let item = get_owner_media(&pool, "post", Some(81))
        .await
        .unwrap()
        .remove(0);
    assert_eq!(
        item.resolve_local_source(temp.path()),
        ResolvedMediaSource::Remote(url.into())
    );
}

#[tokio::test]
async fn production_url_policy_rejects_unsafe_schemes_hosts_and_dns_answers() {
    let temp = tempfile::tempdir().unwrap();
    let pool = create_db_pool_with_url(":memory:").await.unwrap();
    let production = MediaPipelineConfig {
        allow_http: false,
        allow_private_network: false,
        ..test_config(temp.path())
    };
    let private_dns = Arc::new(FixedResolver(vec![IpAddr::V4(Ipv4Addr::new(10, 2, 3, 4))]));
    let pipeline =
        MediaPipeline::new_with_resolver(pool, reqwest::Client::new(), production, private_dns);

    for url in [
        "http://public.example/image.jpg",
        "https://user:password@public.example/image.jpg",
        "https://localhost/image.jpg",
        "https://127.0.0.1/image.jpg",
        "https://8.8.8.8/image.jpg",
        "https://[::1]/image.jpg",
        "https://public.example/image.jpg",
    ] {
        assert!(
            pipeline.validate_url(url).await.is_err(),
            "must reject {url}"
        );
    }
}

#[tokio::test]
async fn production_url_policy_rejects_sensitive_query_keys_and_fragments() {
    let temp = tempfile::tempdir().unwrap();
    let pipeline = MediaPipeline::new_with_resolver(
        create_db_pool_with_url(":memory:").await.unwrap(),
        reqwest::Client::new(),
        MediaPipelineConfig {
            allow_http: false,
            allow_private_network: false,
            ..test_config(temp.path())
        },
        Arc::new(FixedResolver(vec![IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))])),
    );
    for url in [
        "https://public.example/image.jpg?access-token=secret",
        "https://public.example/image.jpg?x-gsid=secret",
        "https://public.example/image.jpg?x-passport=secret",
        "https://public.example/image.jpg?x-xsrf=secret",
        "https://public.example/image.jpg?x-signature=secret",
        "https://public.example/image.jpg#credential",
    ] {
        assert!(
            pipeline.validate_url(url).await.is_err(),
            "must reject {url}"
        );
    }
}

#[tokio::test]
async fn production_url_policy_rejects_all_special_use_address_ranges() {
    let addresses = [
        IpAddr::V4(Ipv4Addr::new(0, 1, 2, 3)),
        IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1)),
        IpAddr::V4(Ipv4Addr::new(192, 0, 0, 1)),
        IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)),
        IpAddr::V4(Ipv4Addr::new(198, 18, 0, 1)),
        IpAddr::V4(Ipv4Addr::new(198, 51, 100, 1)),
        IpAddr::V4(Ipv4Addr::new(203, 0, 113, 1)),
        IpAddr::V4(Ipv4Addr::new(240, 0, 0, 1)),
        IpAddr::V4(Ipv4Addr::BROADCAST),
        IpAddr::V6("2001:db8::1".parse::<Ipv6Addr>().unwrap()),
        IpAddr::V6("fec0::1".parse::<Ipv6Addr>().unwrap()),
    ];
    for address in addresses {
        let temp = tempfile::tempdir().unwrap();
        let pipeline = MediaPipeline::new_with_resolver(
            create_db_pool_with_url(":memory:").await.unwrap(),
            reqwest::Client::new(),
            MediaPipelineConfig {
                allow_http: false,
                allow_private_network: false,
                ..test_config(temp.path())
            },
            Arc::new(FixedResolver(vec![address])),
        );
        assert!(
            pipeline
                .validate_url("https://public.example/a.jpg")
                .await
                .is_err(),
            "must reject {address}"
        );
    }
}

#[tokio::test]
async fn redirect_targets_are_revalidated_and_redirect_count_is_bounded() {
    let mut server = mockito::Server::new_async().await;
    let redirect = server
        .mock("GET", "/one")
        .with_status(302)
        .with_header("location", "/two")
        .create_async()
        .await;
    let second = server
        .mock("GET", "/two")
        .with_status(302)
        .with_header("location", "/three")
        .create_async()
        .await;
    let temp = tempfile::tempdir().unwrap();
    let pool = create_db_pool_with_url(":memory:").await.unwrap();
    let url = format!("{}/one", server.url());
    save_media_reference_with_definition(&pool, &media(&url, 10, "large"))
        .await
        .unwrap();
    let pipeline = MediaPipeline::new(
        pool.clone(),
        reqwest::Client::new(),
        MediaPipelineConfig {
            max_redirects: 1,
            ..test_config(temp.path())
        },
    );

    assert!(pipeline.run_once().await.unwrap());
    redirect.assert_async().await;
    second.assert_async().await;
    let failed = get_media_by_url(&pool, &url).await.unwrap().unwrap();
    assert_eq!(failed.status, "failed");
    assert!(failed.last_error.unwrap().contains("redirect"));
}

#[tokio::test]
async fn redirect_to_disallowed_scheme_is_rejected_before_second_request() {
    let mut server = mockito::Server::new_async().await;
    let redirect = server
        .mock("GET", "/unsafe")
        .with_status(302)
        .with_header("location", "ftp://127.0.0.1/private")
        .create_async()
        .await;
    let temp = tempfile::tempdir().unwrap();
    let pool = create_db_pool_with_url(":memory:").await.unwrap();
    let url = format!("{}/unsafe", server.url());
    save_media_reference_with_definition(&pool, &media(&url, 13, "large"))
        .await
        .unwrap();
    let pipeline = MediaPipeline::new(
        pool.clone(),
        reqwest::Client::new(),
        test_config(temp.path()),
    );

    assert!(pipeline.run_once().await.unwrap());
    redirect.assert_async().await;
    let failed = get_media_by_url(&pool, &url).await.unwrap().unwrap();
    assert_eq!(failed.status, "failed");
    assert!(failed.last_error.unwrap().contains("HTTPS"));
}

#[tokio::test]
async fn claim_is_atomic_and_stale_token_cannot_finish() {
    let pool = create_db_pool_with_url(":memory:").await.unwrap();
    save_media_reference_with_definition(&pool, &media("https://example.test/a.jpg", 1, "large"))
        .await
        .unwrap();
    let first = claim_next_media(
        &pool,
        &MediaClaimRequest {
            token: "worker-a".into(),
            now_epoch: 100,
            claimed_at: "1970-01-01T00:01:40Z".into(),
        },
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        claim_next_media(
            &pool,
            &MediaClaimRequest {
                token: "worker-b".into(),
                now_epoch: 100,
                claimed_at: "1970-01-01T00:01:40Z".into(),
            },
        )
        .await
        .unwrap()
        .is_none()
    );

    recover_downloading_media(&pool, "restarted").await.unwrap();
    let second = claim_next_media(
        &pool,
        &MediaClaimRequest {
            token: "worker-b".into(),
            now_epoch: 101,
            claimed_at: "1970-01-01T00:01:41Z".into(),
        },
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(first.id, second.id);
    let completion = MediaDownloadCompletion {
        local_path: "pictures/a.jpg".into(),
        content_type: Some("image/jpeg".into()),
        content_length: 4,
        updated_at: "done".into(),
    };
    assert!(
        !complete_media_download(&pool, first.id, "worker-a", &completion)
            .await
            .unwrap()
    );
    assert!(
        complete_media_download(&pool, second.id, "worker-b", &completion)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn failed_items_retry_only_when_due_and_manual_retry_is_immediate() {
    let pool = create_db_pool_with_url(":memory:").await.unwrap();
    save_media_reference_with_definition(
        &pool,
        &media("https://example.test/retry.jpg", 1, "large"),
    )
    .await
    .unwrap();
    let claim = claim_next_media(
        &pool,
        &MediaClaimRequest {
            token: "worker".into(),
            now_epoch: 1_000,
            claimed_at: "claimed".into(),
        },
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        fail_media_download(&pool, claim.id, "worker", 1_000, "network", "failed")
            .await
            .unwrap()
    );
    assert!(
        claim_next_media(
            &pool,
            &MediaClaimRequest {
                token: "early".into(),
                now_epoch: 1_001,
                claimed_at: "early".into(),
            },
        )
        .await
        .unwrap()
        .is_none()
    );
    assert!(retry_media(&pool, claim.id, "manual").await.unwrap());
    assert!(
        claim_next_media(
            &pool,
            &MediaClaimRequest {
                token: "manual".into(),
                now_epoch: 1_001,
                claimed_at: "manual".into(),
            },
        )
        .await
        .unwrap()
        .is_some()
    );
}

#[tokio::test]
async fn worker_streams_valid_image_atomically_and_cleans_parts_on_restart() {
    let mut server = mockito::Server::new_async().await;
    let mut png = Vec::new();
    image::DynamicImage::new_rgba8(1, 1)
        .write_to(
            &mut std::io::Cursor::new(&mut png),
            image::ImageOutputFormat::Png,
        )
        .unwrap();
    let mock = server
        .mock("GET", "/image")
        .with_status(200)
        .with_header("content-type", "image/png")
        .with_body(png)
        .create_async()
        .await;
    let temp = tempfile::tempdir().unwrap();
    let pool = create_db_pool_with_url(":memory:").await.unwrap();
    let url = format!("{}/image", server.url());
    save_media_reference_with_definition(&pool, &media(&url, 7, "original"))
        .await
        .unwrap();
    std::fs::create_dir_all(temp.path().join("pictures")).unwrap();
    std::fs::write(temp.path().join("pictures/orphan.part"), b"partial").unwrap();

    let pipeline = MediaPipeline::new(
        pool.clone(),
        reqwest::Client::new(),
        test_config(temp.path()),
    );
    let recovery = pipeline.recover_startup().await.unwrap();
    assert_eq!(recovery.parts_removed, 1);
    assert!(pipeline.run_once().await.unwrap());
    mock.assert_async().await;

    let downloaded = get_media_by_url(&pool, &url).await.unwrap().unwrap();
    assert_eq!(downloaded.status, "downloaded", "{downloaded:?}");
    let path = temp.path().join(downloaded.local_path.unwrap());
    assert!(path.is_file());
    assert!(!path.with_extension("png.part").exists());
}

#[tokio::test]
async fn existing_valid_final_file_repairs_pending_and_downloading_rows() {
    let mut server = mockito::Server::new_async().await;
    let mut png = Vec::new();
    image::DynamicImage::new_rgba8(1, 1)
        .write_to(
            &mut std::io::Cursor::new(&mut png),
            image::ImageOutputFormat::Png,
        )
        .unwrap();
    let mock = server
        .mock("GET", "/recover")
        .with_status(200)
        .with_header("content-type", "image/png")
        .with_body(png)
        .expect(1)
        .create_async()
        .await;
    let temp = tempfile::tempdir().unwrap();
    let pool = create_db_pool_with_url(":memory:").await.unwrap();
    let url = format!("{}/recover", server.url());
    save_media_reference_with_definition(&pool, &media(&url, 11, "original"))
        .await
        .unwrap();
    let pipeline = MediaPipeline::new(
        pool.clone(),
        reqwest::Client::new(),
        test_config(temp.path()),
    );
    assert!(pipeline.run_once().await.unwrap());
    let downloaded = get_media_by_url(&pool, &url).await.unwrap().unwrap();
    let local_path = downloaded.local_path.clone().unwrap();

    for status in ["pending", "downloading"] {
        sqlx::query(
            "UPDATE media SET status=?,local_path=NULL,content_length=NULL,claim_token=CASE WHEN ?='downloading' THEN 'dead' ELSE NULL END WHERE id=?",
        )
        .bind(status)
        .bind(status)
        .bind(downloaded.id)
        .execute(&pool)
        .await
        .unwrap();
        pipeline.recover_startup().await.unwrap();
        assert!(pipeline.run_once().await.unwrap());
        let repaired = get_media_by_url(&pool, &url).await.unwrap().unwrap();
        assert_eq!(repaired.status, "downloaded");
        assert_eq!(repaired.local_path.as_deref(), Some(local_path.as_str()));
    }
    mock.assert_async().await;
}

#[tokio::test]
async fn stale_completion_keeps_final_for_new_claim_recovery() {
    let mut server = mockito::Server::new_async().await;
    let mut png = Vec::new();
    image::DynamicImage::new_rgba8(1, 1)
        .write_to(
            &mut std::io::Cursor::new(&mut png),
            image::ImageOutputFormat::Png,
        )
        .unwrap();
    let mock = server
        .mock("GET", "/stale")
        .with_status(200)
        .with_header("content-type", "image/png")
        .with_chunked_body(move |writer| {
            writer.write_all(&png[..16])?;
            std::thread::sleep(Duration::from_millis(150));
            writer.write_all(&png[16..])
        })
        .expect(1)
        .create_async()
        .await;
    let temp = tempfile::tempdir().unwrap();
    let pool = create_db_pool_with_url(":memory:").await.unwrap();
    let url = format!("{}/stale", server.url());
    save_media_reference_with_definition(&pool, &media(&url, 14, "large"))
        .await
        .unwrap();
    let pipeline = MediaPipeline::new(
        pool.clone(),
        reqwest::Client::new(),
        test_config(temp.path()),
    );
    let old_worker = tokio::spawn({
        let pipeline = pipeline.clone();
        async move { pipeline.run_once().await.unwrap() }
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let status: String = sqlx::query_scalar("SELECT status FROM media WHERE url=?")
                .bind(&url)
                .fetch_one(&pool)
                .await
                .unwrap();
            if status == "downloading" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    sqlx::query("UPDATE media SET claim_token='new-owner' WHERE url=?")
        .bind(&url)
        .execute(&pool)
        .await
        .unwrap();
    assert!(old_worker.await.unwrap());
    let final_count = std::fs::read_dir(temp.path().join("pictures"))
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| !entry.file_name().to_string_lossy().ends_with(".part"))
        .count();
    assert_eq!(final_count, 1, "stale completion must retain final file");

    pipeline.recover_startup().await.unwrap();
    assert!(pipeline.run_once().await.unwrap());
    let repaired = get_media_by_url(&pool, &url).await.unwrap().unwrap();
    assert_eq!(repaired.status, "downloaded");
    mock.assert_async().await;
}

#[tokio::test]
async fn shutdown_interrupts_streaming_download_and_startup_recovers_state() {
    let mut server = mockito::Server::new_async().await;
    let mock = server
        .mock("GET", "/slow")
        .with_status(200)
        .with_header("content-type", "image/png")
        .with_chunked_body(|writer| {
            writer.write_all(b"\x89PNG\r\n\x1a\n")?;
            std::thread::sleep(Duration::from_secs(5));
            writer.write_all(&[0; 32])
        })
        .create_async()
        .await;
    let temp = tempfile::tempdir().unwrap();
    let pool = create_db_pool_with_url(":memory:").await.unwrap();
    let url = format!("{}/slow", server.url());
    save_media_reference_with_definition(&pool, &media(&url, 12, "large"))
        .await
        .unwrap();
    let pipeline = MediaPipeline::new(
        pool.clone(),
        reqwest::Client::new(),
        test_config(temp.path()),
    );
    let worker = MediaWorkerTask::spawn(pipeline.clone());
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let status: String = sqlx::query_scalar("SELECT status FROM media WHERE url=?")
                .bind(&url)
                .fetch_one(&pool)
                .await
                .unwrap();
            let part_exists = temp
                .path()
                .join("pictures")
                .read_dir()
                .ok()
                .into_iter()
                .flatten()
                .filter_map(|entry| entry.ok())
                .any(|entry| entry.file_name().to_string_lossy().ends_with(".part"));
            if status == "downloading" && part_exists {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();

    let started = tokio::time::Instant::now();
    let summary = worker.shutdown(Duration::from_secs(1)).await;
    assert!(summary.stopped, "{summary:?}");
    assert!(started.elapsed() < Duration::from_secs(2));
    let status: String = sqlx::query_scalar("SELECT status FROM media WHERE url=?")
        .bind(&url)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "downloading");

    let recovery = pipeline.recover_startup().await.unwrap();
    assert_eq!(recovery.downloading_requeued, 1);
    let status: String = sqlx::query_scalar("SELECT status FROM media WHERE url=?")
        .bind(&url)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "pending");
    assert!(recovery.parts_removed >= 1);
    mock.assert_async().await;
}

#[tokio::test]
async fn hard_abort_midstream_leaves_no_final_and_startup_makes_it_retryable() {
    let mut server = mockito::Server::new_async().await;
    let mock = server
        .mock("GET", "/hard-abort")
        .with_status(200)
        .with_header("content-type", "image/png")
        .with_chunked_body(|writer| {
            writer.write_all(b"\x89PNG\r\n\x1a\n")?;
            std::thread::sleep(Duration::from_secs(5));
            writer.write_all(&[0; 32])
        })
        .expect(2)
        .create_async()
        .await;
    let temp = tempfile::tempdir().unwrap();
    let pool = create_db_pool_with_url(":memory:").await.unwrap();
    let url = format!("{}/hard-abort", server.url());
    save_media_reference_with_definition(&pool, &media(&url, 16, "large"))
        .await
        .unwrap();
    let pipeline = MediaPipeline::new(
        pool.clone(),
        reqwest::Client::new(),
        test_config(temp.path()),
    );
    let worker = MediaWorkerTask::spawn(pipeline.clone());

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let status: String = sqlx::query_scalar("SELECT status FROM media WHERE url=?")
                .bind(&url)
                .fetch_one(&pool)
                .await
                .unwrap();
            let has_part = temp
                .path()
                .join("pictures")
                .read_dir()
                .ok()
                .into_iter()
                .flatten()
                .filter_map(|entry| entry.ok())
                .any(|entry| entry.file_name().to_string_lossy().ends_with(".part"));
            if status == "downloading" && has_part {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();

    worker.abort_for_test().await;
    let final_count = std::fs::read_dir(temp.path().join("pictures"))
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| !entry.file_name().to_string_lossy().ends_with(".part"))
        .count();
    assert_eq!(final_count, 0, "hard abort must not publish a final file");
    assert_eq!(
        get_media_by_url(&pool, &url).await.unwrap().unwrap().status,
        "downloading"
    );

    let recovery = pipeline.recover_startup().await.unwrap();
    assert_eq!(recovery.downloading_requeued, 1);
    assert!(recovery.parts_removed >= 1);
    assert_eq!(
        get_media_by_url(&pool, &url).await.unwrap().unwrap().status,
        "pending"
    );
    assert!(pipeline.run_once().await.unwrap());
    assert_eq!(
        get_media_by_url(&pool, &url).await.unwrap().unwrap().status,
        "failed",
        "the recovered item must be claimable and retried"
    );
    mock.assert_async().await;
}

#[tokio::test]
async fn worker_retries_startup_failure_until_storage_recovers() {
    let mut server = mockito::Server::new_async().await;
    let mut png = Vec::new();
    image::DynamicImage::new_rgba8(1, 1)
        .write_to(
            &mut std::io::Cursor::new(&mut png),
            image::ImageOutputFormat::Png,
        )
        .unwrap();
    let mock = server
        .mock("GET", "/after-recovery")
        .with_status(200)
        .with_header("content-type", "image/png")
        .with_body(png)
        .create_async()
        .await;
    let temp = tempfile::tempdir().unwrap();
    let blocked_root = temp.path().join("media");
    std::fs::write(&blocked_root, b"not a directory").unwrap();
    let pool = create_db_pool_with_url(":memory:").await.unwrap();
    let url = format!("{}/after-recovery", server.url());
    save_media_reference_with_definition(&pool, &media(&url, 15, "large"))
        .await
        .unwrap();
    let pipeline = MediaPipeline::new(
        pool.clone(),
        reqwest::Client::new(),
        test_config(&blocked_root),
    );
    let worker = MediaWorkerTask::spawn(pipeline);
    tokio::time::sleep(Duration::from_millis(50)).await;
    std::fs::remove_file(&blocked_root).unwrap();
    std::fs::create_dir_all(&blocked_root).unwrap();

    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if get_media_by_url(&pool, &url).await.unwrap().unwrap().status == "downloaded" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(worker.shutdown(Duration::from_secs(1)).await.stopped);
    mock.assert_async().await;
}

#[tokio::test]
async fn oversized_or_invalid_image_fails_media_without_losing_owner_reference() {
    let mut server = mockito::Server::new_async().await;
    let mock = server
        .mock("GET", "/bad")
        .with_status(200)
        .with_header("content-type", "image/png")
        .with_body(vec![b'x'; 64])
        .create_async()
        .await;
    let temp = tempfile::tempdir().unwrap();
    let pool = create_db_pool_with_url(":memory:").await.unwrap();
    let url = format!("{}/bad", server.url());
    save_media_reference_with_definition(&pool, &media(&url, 9, "large"))
        .await
        .unwrap();
    let pipeline = MediaPipeline::new(
        pool.clone(),
        reqwest::Client::new(),
        MediaPipelineConfig {
            max_bytes: 32,
            ..test_config(temp.path())
        },
    );

    assert!(pipeline.run_once().await.unwrap());
    mock.assert_async().await;
    let failed = get_media_by_url(&pool, &url).await.unwrap().unwrap();
    assert_eq!(failed.status, "failed");
    assert!(failed.last_error.unwrap().contains("maximum"));
    assert_eq!(
        get_owner_media(&pool, "post", Some(9)).await.unwrap().len(),
        1
    );

    let row = sqlx::query("SELECT retry_count,next_retry_at_epoch FROM media WHERE id=?")
        .bind(failed.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row.get::<i64, _>("retry_count"), 1);
    assert!(row.get::<Option<i64>, _>("next_retry_at_epoch").is_some());
}
