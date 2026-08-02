use std::{path::PathBuf, process::Command, sync::Arc, time::Duration};

use serde_json::json;
use tempfile::tempdir;
use weiback::sidecar::SpawnOptions;
use weiback::storage::database::create_db_pool_with_url;
use weiback::storage::internal::entities::{
    SyncJobDto, enqueue_test_sync_job as enqueue_sync_job, get_sync_checkpoint, get_sync_job,
    get_sync_run_history, resume_sync_job,
};
use weiback::sync_executor::{
    ActivationBarrier, ControlStopResult, FinalizationBarrier, JobExecutor, WorkerRegistry,
};

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

fn process_is_alive(pid: u32) -> bool {
    let output = Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .output()
        .expect("tasklist should run");
    String::from_utf8_lossy(&output.stdout).contains(&pid.to_string())
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn job(resource_key: &str) -> SyncJobDto {
    SyncJobDto {
        id: 0,
        resource_key: resource_key.into(),
        name: "fixture executor".into(),
        kind: "collect_user_posts".into(),
        payload_json: Some(json!({"uid":"123","max_pages":10}).to_string()),
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
        created_at: now(),
        updated_at: None,
        account_id: 1,
        endpoint_key: "__legacy__".into(),
        endpoint_gate_revision: 0,
        account_gate_revision: 0,
    }
}

fn scripted_options(python: PathBuf, script: &str) -> SpawnOptions {
    SpawnOptions {
        program: python,
        args: vec!["-u".into(), "-c".into(), script.into()],
        env: vec![("PYTHONUTF8".into(), "1".into())],
        cwd: None,
        handshake_timeout: Duration::from_secs(5),
    }
}

async fn wait_for_pid(registry: &WorkerRegistry, job_id: i64) -> u32 {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(pid) = registry.pid(job_id) {
                break pid;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn scheduler_claims_fixture_and_finishes_atomically() {
    let Some(py) = python() else {
        eprintln!("python not found, skipping");
        return;
    };
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    let options = SpawnOptions {
        program: py,
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
    let db = create_db_pool_with_url(":memory:").await.unwrap();
    let mut queued = job("user:1234567890:posts");
    queued.payload_json = Some(json!({"uid":"1234567890","max_pages":10}).to_string());
    let job_id = enqueue_sync_job(&db, &queued).await.unwrap();
    let registry = Arc::new(WorkerRegistry::new());
    let executor = JobExecutor::with_spawn_options(db.clone(), registry, options);

    let result = executor.run_next().await.unwrap().expect("one job");
    assert_eq!(result.job_id, job_id);
    assert_eq!(result.status, "completed");
    assert_eq!(
        get_sync_job(&db, job_id).await.unwrap().unwrap().status,
        "completed"
    );
    assert_eq!(get_sync_run_history(&db, job_id, 5).await.unwrap().len(), 1);
    assert!(
        get_sync_checkpoint(&db, "user:1234567890:posts")
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn control_without_worker_does_not_claim_complete_stop() {
    let db = create_db_pool_with_url(":memory:").await.unwrap();
    let job_id = enqueue_sync_job(&db, &job("user:no-worker:posts"))
        .await
        .unwrap();
    let executor = JobExecutor::new(db, Arc::new(WorkerRegistry::new()));

    let stopped = executor
        .pause(job_id, Duration::from_millis(200))
        .await
        .unwrap();
    assert_eq!(stopped, ControlStopResult::WorkerNotFound);
}

#[tokio::test]
async fn shutdown_before_worker_reservation_requeues_without_spawning_sidecar() {
    let db = create_db_pool_with_url(":memory:").await.unwrap();
    let job_id = enqueue_sync_job(&db, &job("user:shutdown-before-reserve:posts"))
        .await
        .unwrap();
    let registry = Arc::new(WorkerRegistry::new());
    registry.begin_shutdown();
    let executor = JobExecutor::with_spawn_options(
        db.clone(),
        registry,
        SpawnOptions {
            program: PathBuf::from("must-not-be-spawned.exe"),
            ..SpawnOptions::default()
        },
    );

    let result = executor.run_next().await.unwrap().unwrap();
    assert_eq!(result.status, "pending");
    assert_eq!(
        get_sync_job(&db, job_id).await.unwrap().unwrap().status,
        "pending"
    );
    assert_eq!(
        get_sync_job(&db, job_id)
            .await
            .unwrap()
            .unwrap()
            .recovery_count,
        0
    );
    assert_eq!(
        get_sync_run_history(&db, job_id, 1).await.unwrap()[0].status,
        "interrupted"
    );
}

#[tokio::test]
async fn shutdown_during_handshake_requeues_without_consuming_recovery_budget() {
    let Some(py) = python() else {
        eprintln!("python not found, skipping");
        return;
    };
    let dir = tempdir().unwrap();
    let pid_path = dir.path().join("handshake-shutdown.pid");
    let script = format!(
        "import os,pathlib,time\npathlib.Path(r'{}').write_text(str(os.getpid()))\ntime.sleep(30)\n",
        pid_path.display()
    );
    let db = create_db_pool_with_url(":memory:").await.unwrap();
    let job_id = enqueue_sync_job(&db, &job("user:shutdown-handshake:posts"))
        .await
        .unwrap();
    let registry = Arc::new(WorkerRegistry::new());
    let executor = JobExecutor::with_spawn_options(
        db.clone(),
        registry.clone(),
        scripted_options(py, &script),
    );
    let running = tokio::spawn(async move { executor.run_next().await });
    tokio::time::timeout(Duration::from_secs(5), async {
        while !pid_path.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let pid: u32 = std::fs::read_to_string(&pid_path).unwrap().parse().unwrap();

    let stopping = registry.clone();
    let summary =
        tokio::task::spawn_blocking(move || stopping.shutdown_all(Duration::from_secs(3)))
            .await
            .unwrap();
    assert!(!summary.degraded());
    let result = running.await.unwrap().unwrap().unwrap();
    assert_eq!(result.status, "pending");
    let stored = get_sync_job(&db, job_id).await.unwrap().unwrap();
    assert_eq!(stored.status, "pending");
    assert_eq!(stored.recovery_count, 0);
    assert_eq!(
        get_sync_run_history(&db, job_id, 1).await.unwrap()[0].status,
        "interrupted"
    );
    assert!(
        !process_is_alive(pid),
        "PID {pid} must be reaped on shutdown"
    );
}

#[tokio::test]
async fn spawn_failure_atomically_fails_owned_job_and_run() {
    let db = create_db_pool_with_url(":memory:").await.unwrap();
    let job_id = enqueue_sync_job(&db, &job("user:spawn-failure:posts"))
        .await
        .unwrap();
    let executor = JobExecutor::with_spawn_options(
        db.clone(),
        Arc::new(WorkerRegistry::new()),
        SpawnOptions {
            program: PathBuf::from("definitely-missing-sidecar.exe"),
            ..SpawnOptions::default()
        },
    );
    assert!(executor.run_next().await.is_err());
    assert_eq!(
        get_sync_job(&db, job_id).await.unwrap().unwrap().status,
        "failed"
    );
    let run = get_sync_run_history(&db, job_id, 1)
        .await
        .unwrap()
        .remove(0);
    assert_eq!(run.status, "failed");
    assert!(run.finished_at.is_some());
}

#[tokio::test]
async fn worker_registry_is_cleared_when_finalization_fails() {
    let Some(py) = python() else {
        eprintln!("python not found, skipping");
        return;
    };
    let db = create_db_pool_with_url(":memory:").await.unwrap();
    let job_id = enqueue_sync_job(&db, &job("user:registry-cleanup:posts"))
        .await
        .unwrap();
    sqlx::query(
        "CREATE TRIGGER fail_finish BEFORE UPDATE OF status ON sync_jobs \
         WHEN NEW.status='completed' BEGIN SELECT RAISE(ABORT,'finish failed'); END",
    )
    .execute(&db)
    .await
    .unwrap();
    let script = concat!(
        "import json,sys\n",
        "print('{\"protocol_version\":1,\"request_id\":null,\"event_id\":\"019fbbd7-ea26-7b7c-b113-c89ac2792001\",\"type\":\"ready\",\"occurred_at\":\"2026-08-01T00:00:00Z\",\"payload\":{\"sidecar_name\":\"x\",\"sidecar_version\":\"1\",\"protocol_version\":1}}')\n",
        "print('{\"protocol_version\":1,\"request_id\":null,\"event_id\":\"019fbbd7-ea26-7b7c-b113-c89ac2792002\",\"type\":\"capabilities\",\"occurred_at\":\"2026-08-01T00:00:00Z\",\"payload\":{\"protocol_versions\":[1],\"commands\":[\"hello\",\"collect_user_posts\"]}}')\n",
        "sys.stdout.flush();sys.stdin.readline();cmd=json.loads(sys.stdin.readline());rid=cmd['request_id']\n",
        "base={'protocol_version':1,'request_id':rid,'stream':'user:registry-cleanup:posts','occurred_at':'2026-08-01T00:00:01Z'}\n",
        "print(json.dumps({**base,'event_id':'019fbbd7-ea26-7b7c-b113-c89ac2792003','type':'started','sequence':1,'payload':{}}))\n",
        "print(json.dumps({**base,'event_id':'019fbbd7-ea26-7b7c-b113-c89ac2792004','type':'checkpoint','sequence':2,'payload':{'cursor':{'max_id':'done'},'fetched_count':1}}))\n",
        "print(json.dumps({**base,'event_id':'019fbbd7-ea26-7b7c-b113-c89ac2792005','type':'done','sequence':3,'payload':{'status':'completed','fetched_count':1}}));sys.stdout.flush()\n",
    );
    let registry = Arc::new(WorkerRegistry::new());
    let executor =
        JobExecutor::with_spawn_options(db, registry.clone(), scripted_options(py, script));
    assert!(executor.run_next().await.is_err());
    assert!(!registry.contains(job_id));
}

#[tokio::test]
async fn pause_fences_then_kills_and_waits_for_pid_exit() {
    let Some(py) = python() else {
        eprintln!("python not found, skipping");
        return;
    };
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("executor.sqlite");
    let db = create_db_pool_with_url(db_path.to_str().unwrap())
        .await
        .unwrap();
    let job_id = enqueue_sync_job(&db, &job("user:slow:posts"))
        .await
        .unwrap();
    let script = concat!(
        "import json,sys,time\n",
        "print('{\"protocol_version\":1,\"request_id\":null,\"event_id\":\"019fbbd7-ea26-7b7c-b113-c89ac2789901\",\"type\":\"ready\",\"occurred_at\":\"2026-08-01T00:00:00Z\",\"payload\":{\"sidecar_name\":\"x\",\"sidecar_version\":\"1\",\"protocol_version\":1}}')\n",
        "print('{\"protocol_version\":1,\"request_id\":null,\"event_id\":\"019fbbd7-ea26-7b7c-b113-c89ac2789902\",\"type\":\"capabilities\",\"occurred_at\":\"2026-08-01T00:00:00Z\",\"payload\":{\"protocol_versions\":[1],\"commands\":[\"hello\",\"collect_user_posts\"]}}')\n",
        "sys.stdout.flush();sys.stdin.readline();cmd=json.loads(sys.stdin.readline());rid=cmd['request_id']\n",
        "print(json.dumps({'protocol_version':1,'request_id':rid,'event_id':'019fbbd7-ea26-7b7c-b113-c89ac2789903','type':'started','stream':'user:slow:posts','sequence':1,'occurred_at':'2026-08-01T00:00:01Z','payload':{}}));sys.stdout.flush();time.sleep(30)\n",
    );
    let options = scripted_options(py, script);
    let registry = Arc::new(WorkerRegistry::new());
    let executor = JobExecutor::with_spawn_options(db.clone(), registry.clone(), options);
    let running = tokio::spawn(async move { executor.run_next().await });

    let pid = wait_for_pid(&registry, job_id).await;
    let controller = JobExecutor::new(db.clone(), registry);
    let stopped = controller
        .pause(job_id, Duration::from_secs(3))
        .await
        .unwrap();
    assert_eq!(stopped, ControlStopResult::Stopped { pid });
    assert!(running.await.unwrap().unwrap().is_some());
    assert_eq!(
        get_sync_job(&db, job_id).await.unwrap().unwrap().status,
        "paused"
    );
}

#[tokio::test]
async fn pause_during_worker_starting_waits_for_activation_and_ack() {
    let Some(py) = python() else {
        eprintln!("python not found, skipping");
        return;
    };
    let db = create_db_pool_with_url(":memory:").await.unwrap();
    let job_id = enqueue_sync_job(&db, &job("user:starting-race:posts"))
        .await
        .unwrap();
    let script = concat!(
        "import json,sys,time\n",
        "print('{\"protocol_version\":1,\"request_id\":null,\"event_id\":\"019fbbd7-ea26-7b7c-b113-c89ac2789951\",\"type\":\"ready\",\"occurred_at\":\"2026-08-01T00:00:00Z\",\"payload\":{\"sidecar_name\":\"x\",\"sidecar_version\":\"1\",\"protocol_version\":1}}')\n",
        "print('{\"protocol_version\":1,\"request_id\":null,\"event_id\":\"019fbbd7-ea26-7b7c-b113-c89ac2789952\",\"type\":\"capabilities\",\"occurred_at\":\"2026-08-01T00:00:00Z\",\"payload\":{\"protocol_versions\":[1],\"commands\":[\"hello\",\"collect_user_posts\"]}}')\n",
        "sys.stdout.flush();sys.stdin.readline();cmd=json.loads(sys.stdin.readline());rid=cmd['request_id']\n",
        "print(json.dumps({'protocol_version':1,'request_id':rid,'event_id':'019fbbd7-ea26-7b7c-b113-c89ac2789953','type':'started','stream':'user:starting-race:posts','sequence':1,'occurred_at':'2026-08-01T00:00:01Z','payload':{}}));sys.stdout.flush();time.sleep(30)\n",
    );
    let registry = Arc::new(WorkerRegistry::new());
    let barrier = ActivationBarrier::new();
    let executor =
        JobExecutor::with_spawn_options(db.clone(), registry.clone(), scripted_options(py, script))
            .with_activation_barrier(barrier.clone());
    let running = tokio::spawn(async move { executor.run_next().await });
    let entered = barrier.clone();
    tokio::task::spawn_blocking(move || entered.wait_until_entered())
        .await
        .unwrap();

    let controller = JobExecutor::new(db.clone(), registry);
    let pause = tokio::spawn(async move { controller.pause(job_id, Duration::from_secs(3)).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !pause.is_finished(),
        "pause returned while worker was Starting"
    );
    barrier.release();

    assert!(matches!(
        pause.await.unwrap().unwrap(),
        ControlStopResult::Stopped { .. }
    ));
    assert_eq!(running.await.unwrap().unwrap().unwrap().status, "paused");
    assert_eq!(
        get_sync_job(&db, job_id).await.unwrap().unwrap().status,
        "paused"
    );
}

#[tokio::test]
async fn shutdown_all_waits_for_starting_worker_and_interrupt_ack() {
    let Some(py) = python() else {
        eprintln!("python not found, skipping");
        return;
    };
    let db = create_db_pool_with_url(":memory:").await.unwrap();
    let job_id = enqueue_sync_job(&db, &job("user:shutdown-starting:posts"))
        .await
        .unwrap();
    let script = concat!(
        "import json,sys,time\n",
        "print('{\"protocol_version\":1,\"request_id\":null,\"event_id\":\"019fbbd7-ea26-7b7c-b113-c89ac2789961\",\"type\":\"ready\",\"occurred_at\":\"2026-08-01T00:00:00Z\",\"payload\":{\"sidecar_name\":\"x\",\"sidecar_version\":\"1\",\"protocol_version\":1}}')\n",
        "print('{\"protocol_version\":1,\"request_id\":null,\"event_id\":\"019fbbd7-ea26-7b7c-b113-c89ac2789962\",\"type\":\"capabilities\",\"occurred_at\":\"2026-08-01T00:00:00Z\",\"payload\":{\"protocol_versions\":[1],\"commands\":[\"hello\",\"collect_user_posts\"]}}')\n",
        "sys.stdout.flush();sys.stdin.readline();cmd=json.loads(sys.stdin.readline());rid=cmd['request_id']\n",
        "print(json.dumps({'protocol_version':1,'request_id':rid,'event_id':'019fbbd7-ea26-7b7c-b113-c89ac2789963','type':'started','stream':'user:shutdown-starting:posts','sequence':1,'occurred_at':'2026-08-01T00:00:01Z','payload':{}}));sys.stdout.flush();time.sleep(30)\n",
    );
    let registry = Arc::new(WorkerRegistry::new());
    let barrier = ActivationBarrier::new();
    let executor =
        JobExecutor::with_spawn_options(db.clone(), registry.clone(), scripted_options(py, script))
            .with_activation_barrier(barrier.clone());
    let running = tokio::spawn(async move { executor.run_next().await });
    let entered = barrier.clone();
    tokio::task::spawn_blocking(move || entered.wait_until_entered())
        .await
        .unwrap();
    let stopping_registry = registry.clone();
    let shutdown =
        tokio::task::spawn_blocking(move || stopping_registry.shutdown_all(Duration::from_secs(3)));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!shutdown.is_finished());
    barrier.release();

    let summary = shutdown.await.unwrap();
    assert_eq!(summary.workers.len(), 1);
    assert_eq!(
        summary.workers[0].worker_stop,
        ControlStopResult::WorkerNotFound
    );
    assert!(!summary.degraded());
    assert_eq!(running.await.unwrap().unwrap().unwrap().status, "pending");
    assert_eq!(
        get_sync_job(&db, job_id).await.unwrap().unwrap().status,
        "pending"
    );
    assert_eq!(
        get_sync_run_history(&db, job_id, 1).await.unwrap()[0].status,
        "interrupted"
    );
    assert!(!registry.contains(job_id));
}

#[tokio::test]
async fn shutdown_interrupts_running_job_and_reaps_sidecar_without_cancelling_job() {
    let Some(py) = python() else {
        eprintln!("python not found, skipping");
        return;
    };
    let db = create_db_pool_with_url(":memory:").await.unwrap();
    let job_id = enqueue_sync_job(&db, &job("user:shutdown-interrupt:posts"))
        .await
        .unwrap();
    let script = concat!(
        "import json,sys,time\n",
        "print('{\"protocol_version\":1,\"request_id\":null,\"event_id\":\"019fbbd7-ea26-7b7c-b113-c89ac2789981\",\"type\":\"ready\",\"occurred_at\":\"2026-08-01T00:00:00Z\",\"payload\":{\"sidecar_name\":\"x\",\"sidecar_version\":\"1\",\"protocol_version\":1}}')\n",
        "print('{\"protocol_version\":1,\"request_id\":null,\"event_id\":\"019fbbd7-ea26-7b7c-b113-c89ac2789982\",\"type\":\"capabilities\",\"occurred_at\":\"2026-08-01T00:00:00Z\",\"payload\":{\"protocol_versions\":[1],\"commands\":[\"hello\",\"collect_user_posts\"]}}')\n",
        "sys.stdout.flush();sys.stdin.readline();cmd=json.loads(sys.stdin.readline());rid=cmd['request_id']\n",
        "print(json.dumps({'protocol_version':1,'request_id':rid,'event_id':'019fbbd7-ea26-7b7c-b113-c89ac2789983','type':'started','stream':'user:shutdown-interrupt:posts','sequence':1,'occurred_at':'2026-08-01T00:00:01Z','payload':{}}));sys.stdout.flush();time.sleep(30)\n",
    );
    let registry = Arc::new(WorkerRegistry::new());
    let executor =
        JobExecutor::with_spawn_options(db.clone(), registry.clone(), scripted_options(py, script));
    let running = tokio::spawn(async move { executor.run_next().await });
    let pid = wait_for_pid(&registry, job_id).await;

    let stopping = registry.clone();
    let summary =
        tokio::task::spawn_blocking(move || stopping.shutdown_all(Duration::from_secs(3)))
            .await
            .unwrap();

    assert_eq!(
        summary.workers[0].worker_stop,
        ControlStopResult::Stopped { pid }
    );
    assert!(!summary.degraded());
    assert_eq!(running.await.unwrap().unwrap().unwrap().status, "pending");
    let stored = get_sync_job(&db, job_id).await.unwrap().unwrap();
    assert_eq!(stored.status, "pending");
    assert_ne!(stored.status, "cancelled");
    assert_eq!(stored.recovery_count, 0);
    assert_eq!(
        get_sync_run_history(&db, job_id, 1).await.unwrap()[0].status,
        "interrupted"
    );
}

#[tokio::test]
async fn starting_worker_cannot_activate_after_shutdown_timeout() {
    let Some(py) = python() else {
        eprintln!("python not found, skipping");
        return;
    };
    let db = create_db_pool_with_url(":memory:").await.unwrap();
    let job_id = enqueue_sync_job(&db, &job("user:shutdown-timeout:posts"))
        .await
        .unwrap();
    let script = concat!(
        "import json,sys,time\n",
        "print('{\"protocol_version\":1,\"request_id\":null,\"event_id\":\"019fbbd7-ea26-7b7c-b113-c89ac2789971\",\"type\":\"ready\",\"occurred_at\":\"2026-08-01T00:00:00Z\",\"payload\":{\"sidecar_name\":\"x\",\"sidecar_version\":\"1\",\"protocol_version\":1}}')\n",
        "print('{\"protocol_version\":1,\"request_id\":null,\"event_id\":\"019fbbd7-ea26-7b7c-b113-c89ac2789972\",\"type\":\"capabilities\",\"occurred_at\":\"2026-08-01T00:00:00Z\",\"payload\":{\"protocol_versions\":[1],\"commands\":[\"hello\",\"collect_user_posts\"]}}')\n",
        "sys.stdout.flush();sys.stdin.readline();time.sleep(30)\n",
    );
    let registry = Arc::new(WorkerRegistry::new());
    let barrier = ActivationBarrier::new();
    let executor =
        JobExecutor::with_spawn_options(db.clone(), registry.clone(), scripted_options(py, script))
            .with_activation_barrier(barrier.clone());
    let running = tokio::spawn(async move { executor.run_next().await });
    let entered = barrier.clone();
    tokio::task::spawn_blocking(move || entered.wait_until_entered())
        .await
        .unwrap();
    let stopping = registry.clone();
    let summary =
        tokio::task::spawn_blocking(move || stopping.shutdown_all(Duration::from_millis(50)))
            .await
            .unwrap();
    assert_eq!(
        summary.workers[0].worker_stop,
        ControlStopResult::WorkerStarting
    );
    barrier.release();
    assert_eq!(running.await.unwrap().unwrap().unwrap().status, "pending");
    assert_eq!(
        get_sync_job(&db, job_id).await.unwrap().unwrap().status,
        "pending"
    );
    assert!(!registry.contains(job_id));
}

#[tokio::test]
async fn pause_during_half_page_rolls_back_uncheckpointed_rows() {
    let Some(py) = python() else {
        eprintln!("python not found, skipping");
        return;
    };
    let db = create_db_pool_with_url(":memory:").await.unwrap();
    let job_id = enqueue_sync_job(&db, &job("user:half-page:posts"))
        .await
        .unwrap();
    let script = concat!(
        "import json,sys,time\n",
        "print('{\"protocol_version\":1,\"request_id\":null,\"event_id\":\"019fbbd7-ea26-7b7c-b113-c89ac2790001\",\"type\":\"ready\",\"occurred_at\":\"2026-08-01T00:00:00Z\",\"payload\":{\"sidecar_name\":\"x\",\"sidecar_version\":\"1\",\"protocol_version\":1}}')\n",
        "print('{\"protocol_version\":1,\"request_id\":null,\"event_id\":\"019fbbd7-ea26-7b7c-b113-c89ac2790002\",\"type\":\"capabilities\",\"occurred_at\":\"2026-08-01T00:00:00Z\",\"payload\":{\"protocol_versions\":[1],\"commands\":[\"hello\",\"collect_user_posts\"]}}')\n",
        "sys.stdout.flush();sys.stdin.readline();cmd=json.loads(sys.stdin.readline());rid=cmd['request_id'];base={'protocol_version':1,'request_id':rid,'stream':'user:half-page:posts','occurred_at':'2026-08-01T00:00:01Z'}\n",
        "print(json.dumps({**base,'event_id':'019fbbd7-ea26-7b7c-b113-c89ac2790003','type':'started','sequence':1,'payload':{}}))\n",
        "print(json.dumps({**base,'event_id':'019fbbd7-ea26-7b7c-b113-c89ac2790004','type':'post','sequence':2,'payload':{'id':'88001','uid':'1','text':'half page'}}));sys.stdout.flush();time.sleep(30)\n",
    );
    let registry = Arc::new(WorkerRegistry::new());
    let executor =
        JobExecutor::with_spawn_options(db.clone(), registry.clone(), scripted_options(py, script));
    let running = tokio::spawn(async move { executor.run_next().await });
    wait_for_pid(&registry, job_id).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    let controller = JobExecutor::new(db.clone(), registry);
    assert!(matches!(
        controller
            .pause(job_id, Duration::from_secs(3))
            .await
            .unwrap(),
        ControlStopResult::Stopped { .. }
    ));
    running.await.unwrap().unwrap();
    let posts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM posts WHERE id=88001")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(posts, 0);
    assert!(
        get_sync_checkpoint(&db, "user:half-page:posts")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn pause_checkpoint_race_commits_whole_page_or_rolls_it_all_back() {
    let Some(py) = python() else {
        eprintln!("python not found, skipping");
        return;
    };
    for cycle in 0..20 {
        let db = create_db_pool_with_url(":memory:").await.unwrap();
        let stream = format!("user:checkpoint-race-{cycle}:posts");
        let mut queued = job(&stream);
        queued.payload_json = Some(json!({"uid":format!("checkpoint-race-{cycle}")}).to_string());
        let job_id = enqueue_sync_job(&db, &queued).await.unwrap();
        let script = format!(
            "import json,sys,time\nprint('{{\"protocol_version\":1,\"request_id\":null,\"event_id\":\"019fbbd7-ea26-7b7c-b113-c89ac2790101\",\"type\":\"ready\",\"occurred_at\":\"2026-08-01T00:00:00Z\",\"payload\":{{\"sidecar_name\":\"x\",\"sidecar_version\":\"1\",\"protocol_version\":1}}}}')\nprint('{{\"protocol_version\":1,\"request_id\":null,\"event_id\":\"019fbbd7-ea26-7b7c-b113-c89ac2790102\",\"type\":\"capabilities\",\"occurred_at\":\"2026-08-01T00:00:00Z\",\"payload\":{{\"protocol_versions\":[1],\"commands\":[\"hello\",\"collect_user_posts\"]}}}}')\nsys.stdout.flush();sys.stdin.readline();cmd=json.loads(sys.stdin.readline());rid=cmd['request_id'];base={{'protocol_version':1,'request_id':rid,'stream':'{stream}','occurred_at':'2026-08-01T00:00:01Z'}}\nprint(json.dumps({{**base,'event_id':'019fbbd7-ea26-7b7c-b113-c89ac2790103','type':'started','sequence':1,'payload':{{}}}}))\nprint(json.dumps({{**base,'event_id':'019fbbd7-ea26-7b7c-b113-c89ac2790104','type':'post','sequence':2,'payload':{{'id':'{}','uid':'1','text':'race'}}}}));sys.stdout.flush();time.sleep(0.05)\nprint(json.dumps({{**base,'event_id':'019fbbd7-ea26-7b7c-b113-c89ac2790105','type':'checkpoint','sequence':3,'payload':{{'cursor':{{'max_id':'p2','max_id_type':0}},'fetched_count':20}}}}));sys.stdout.flush();time.sleep(30)\n",
            89000 + cycle
        );
        let registry = Arc::new(WorkerRegistry::new());
        let executor = JobExecutor::with_spawn_options(
            db.clone(),
            registry.clone(),
            scripted_options(py.clone(), &script),
        );
        let running = tokio::spawn(async move { executor.run_next().await });
        wait_for_pid(&registry, job_id).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        JobExecutor::new(db.clone(), registry)
            .pause(job_id, Duration::from_secs(3))
            .await
            .unwrap();
        running.await.unwrap().unwrap();
        let posts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM posts WHERE id=?")
            .bind(89000 + cycle)
            .fetch_one(&db)
            .await
            .unwrap();
        let checkpoint = get_sync_checkpoint(&db, &stream).await.unwrap();
        assert_eq!(posts == 1, checkpoint.is_some(), "cycle {cycle}");
    }
}

#[tokio::test]
async fn checkpoint_then_pause_resume_starts_new_run_from_saved_cursor() {
    let Some(py) = python() else {
        eprintln!("python not found, skipping");
        return;
    };
    let db = create_db_pool_with_url(":memory:").await.unwrap();
    let job_id = enqueue_sync_job(&db, &job("user:resume-worker:posts"))
        .await
        .unwrap();
    let first_script = concat!(
        "import json,sys,time\n",
        "print('{\"protocol_version\":1,\"request_id\":null,\"event_id\":\"019fbbd7-ea26-7b7c-b113-c89ac2790201\",\"type\":\"ready\",\"occurred_at\":\"2026-08-01T00:00:00Z\",\"payload\":{\"sidecar_name\":\"x\",\"sidecar_version\":\"1\",\"protocol_version\":1}}')\n",
        "print('{\"protocol_version\":1,\"request_id\":null,\"event_id\":\"019fbbd7-ea26-7b7c-b113-c89ac2790202\",\"type\":\"capabilities\",\"occurred_at\":\"2026-08-01T00:00:00Z\",\"payload\":{\"protocol_versions\":[1],\"commands\":[\"hello\",\"collect_user_posts\"]}}')\n",
        "sys.stdout.flush();sys.stdin.readline();cmd=json.loads(sys.stdin.readline());rid=cmd['request_id'];base={'protocol_version':1,'request_id':rid,'stream':'user:resume-worker:posts','occurred_at':'2026-08-01T00:00:01Z'}\n",
        "print(json.dumps({**base,'event_id':'019fbbd7-ea26-7b7c-b113-c89ac2790203','type':'started','sequence':1,'payload':{}}))\n",
        "print(json.dumps({**base,'event_id':'019fbbd7-ea26-7b7c-b113-c89ac2790204','type':'post','sequence':2,'payload':{'id':'90001','uid':'1','text':'page one'}}))\n",
        "print(json.dumps({**base,'event_id':'019fbbd7-ea26-7b7c-b113-c89ac2790205','type':'checkpoint','sequence':3,'payload':{'cursor':{'max_id':'saved-cursor','max_id_type':0},'fetched_count':20}}));sys.stdout.flush();time.sleep(30)\n",
    );
    let first_registry = Arc::new(WorkerRegistry::new());
    let first_executor = JobExecutor::with_spawn_options(
        db.clone(),
        first_registry.clone(),
        scripted_options(py.clone(), first_script),
    );
    let first = tokio::spawn(async move { first_executor.run_next().await });
    wait_for_pid(&first_registry, job_id).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if get_sync_checkpoint(&db, "user:resume-worker:posts")
                .await
                .unwrap()
                .is_some()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    JobExecutor::new(db.clone(), first_registry)
        .pause(job_id, Duration::from_secs(3))
        .await
        .unwrap();
    first.await.unwrap().unwrap();
    let first_run = get_sync_run_history(&db, job_id, 5).await.unwrap()[0].id;
    resume_sync_job(&db, job_id, &now()).await.unwrap();

    let second_script = concat!(
        "import json,sys\n",
        "print('{\"protocol_version\":1,\"request_id\":null,\"event_id\":\"019fbbd7-ea26-7b7c-b113-c89ac2790211\",\"type\":\"ready\",\"occurred_at\":\"2026-08-01T00:00:00Z\",\"payload\":{\"sidecar_name\":\"x\",\"sidecar_version\":\"1\",\"protocol_version\":1}}')\n",
        "print('{\"protocol_version\":1,\"request_id\":null,\"event_id\":\"019fbbd7-ea26-7b7c-b113-c89ac2790212\",\"type\":\"capabilities\",\"occurred_at\":\"2026-08-01T00:00:00Z\",\"payload\":{\"protocol_versions\":[1],\"commands\":[\"hello\",\"collect_user_posts\"]}}')\n",
        "sys.stdout.flush();sys.stdin.readline();cmd=json.loads(sys.stdin.readline());assert cmd['payload']['checkpoint']['max_id']=='saved-cursor';rid=cmd['request_id'];base={'protocol_version':1,'request_id':rid,'stream':'user:resume-worker:posts','occurred_at':'2026-08-01T00:00:02Z'}\n",
        "print(json.dumps({**base,'event_id':'019fbbd7-ea26-7b7c-b113-c89ac2790213','type':'started','sequence':1,'payload':{}}))\n",
        "print(json.dumps({**base,'event_id':'019fbbd7-ea26-7b7c-b113-c89ac2790214','type':'post','sequence':2,'payload':{'id':'90002','uid':'1','text':'page two'}}))\n",
        "print(json.dumps({**base,'event_id':'019fbbd7-ea26-7b7c-b113-c89ac2790215','type':'checkpoint','sequence':3,'payload':{'cursor':{'max_id':'finished','max_id_type':0},'fetched_count':40}}))\n",
        "print(json.dumps({**base,'event_id':'019fbbd7-ea26-7b7c-b113-c89ac2790216','type':'done','sequence':4,'payload':{'status':'completed','fetched_count':40}}));sys.stdout.flush()\n",
    );
    let second = JobExecutor::with_spawn_options(
        db.clone(),
        Arc::new(WorkerRegistry::new()),
        scripted_options(py, second_script),
    )
    .run_next()
    .await
    .unwrap()
    .unwrap();
    assert_eq!(second.status, "completed");
    assert_ne!(second.run_id, first_run);
    assert_eq!(get_sync_run_history(&db, job_id, 5).await.unwrap().len(), 2);
    let posts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM posts WHERE id IN (90001,90002)")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(posts, 2);
}

#[tokio::test]
async fn real_sidecar_exit_recovers_only_its_job_then_next_run_resumes_checkpoint() {
    let Some(py) = python() else {
        eprintln!("python not found, skipping");
        return;
    };
    let db = create_db_pool_with_url(":memory:").await.unwrap();
    let mut queued = job("user:crash-recovery:posts");
    queued.max_recovery_attempts = 2;
    let job_id = enqueue_sync_job(&db, &queued).await.unwrap();
    let unrelated_id = enqueue_sync_job(&db, &job("user:unrelated-interrupted:posts"))
        .await
        .unwrap();
    sqlx::query("UPDATE sync_jobs SET status='interrupted', recovery_count=1 WHERE id=?")
        .bind(unrelated_id)
        .execute(&db)
        .await
        .unwrap();

    let crash_script = concat!(
        "import json,sys\n",
        "print('{\"protocol_version\":1,\"request_id\":null,\"event_id\":\"019fbbd7-ea26-7b7c-b113-c89ac2790301\",\"type\":\"ready\",\"occurred_at\":\"2026-08-01T00:00:00Z\",\"payload\":{\"sidecar_name\":\"x\",\"sidecar_version\":\"1\",\"protocol_version\":1}}')\n",
        "print('{\"protocol_version\":1,\"request_id\":null,\"event_id\":\"019fbbd7-ea26-7b7c-b113-c89ac2790302\",\"type\":\"capabilities\",\"occurred_at\":\"2026-08-01T00:00:00Z\",\"payload\":{\"protocol_versions\":[1],\"commands\":[\"hello\",\"collect_user_posts\"]}}')\n",
        "sys.stdout.flush();sys.stdin.readline();cmd=json.loads(sys.stdin.readline());rid=cmd['request_id'];base={'protocol_version':1,'request_id':rid,'stream':'user:crash-recovery:posts','occurred_at':'2026-08-01T00:00:01Z'}\n",
        "print(json.dumps({**base,'event_id':'019fbbd7-ea26-7b7c-b113-c89ac2790303','type':'started','sequence':1,'payload':{}}))\n",
        "print(json.dumps({**base,'event_id':'019fbbd7-ea26-7b7c-b113-c89ac2790304','type':'post','sequence':2,'payload':{'id':'91001','uid':'1','text':'committed before crash'}}))\n",
        "print(json.dumps({**base,'event_id':'019fbbd7-ea26-7b7c-b113-c89ac2790305','type':'checkpoint','sequence':3,'payload':{'cursor':{'max_id':'crash-cursor','max_id_type':0},'fetched_count':20}}));sys.stdout.flush();sys.exit(7)\n",
    );
    let first = JobExecutor::with_spawn_options(
        db.clone(),
        Arc::new(WorkerRegistry::new()),
        scripted_options(py.clone(), crash_script),
    )
    .run_next()
    .await
    .unwrap()
    .unwrap();
    assert_eq!(first.status, "pending");
    let recovered = get_sync_job(&db, job_id).await.unwrap().unwrap();
    assert_eq!(recovered.recovery_count, 1);
    assert_eq!(
        get_sync_job(&db, unrelated_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        "interrupted"
    );
    assert_eq!(
        get_sync_job(&db, unrelated_id)
            .await
            .unwrap()
            .unwrap()
            .recovery_count,
        1
    );
    assert!(
        get_sync_checkpoint(&db, "user:crash-recovery:posts")
            .await
            .unwrap()
            .is_some()
    );

    let resume_script = concat!(
        "import json,sys\n",
        "print('{\"protocol_version\":1,\"request_id\":null,\"event_id\":\"019fbbd7-ea26-7b7c-b113-c89ac2790311\",\"type\":\"ready\",\"occurred_at\":\"2026-08-01T00:00:00Z\",\"payload\":{\"sidecar_name\":\"x\",\"sidecar_version\":\"1\",\"protocol_version\":1}}')\n",
        "print('{\"protocol_version\":1,\"request_id\":null,\"event_id\":\"019fbbd7-ea26-7b7c-b113-c89ac2790312\",\"type\":\"capabilities\",\"occurred_at\":\"2026-08-01T00:00:00Z\",\"payload\":{\"protocol_versions\":[1],\"commands\":[\"hello\",\"collect_user_posts\"]}}')\n",
        "sys.stdout.flush();sys.stdin.readline();cmd=json.loads(sys.stdin.readline());assert cmd['payload']['checkpoint']['max_id']=='crash-cursor';rid=cmd['request_id'];base={'protocol_version':1,'request_id':rid,'stream':'user:crash-recovery:posts','occurred_at':'2026-08-01T00:00:02Z'}\n",
        "print(json.dumps({**base,'event_id':'019fbbd7-ea26-7b7c-b113-c89ac2790313','type':'started','sequence':1,'payload':{}}))\n",
        "print(json.dumps({**base,'event_id':'019fbbd7-ea26-7b7c-b113-c89ac2790314','type':'checkpoint','sequence':2,'payload':{'cursor':{'max_id':'done','max_id_type':0},'fetched_count':40}}))\n",
        "print(json.dumps({**base,'event_id':'019fbbd7-ea26-7b7c-b113-c89ac2790315','type':'done','sequence':3,'payload':{'status':'completed','fetched_count':40}}));sys.stdout.flush()\n",
    );
    let second = JobExecutor::with_spawn_options(
        db.clone(),
        Arc::new(WorkerRegistry::new()),
        scripted_options(py, resume_script),
    )
    .run_next()
    .await
    .unwrap()
    .unwrap();
    assert_eq!(second.job_id, job_id);
    assert_eq!(second.status, "completed");
    assert_eq!(
        get_sync_run_history(&db, job_id, 10).await.unwrap().len(),
        2
    );
}

#[tokio::test]
async fn real_sidecar_exit_at_recovery_limit_fails_exactly_once() {
    let Some(py) = python() else {
        eprintln!("python not found, skipping");
        return;
    };
    let db = create_db_pool_with_url(":memory:").await.unwrap();
    let mut queued = job("user:crash-limit:posts");
    queued.max_recovery_attempts = 1;
    let job_id = enqueue_sync_job(&db, &queued).await.unwrap();
    let script = concat!(
        "import json,sys\n",
        "print('{\"protocol_version\":1,\"request_id\":null,\"event_id\":\"019fbbd7-ea26-7b7c-b113-c89ac2790351\",\"type\":\"ready\",\"occurred_at\":\"2026-08-01T00:00:00Z\",\"payload\":{\"sidecar_name\":\"x\",\"sidecar_version\":\"1\",\"protocol_version\":1}}')\n",
        "print('{\"protocol_version\":1,\"request_id\":null,\"event_id\":\"019fbbd7-ea26-7b7c-b113-c89ac2790352\",\"type\":\"capabilities\",\"occurred_at\":\"2026-08-01T00:00:00Z\",\"payload\":{\"protocol_versions\":[1],\"commands\":[\"hello\",\"collect_user_posts\"]}}')\n",
        "sys.stdout.flush();sys.stdin.readline();sys.exit(9)\n",
    );
    let result = JobExecutor::with_spawn_options(
        db.clone(),
        Arc::new(WorkerRegistry::new()),
        scripted_options(py, script),
    )
    .run_next()
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result.status, "failed");
    let stored = get_sync_job(&db, job_id).await.unwrap().unwrap();
    assert_eq!(stored.status, "failed");
    assert_eq!(stored.recovery_count, 1);
}

#[tokio::test]
async fn exit_after_post_before_checkpoint_rolls_back_then_successful_replacement_completes() {
    let Some(py) = python() else {
        eprintln!("python not found, skipping");
        return;
    };
    let db = create_db_pool_with_url(":memory:").await.unwrap();
    let stream = "user:post-before-checkpoint:posts";
    let job_id = enqueue_sync_job(&db, &job(stream)).await.unwrap();
    let crash_script = concat!(
        "import json,sys\n",
        "print('{\"protocol_version\":1,\"request_id\":null,\"event_id\":\"019fbbd7-ea26-7b7c-b113-c89ac2790501\",\"type\":\"ready\",\"occurred_at\":\"2026-08-01T00:00:00Z\",\"payload\":{\"sidecar_name\":\"x\",\"sidecar_version\":\"1\",\"protocol_version\":1}}')\n",
        "print('{\"protocol_version\":1,\"request_id\":null,\"event_id\":\"019fbbd7-ea26-7b7c-b113-c89ac2790502\",\"type\":\"capabilities\",\"occurred_at\":\"2026-08-01T00:00:00Z\",\"payload\":{\"protocol_versions\":[1],\"commands\":[\"hello\",\"collect_user_posts\"]}}')\n",
        "sys.stdout.flush();sys.stdin.readline();cmd=json.loads(sys.stdin.readline());rid=cmd['request_id'];base={'protocol_version':1,'request_id':rid,'stream':'user:post-before-checkpoint:posts','occurred_at':'2026-08-01T00:00:01Z'}\n",
        "print(json.dumps({**base,'event_id':'019fbbd7-ea26-7b7c-b113-c89ac2790503','type':'started','sequence':1,'payload':{}}))\n",
        "print(json.dumps({**base,'event_id':'019fbbd7-ea26-7b7c-b113-c89ac2790504','type':'post','sequence':2,'payload':{'id':'92001','uid':'1','text':'must not commit'}}));sys.stdout.flush();sys.exit(8)\n",
    );
    let first = JobExecutor::with_spawn_options(
        db.clone(),
        Arc::new(WorkerRegistry::new()),
        scripted_options(py.clone(), crash_script),
    )
    .run_next()
    .await
    .unwrap()
    .unwrap();
    assert_eq!(first.status, "pending");
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM posts WHERE id=92001")
            .fetch_one(&db)
            .await
            .unwrap(),
        0
    );
    assert!(get_sync_checkpoint(&db, stream).await.unwrap().is_none());
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM processed_events WHERE event_id='019fbbd7-ea26-7b7c-b113-c89ac2790504'",
        )
        .fetch_one(&db)
        .await
        .unwrap(),
        0
    );
    let recovered = get_sync_job(&db, job_id).await.unwrap().unwrap();
    assert_eq!(
        (recovered.status.as_str(), recovered.recovery_count),
        ("pending", 1)
    );

    let success_script = concat!(
        "import json,sys\n",
        "print('{\"protocol_version\":1,\"request_id\":null,\"event_id\":\"019fbbd7-ea26-7b7c-b113-c89ac2790511\",\"type\":\"ready\",\"occurred_at\":\"2026-08-01T00:00:00Z\",\"payload\":{\"sidecar_name\":\"x\",\"sidecar_version\":\"1\",\"protocol_version\":1}}')\n",
        "print('{\"protocol_version\":1,\"request_id\":null,\"event_id\":\"019fbbd7-ea26-7b7c-b113-c89ac2790512\",\"type\":\"capabilities\",\"occurred_at\":\"2026-08-01T00:00:00Z\",\"payload\":{\"protocol_versions\":[1],\"commands\":[\"hello\",\"collect_user_posts\"]}}')\n",
        "sys.stdout.flush();sys.stdin.readline();cmd=json.loads(sys.stdin.readline());rid=cmd['request_id'];base={'protocol_version':1,'request_id':rid,'stream':'user:post-before-checkpoint:posts','occurred_at':'2026-08-01T00:00:02Z'}\n",
        "print(json.dumps({**base,'event_id':'019fbbd7-ea26-7b7c-b113-c89ac2790513','type':'started','sequence':1,'payload':{}}))\n",
        "print(json.dumps({**base,'event_id':'019fbbd7-ea26-7b7c-b113-c89ac2790514','type':'post','sequence':2,'payload':{'id':'92001','uid':'1','text':'committed retry'}}))\n",
        "print(json.dumps({**base,'event_id':'019fbbd7-ea26-7b7c-b113-c89ac2790515','type':'checkpoint','sequence':3,'payload':{'cursor':{'max_id':'done','max_id_type':0},'fetched_count':1}}))\n",
        "print(json.dumps({**base,'event_id':'019fbbd7-ea26-7b7c-b113-c89ac2790516','type':'done','sequence':4,'payload':{'status':'completed','fetched_count':1}}));sys.stdout.flush()\n",
    );
    let second = JobExecutor::with_spawn_options(
        db.clone(),
        Arc::new(WorkerRegistry::new()),
        scripted_options(py, success_script),
    )
    .run_next()
    .await
    .unwrap()
    .unwrap();
    assert_eq!(second.status, "completed");
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM posts WHERE id=92001")
            .fetch_one(&db)
            .await
            .unwrap(),
        1
    );
    assert!(get_sync_checkpoint(&db, stream).await.unwrap().is_some());
}

#[tokio::test]
async fn pause_wins_done_to_finish_window_without_worker_lost_ownership_error() {
    let Some(py) = python() else {
        eprintln!("python not found, skipping");
        return;
    };
    let db = create_db_pool_with_url(":memory:").await.unwrap();
    let job_id = enqueue_sync_job(&db, &job("user:done-finish-race:posts"))
        .await
        .unwrap();
    let script = concat!(
        "import json,sys\n",
        "print('{\"protocol_version\":1,\"request_id\":null,\"event_id\":\"019fbbd7-ea26-7b7c-b113-c89ac2790401\",\"type\":\"ready\",\"occurred_at\":\"2026-08-01T00:00:00Z\",\"payload\":{\"sidecar_name\":\"x\",\"sidecar_version\":\"1\",\"protocol_version\":1}}')\n",
        "print('{\"protocol_version\":1,\"request_id\":null,\"event_id\":\"019fbbd7-ea26-7b7c-b113-c89ac2790402\",\"type\":\"capabilities\",\"occurred_at\":\"2026-08-01T00:00:00Z\",\"payload\":{\"protocol_versions\":[1],\"commands\":[\"hello\",\"collect_user_posts\"]}}')\n",
        "sys.stdout.flush();sys.stdin.readline();cmd=json.loads(sys.stdin.readline());rid=cmd['request_id'];base={'protocol_version':1,'request_id':rid,'stream':'user:done-finish-race:posts','occurred_at':'2026-08-01T00:00:01Z'}\n",
        "print(json.dumps({**base,'event_id':'019fbbd7-ea26-7b7c-b113-c89ac2790403','type':'started','sequence':1,'payload':{}}))\n",
        "print(json.dumps({**base,'event_id':'019fbbd7-ea26-7b7c-b113-c89ac2790404','type':'checkpoint','sequence':2,'payload':{'cursor':{'max_id':'done','max_id_type':0},'fetched_count':20}}))\n",
        "print(json.dumps({**base,'event_id':'019fbbd7-ea26-7b7c-b113-c89ac2790405','type':'done','sequence':3,'payload':{'status':'completed','fetched_count':20}}));sys.stdout.flush()\n",
    );
    let registry = Arc::new(WorkerRegistry::new());
    let barrier = FinalizationBarrier::new();
    let executor =
        JobExecutor::with_spawn_options(db.clone(), registry.clone(), scripted_options(py, script))
            .with_finalization_barrier(barrier.clone());
    let running = tokio::spawn(async move { executor.run_next().await });

    let entered = barrier.clone();
    tokio::task::spawn_blocking(move || entered.wait_until_entered())
        .await
        .unwrap();
    let controller = JobExecutor::new(db.clone(), registry);
    let pause = tokio::spawn(async move { controller.pause(job_id, Duration::from_secs(3)).await });
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if get_sync_job(&db, job_id).await.unwrap().unwrap().status == "paused" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let release = barrier.clone();
    tokio::task::spawn_blocking(move || release.release())
        .await
        .unwrap();

    assert!(matches!(
        pause.await.unwrap().unwrap(),
        ControlStopResult::Stopped { .. }
    ));
    let result = running
        .await
        .unwrap()
        .expect("worker must not report lost ownership")
        .unwrap();
    assert_eq!(result.status, "paused");
    assert_eq!(
        get_sync_job(&db, job_id).await.unwrap().unwrap().status,
        "paused"
    );
}

fn checkpoint_then_failure_script(stream: &str, event_type: &str, code: &str) -> String {
    format!(
        r#"import json,sys
print('{{"protocol_version":1,"request_id":null,"event_id":"019fbbd7-ea26-7b7c-b113-c89ac2790601","type":"ready","occurred_at":"2026-08-01T00:00:00Z","payload":{{"sidecar_name":"x","sidecar_version":"1","protocol_version":1}}}}')
print('{{"protocol_version":1,"request_id":null,"event_id":"019fbbd7-ea26-7b7c-b113-c89ac2790602","type":"capabilities","occurred_at":"2026-08-01T00:00:00Z","payload":{{"protocol_versions":[1],"commands":["hello","collect_user_posts"]}}}}')
sys.stdout.flush();sys.stdin.readline();cmd=json.loads(sys.stdin.readline());rid=cmd['request_id'];base={{'protocol_version':1,'request_id':rid,'stream':'{stream}','occurred_at':'2026-08-01T00:00:01Z'}}
print(json.dumps({{**base,'event_id':'019fbbd7-ea26-7b7c-b113-c89ac2790603','type':'started','sequence':1,'payload':{{}}}}))
print(json.dumps({{**base,'event_id':'019fbbd7-ea26-7b7c-b113-c89ac2790604','type':'post','sequence':2,'payload':{{'id':'94001','uid':'1','text':'committed page'}}}}))
print(json.dumps({{**base,'event_id':'019fbbd7-ea26-7b7c-b113-c89ac2790605','type':'checkpoint','sequence':3,'payload':{{'cursor':{{'max_id':'committed-page','max_id_type':0}},'fetched_count':1}}}}))
print(json.dumps({{**base,'event_id':'019fbbd7-ea26-7b7c-b113-c89ac2790606','type':'post','sequence':4,'payload':{{'id':'94002','uid':'1','text':'must roll back'}}}}))
print(json.dumps({{**base,'event_id':'019fbbd7-ea26-7b7c-b113-c89ac2790607','type':'{event_type}','sequence':5,'payload':{{'code':'{code}','message':'collector rejected C:\\private\\session.json','retryable':False}}}}));sys.stdout.flush()
"#
    )
}

#[tokio::test]
async fn auth_and_schema_failures_keep_committed_page_without_leaking_rate_or_session_state() {
    let Some(py) = python() else {
        eprintln!("python not found, skipping");
        return;
    };
    for (event_type, code) in [
        ("auth_required", "AUTH_REQUIRED"),
        ("error", "RESPONSE_SCHEMA_CHANGED"),
    ] {
        let db = create_db_pool_with_url(":memory:").await.unwrap();
        let stream = format!("user:{code}:posts");
        let job_id = enqueue_sync_job(&db, &job(&stream)).await.unwrap();
        let result = JobExecutor::with_spawn_options(
            db.clone(),
            Arc::new(WorkerRegistry::new()),
            scripted_options(
                py.clone(),
                &checkpoint_then_failure_script(&stream, event_type, code),
            ),
        )
        .run_next()
        .await
        .unwrap()
        .unwrap();

        assert_eq!(result.status, "failed", "{code}");
        let stored = get_sync_job(&db, job_id).await.unwrap().unwrap();
        assert_eq!(stored.status, "failed", "{code}");
        let last_error = stored.last_error.unwrap();
        assert_eq!(last_error, code);
        let run = get_sync_run_history(&db, job_id, 1)
            .await
            .unwrap()
            .remove(0);
        assert_eq!(run.status, "failed", "{code}");
        assert_eq!(run.error.as_deref(), Some(code));

        let checkpoint = get_sync_checkpoint(&db, &stream).await.unwrap().unwrap();
        assert_eq!(checkpoint.fetched_count, 1, "{code}");
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM posts WHERE id=94001")
                .fetch_one(&db)
                .await
                .unwrap(),
            1,
            "{code}"
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM posts WHERE id=94002")
                .fetch_one(&db)
                .await
                .unwrap(),
            0,
            "{code}"
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM processed_events WHERE event_id='019fbbd7-ea26-7b7c-b113-c89ac2790605'",
            )
            .fetch_one(&db)
            .await
            .unwrap(),
            1,
            "{code}"
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM rate_limit_gates")
                .fetch_one(&db)
                .await
                .unwrap(),
            0,
            "{code}"
        );
    }
}
