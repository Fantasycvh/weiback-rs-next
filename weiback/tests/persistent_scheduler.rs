use std::{
    path::PathBuf,
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use tempfile::tempdir;
use weiback::refresh_scheduler::{PersistentScheduler, RefreshScheduleConfig};
use weiback::sidecar::SpawnOptions;
use weiback::storage::database::create_db_pool_with_url;
use weiback::storage::internal::entities::{
    AccountDto, MonitoredUserDto, RefreshTier, SyncJobDto,
    enqueue_test_sync_job as enqueue_sync_job, get_sync_checkpoint, get_sync_job, get_sync_jobs,
    get_sync_run_history, save_account, save_monitored_user,
};
use weiback::sync_executor::{JobExecutor, WorkerRegistry};

fn python() -> Option<PathBuf> {
    [PathBuf::from("python"), PathBuf::from("python3")]
        .into_iter()
        .find(|path| {
            Command::new(path)
                .arg("--version")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|status| status.success())
                .unwrap_or(false)
        })
}

#[tokio::test]
async fn tick_enqueues_due_monitors_and_executes_one_persistent_job() {
    let Some(python) = python() else {
        eprintln!("python not found, skipping");
        return;
    };
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    let db = create_db_pool_with_url(":memory:").await.unwrap();
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("session.json"), "{}").unwrap();
    let account_id = save_account(
        &db,
        &AccountDto {
            id: 0,
            provider: "weibo".into(),
            uid: "100".into(),
            display_name: Some("scheduler".into()),
            session_ref: "session.json".into(),
            enabled: true,
            created_at: "2026-08-02T00:00:00Z".into(),
            updated_at: None,
        },
    )
    .await
    .unwrap();
    save_monitored_user(
        &db,
        &MonitoredUserDto {
            account_id,
            uid: 123,
            screen_name: Some("due".into()),
            refresh_strategy: "hot".into(),
            enabled: true,
            last_refreshed_at: None,
            created_at: "2026-08-02T00:00:00Z".into(),
            updated_at: None,
            tier: RefreshTier::Hot,
            interval_secs: 60,
            jitter_secs: 0,
            next_refresh_epoch: 10,
            last_refresh_epoch: None,
        },
    )
    .await
    .unwrap();
    let options = SpawnOptions {
        program: python,
        args: vec!["-u".into(), "-m".into(), "weiback_collector".into()],
        env: vec![
            (
                "PYTHONPATH".into(),
                root.join("sidecar").to_string_lossy().into(),
            ),
            ("PYTHONUTF8".into(), "1".into()),
            (
                "WEIBACK_COLLECTOR_FIXTURE".into(),
                "posts/long_text_full.jsonl".into(),
            ),
        ],
        cwd: Some(root),
        handshake_timeout: Duration::from_secs(5),
    };
    let scheduler = PersistentScheduler::new(
        db.clone(),
        JobExecutor::with_spawn_options(db.clone(), Arc::new(WorkerRegistry::new()), options),
        RefreshScheduleConfig::default(),
    );

    let summary = scheduler.tick_at(10).await.unwrap();

    assert_eq!(summary.refresh_enqueued, 1);
    assert_eq!(summary.executed.as_ref().unwrap().status, "completed");
    let jobs = get_sync_jobs(&db).await.unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].status, "completed");
    assert_eq!(
        get_sync_run_history(&db, jobs[0].id, 10)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        get_sync_checkpoint(&db, &format!("account:{account_id}:user:123:posts"))
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        get_sync_checkpoint(&db, "user:123:posts")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn run_until_cancelled_exits_promptly_without_another_claim() {
    let db = create_db_pool_with_url(":memory:").await.unwrap();
    let scheduler = PersistentScheduler::new(
        db.clone(),
        JobExecutor::new(db.clone(), Arc::new(WorkerRegistry::new())),
        RefreshScheduleConfig::default(),
    );
    let cancelled = Arc::new(AtomicBool::new(false));
    let wake = Arc::new(tokio::sync::Notify::new());
    let task = tokio::spawn(scheduler.run_until_cancelled(
        Duration::from_secs(30),
        cancelled.clone(),
        wake.clone(),
    ));
    tokio::time::sleep(Duration::from_millis(100)).await;

    let job_id = enqueue_sync_job(
        &db,
        &SyncJobDto {
            id: 0,
            resource_key: "user:cancelled-scheduler:posts".into(),
            name: "cancelled scheduler".into(),
            kind: "collect_user_posts".into(),
            payload_json: Some(r#"{"uid":"123"}"#.into()),
            status: "pending".into(),
            priority: 1,
            schedule_config: None,
            enabled: true,
            recovery_count: 0,
            max_recovery_attempts: 3,
            available_at: None,
            available_at_epoch: 0,
            claimed_at: None,
            owner_token: None,
            lease_until_epoch: None,
            current_run_id: None,
            generation: 0,
            last_error: None,
            created_at: "2026-08-02T00:00:00Z".into(),
            updated_at: None,
            account_id: 1,
            endpoint_key: "__legacy__".into(),
            endpoint_gate_revision: 0,
            account_gate_revision: 0,
        },
    )
    .await
    .unwrap();
    cancelled.store(true, Ordering::Release);
    wake.notify_waiters();

    tokio::time::timeout(Duration::from_millis(500), task)
        .await
        .expect("scheduler cancellation was not prompt")
        .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        get_sync_job(&db, job_id).await.unwrap().unwrap().status,
        "pending"
    );
}
