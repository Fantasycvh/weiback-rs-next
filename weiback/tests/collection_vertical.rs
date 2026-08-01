//! P1-B 垂直切片：Rust 发起采集 → Sidecar 输出数据与 checkpoint →
//! Rust 批事务写入 → 崩溃续传。
//!
//! 覆盖门槛：
//! - 完整采集：帖子/用户/媒体引用 + checkpoint 同事务落库，done completed。
//! - Sidecar 崩溃：未提交批丢弃，任务标记 `Interrupted`，已提交页不丢。
//! - 续传：从最后已提交 cursor 继续，已提交页不重不丢。

use std::{
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use serde_json::json;
use sqlx::{SqlitePool, query_scalar};
use weiback::sidecar::{
    CollectionRequest, CollectionStatus, CollectionSummary, CommandType, Sidecar, SpawnOptions,
    run_collection,
};
use weiback::storage::database::create_db_pool_with_url;
use weiback::storage::internal::entities::SyncCheckpointDto;
use weiback::storage::internal::entities::{get_sync_checkpoint, save_sync_checkpoint};

/// 仓库根目录（`tests/` 的上两级）。
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

/// 定位可用的 Python 解释器。
fn python() -> Option<PathBuf> {
    let candidates: Vec<PathBuf> = std::env::var("WEIBACK_COLLECTOR_PYTHON")
        .map(PathBuf::from)
        .into_iter()
        .chain([
            PathBuf::from("F:\\build\\projects\\weiback-python\\.venv\\Scripts\\python.exe"),
            PathBuf::from("python"),
            PathBuf::from("python3"),
        ])
        .collect();
    candidates.into_iter().find(|p| {
        Command::new(p)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

/// 启动真实 Python sidecar 的选项，并用给定 fixture 重放。
fn collector_options(python: &Path, fixture: &str) -> SpawnOptions {
    let root = repo_root();
    SpawnOptions {
        program: python.to_path_buf(),
        args: vec!["-u".into(), "-m".into(), "weiback_collector".into()],
        env: vec![
            (
                "PYTHONPATH".into(),
                root.join("sidecar").to_string_lossy().to_string(),
            ),
            ("PYTHONUTF8".into(), "1".into()),
            ("WEIBACK_COLLECTOR_FIXTURE".into(), fixture.to_string()),
        ],
        cwd: Some(root),
        handshake_timeout: Duration::from_secs(10),
    }
}

async fn setup_db() -> SqlitePool {
    create_db_pool_with_url(":memory:").await.unwrap()
}

fn user_posts_request(uid: &str) -> CollectionRequest {
    CollectionRequest {
        command_type: CommandType::CollectUserPosts,
        stream: format!("user:{uid}:posts"),
        payload: json!({"uid": uid, "max_pages": 10}),
    }
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// 完整采集 long_text_full fixture：post/user/media/checkpoint 全部落库。
#[tokio::test]
async fn collect_posts_commits_all_batches() {
    let Some(py) = python() else {
        eprintln!("python not found, skipping");
        return;
    };
    let pool = setup_db().await;
    let (mut sidecar, _ready, _capabilities) =
        Sidecar::spawn_with_handshake(&collector_options(&py, "posts/long_text_full.jsonl"))
            .expect("handshake should succeed");

    let request = user_posts_request("1234567890");
    let summary = run_collection(
        &mut sidecar,
        &pool,
        &request,
        |_, _| {},
        Duration::from_secs(5),
    )
    .await
    .expect("collection should run");

    assert_eq!(
        summary.status,
        CollectionStatus::Completed,
        "summary: {summary:?}"
    );

    // 帖子已落库。
    let posts: i64 = query_scalar("SELECT COUNT(*) FROM posts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(posts, 1, "expected one post from long_text_full fixture");

    // 扩展字段往返：长文完整字段保留。
    let text: String = query_scalar("SELECT text FROM posts WHERE id = 4876543210987654321")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(text.contains("长文"));
    let is_long: i64 =
        query_scalar("SELECT is_long_text FROM posts WHERE id = 4876543210987654321")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(is_long, 1);
    let content_status: String =
        query_scalar("SELECT content_status FROM posts WHERE id = 4876543210987654321")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(content_status, "complete");

    // 用户落库。
    let users: i64 = query_scalar("SELECT COUNT(*) FROM users")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(users, 1);

    // 媒体引用落库（picture）。
    let media: i64 = query_scalar("SELECT COUNT(*) FROM media")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(media, 1);

    // checkpoint 落库。
    let cp = get_sync_checkpoint(&pool, "user:1234567890:posts")
        .await
        .unwrap()
        .expect("checkpoint should exist");
    assert_eq!(cp.fetched_count, 1);

    sidecar
        .shutdown(Duration::from_millis(500))
        .expect("clean shutdown");
}

/// Sidecar 崩溃：已提交 checkpoint 页保留，任务标记 Interrupted。
#[tokio::test]
async fn sidecar_crash_marks_interrupted_and_keeps_committed_page() {
    let Some(py) = python() else {
        eprintln!("python not found, skipping");
        return;
    };
    let pool = setup_db().await;

    // 崩溃 sidecar：握手后等 collect 命令，输出一页数据 + checkpoint，然后 exit(9)。
    let script = concat!(
        "import sys\n",
        "print('{\"protocol_version\":1,\"request_id\":null,\"event_id\":\"",
        "019fbbd7-ea26-7b7c-b113-c89ac2788701\",\"type\":\"ready\",\"occurred_at\":\"",
        "2026-08-01T00:00:00.000Z\",\"payload\":{\"sidecar_name\":\"weiback-collector\",",
        "\"sidecar_version\":\"0.4.0\",\"protocol_version\":1}}')\n",
        "print('{\"protocol_version\":1,\"request_id\":null,\"event_id\":\"",
        "019fbbd7-ea26-7b7c-b113-c89ac2788702\",\"type\":\"capabilities\",\"occurred_at\":\"",
        "2026-08-01T00:00:00.001Z\",\"payload\":{\"protocol_versions\":[1],",
        "\"commands\":[\"hello\",\"health\",\"collect_user_posts\",\"collect_comments\",",
        "\"collect_comment_replies\",\"cancel\",\"shutdown\"],\"browser_installed\":false}}')\n",
        "sys.stdout.flush()\n",
        "import json\n",
        "sys.stdin.readline()\n",                     // hello
        "command=json.loads(sys.stdin.readline())\n", // collect 命令
        "rid=command['request_id']\n",
        "base={'protocol_version':1,'request_id':rid,'stream':'user:123:posts'}\n",
        "print(json.dumps({**base,'event_id':'019fbbd7-ea26-7b7c-b113-c89ac2788703',",
        "'type':'post','sequence':1,'occurred_at':'2026-08-01T00:00:01.000Z',",
        "'payload':{'id':'9001','uid':'123','text':'crash post'}}))\n",
        "print(json.dumps({**base,'event_id':'019fbbd7-ea26-7b7c-b113-c89ac2788704',",
        "'type':'checkpoint','sequence':2,'occurred_at':'2026-08-01T00:00:02.000Z',",
        "'payload':{'cursor':{'max_id':'p1_after','max_id_type':0},",
        "'fetched_count':1,'has_more':True}}))\n",
        "sys.stdout.flush()\n",
        "sys.exit(9)\n",
    );
    let options = SpawnOptions {
        program: py.to_path_buf(),
        args: vec!["-u".into(), "-c".into(), script.into()],
        env: vec![("PYTHONUTF8".into(), "1".into())],
        cwd: None,
        handshake_timeout: Duration::from_secs(5),
    };
    let (mut sidecar, _ready, _capabilities) =
        Sidecar::spawn_with_handshake(&options).expect("handshake should succeed");

    let request = user_posts_request("123");
    let summary: CollectionSummary = run_collection(
        &mut sidecar,
        &pool,
        &request,
        |_, _| {},
        Duration::from_secs(5),
    )
    .await
    .expect("collection should return summary");

    assert_eq!(
        summary.status,
        CollectionStatus::Interrupted,
        "summary: {summary:?}"
    );
    assert!(
        summary.error.as_deref().unwrap_or("").contains("9"),
        "summary: {summary:?}"
    );

    // 已提交页不丢：checkpoint 提交成功，post 在库。
    let posts: i64 = query_scalar("SELECT COUNT(*) FROM posts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(posts, 1, "committed page must survive crash");
    let cp = get_sync_checkpoint(&pool, "user:123:posts")
        .await
        .unwrap()
        .expect("committed checkpoint must survive crash");
    assert_eq!(cp.fetched_count, 1);
}

/// Sidecar 若在数据事件后直接 done，Rust 不得把未受 checkpoint 覆盖的数据落库。
#[tokio::test]
async fn done_without_checkpoint_rejects_uncommitted_data() {
    let Some(py) = python() else {
        eprintln!("python not found, skipping");
        return;
    };
    let pool = setup_db().await;
    let script = concat!(
        "import json,sys\n",
        "print('{\"protocol_version\":1,\"request_id\":null,\"event_id\":\"",
        "019fbbd7-ea26-7b7c-b113-c89ac2788801\",\"type\":\"ready\",\"occurred_at\":\"",
        "2026-08-01T00:00:00.000Z\",\"payload\":{\"sidecar_name\":\"weiback-collector\",",
        "\"sidecar_version\":\"0.4.0\",\"protocol_version\":1}}')\n",
        "print('{\"protocol_version\":1,\"request_id\":null,\"event_id\":\"",
        "019fbbd7-ea26-7b7c-b113-c89ac2788802\",\"type\":\"capabilities\",\"occurred_at\":\"",
        "2026-08-01T00:00:00.001Z\",\"payload\":{\"protocol_versions\":[1],",
        "\"commands\":[\"hello\",\"collect_user_posts\",\"shutdown\"],",
        "\"browser_installed\":false}}')\n",
        "sys.stdout.flush()\n",
        "sys.stdin.readline()\n",
        "command=json.loads(sys.stdin.readline())\n",
        "rid=command['request_id']\n",
        "base={'protocol_version':1,'request_id':rid,'occurred_at':'2026-08-01T00:00:01.000Z',",
        "'stream':'user:321:posts'}\n",
        "print(json.dumps({**base,'event_id':'019fbbd7-ea26-7b7c-b113-c89ac2788803',",
        "'type':'post','sequence':1,'payload':{'id':'9901','uid':'321','text':'uncommitted'}}))\n",
        "print(json.dumps({**base,'event_id':'019fbbd7-ea26-7b7c-b113-c89ac2788804',",
        "'type':'done','sequence':2,'payload':{'status':'completed','fetched_count':1,'has_more':False}}))\n",
        "sys.stdout.flush()\n",
    );
    let options = SpawnOptions {
        program: py,
        args: vec!["-u".into(), "-c".into(), script.into()],
        env: vec![("PYTHONUTF8".into(), "1".into())],
        cwd: None,
        handshake_timeout: Duration::from_secs(5),
    };
    let (mut sidecar, _, _) =
        Sidecar::spawn_with_handshake(&options).expect("handshake should succeed");

    let error = run_collection(
        &mut sidecar,
        &pool,
        &user_posts_request("321"),
        |_, _| {},
        Duration::from_secs(5),
    )
    .await
    .expect_err("uncheckpointed data must fail the collection");
    assert!(error.to_string().contains("uncommitted data"));

    let posts: i64 = query_scalar("SELECT COUNT(*) FROM posts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(posts, 0);
    assert!(
        get_sync_checkpoint(&pool, "user:321:posts")
            .await
            .unwrap()
            .is_none()
    );
}

/// 续传：从最后已提交 cursor 继续，已提交页不重不丢。
#[tokio::test]
async fn resume_from_committed_checkpoint_skips_covered_pages() {
    let Some(py) = python() else {
        eprintln!("python not found, skipping");
        return;
    };
    let pool = setup_db().await;

    // 预置一个中间 checkpoint：第 1 页已提交（p1_after，20 条）。
    save_sync_checkpoint(
        &pool,
        &SyncCheckpointDto {
            stream: "user:1000000010:posts".into(),
            cursor_json: Some(
                json!({"cursor": {"max_id": "p1_after", "max_id_type": 0}}).to_string(),
            ),
            fetched_count: 20,
            last_sequence: Some(3),
            updated_at: now(),
        },
    )
    .await
    .unwrap();

    let (mut sidecar, _ready, _capabilities) = Sidecar::spawn_with_handshake(&collector_options(
        &py,
        "checkpoints/pagination_cursor_progress.jsonl",
    ))
    .expect("handshake should succeed");

    let request = user_posts_request("1000000010");
    let summary = run_collection(
        &mut sidecar,
        &pool,
        &request,
        |_, _| {},
        Duration::from_secs(5),
    )
    .await
    .expect("collection should run");

    assert_eq!(
        summary.status,
        CollectionStatus::Completed,
        "summary: {summary:?}"
    );

    // 续传跳过第 1 页（id 101），只收集第 2、3 页（102、103）。
    let posts: i64 = query_scalar("SELECT COUNT(*) FROM posts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(posts, 2, "resume must skip the committed first page");
    let has_101: i64 = query_scalar("SELECT COUNT(*) FROM posts WHERE id = 5550000000000000101")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(has_101, 0, "committed page must not be refetched");

    // checkpoint 推进到最后（p3_after，60 条）。
    let cp = get_sync_checkpoint(&pool, "user:1000000010:posts")
        .await
        .unwrap()
        .expect("checkpoint should exist");
    assert_eq!(cp.fetched_count, 60);

    sidecar
        .shutdown(Duration::from_millis(500))
        .expect("clean shutdown");

    // 再跑一次：无新页可采，posts 数量不变（不重不丢）。
    let (mut sidecar2, _ready2, _capabilities2) = Sidecar::spawn_with_handshake(
        &collector_options(&py, "checkpoints/pagination_cursor_progress.jsonl"),
    )
    .expect("second handshake should succeed");
    let summary2 = run_collection(
        &mut sidecar2,
        &pool,
        &request,
        |_, _| {},
        Duration::from_secs(5),
    )
    .await
    .expect("second collection should run");
    assert_eq!(
        summary2.status,
        CollectionStatus::Completed,
        "summary2: {summary2:?}"
    );

    let posts_after: i64 = query_scalar("SELECT COUNT(*) FROM posts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(posts_after, 2, "idempotent rerun must not duplicate posts");

    sidecar2
        .shutdown(Duration::from_millis(500))
        .expect("clean shutdown");
}
