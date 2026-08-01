use std::sync::Arc;

use tempfile::tempdir;
use tokio::sync::Barrier;
use weiback::storage::database::create_db_pool_with_url;
use weiback::storage::internal::entities::{
    CheckpointOwner, ClaimRequest, FinishRunRequest, SyncCheckpointDto, SyncJobDto, SyncJobStatus,
    claim_next_sync_job, create_sync_run, enqueue_test_sync_job as enqueue_sync_job,
    finish_sync_run, get_sync_checkpoint, get_sync_job, get_sync_run_history,
    recover_interrupted_sync_jobs, save_sync_checkpoint, transition_sync_job,
};

fn now(second: u8) -> String {
    format!("2026-08-01T00:00:{second:02}Z")
}

fn job(resource_key: &str) -> SyncJobDto {
    SyncJobDto {
        id: 0,
        resource_key: resource_key.into(),
        name: "用户帖子增量".into(),
        kind: "collect_user_posts".into(),
        payload_json: Some(r#"{"uid":123}"#.into()),
        status: SyncJobStatus::Pending.as_str().into(),
        priority: 10,
        schedule_config: None,
        enabled: true,
        recovery_count: 0,
        max_recovery_attempts: 2,
        available_at: Some(now(0)),
        available_at_epoch: 0,
        claimed_at: None,
        owner_token: None,
        lease_until_epoch: None,
        current_run_id: None,
        generation: 0,
        last_error: None,
        created_at: now(0),
        updated_at: None,
        account_id: 1,
        endpoint_key: "__legacy__".into(),
        endpoint_gate_revision: 0,
        account_gate_revision: 0,
    }
}

#[tokio::test]
async fn queue_roundtrip_cas_and_run_history() {
    let db = create_db_pool_with_url(":memory:").await.unwrap();
    let job_id = enqueue_sync_job(&db, &job("user:123:posts")).await.unwrap();

    let stored = get_sync_job(&db, job_id).await.unwrap().unwrap();
    assert_eq!(stored.resource_key, "user:123:posts");
    assert_eq!(stored.payload_json.as_deref(), Some(r#"{"uid":123}"#));
    assert_eq!(stored.status, "pending");

    let claimed = claim_next_sync_job(
        &db,
        &ClaimRequest {
            owner_token: "owner-a".into(),
            now_epoch: 1,
            lease_until_epoch: 100,
            claimed_at: now(1),
        },
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(claimed.id, job_id);
    assert!(
        !transition_sync_job(
            &db,
            job_id,
            SyncJobStatus::Pending,
            SyncJobStatus::Failed,
            &now(2),
            Some("stale writer"),
        )
        .await
        .unwrap()
    );

    let run_id = create_sync_run(&db, job_id, "owner-a", claimed.generation, &now(1))
        .await
        .unwrap()
        .expect("first run wins");
    assert!(
        finish_sync_run(
            &db,
            &FinishRunRequest {
                job_id,
                run_id,
                owner_token: "owner-a".into(),
                generation: claimed.generation,
                next_status: SyncJobStatus::Completed,
                finished_at: now(3),
                stats_json: Some(r#"{"posts":20}"#.into()),
                error: None,
            },
        )
        .await
        .unwrap()
    );
    let history = get_sync_run_history(&db, job_id, 10).await.unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].status, "completed");
    assert_eq!(history[0].stats_json.as_deref(), Some(r#"{"posts":20}"#));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_claim_has_exactly_one_winner() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("queue.sqlite");
    let db = create_db_pool_with_url(path.to_str().unwrap())
        .await
        .unwrap();
    enqueue_sync_job(&db, &job("user:123:posts")).await.unwrap();

    let barrier = Arc::new(Barrier::new(3));
    let mut tasks = Vec::new();
    for _ in 0..2 {
        let pool = db.clone();
        let barrier = barrier.clone();
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            claim_next_sync_job(
                &pool,
                &ClaimRequest {
                    owner_token: format!("owner-{}", std::thread::current().name().unwrap_or("x")),
                    now_epoch: 10,
                    lease_until_epoch: 20,
                    claimed_at: now(1),
                },
            )
            .await
            .unwrap()
        }));
    }
    barrier.wait().await;

    let mut winners = 0;
    for task in tasks {
        if task.await.unwrap().is_some() {
            winners += 1;
        }
    }
    assert_eq!(winners, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn active_resource_is_deduplicated() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("dedupe.sqlite");
    let db = create_db_pool_with_url(path.to_str().unwrap())
        .await
        .unwrap();
    let first = enqueue_sync_job(&db, &job("user:123:posts")).await.unwrap();

    let mut replacement = job("user:123:posts");
    replacement.priority = 99;
    replacement.payload_json = Some(r#"{"uid":123,"pages":5}"#.into());
    let second = enqueue_sync_job(&db, &replacement).await.unwrap();

    assert_eq!(first, second);
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sync_jobs WHERE resource_key = ? AND status IN ('pending','running','paused','interrupted')",
    )
    .bind("user:123:posts")
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(count, 1);
    let stored = get_sync_job(&db, first).await.unwrap().unwrap();
    assert_eq!(stored.priority, 99);
    assert_eq!(stored.payload_json, replacement.payload_json);

    assert!(
        transition_sync_job(
            &db,
            first,
            SyncJobStatus::Pending,
            SyncJobStatus::Completed,
            &now(1),
            None,
        )
        .await
        .unwrap()
    );
    let next = enqueue_sync_job(&db, &job("user:123:posts")).await.unwrap();
    assert_ne!(next, first);
}

#[tokio::test]
async fn stale_checkpoint_sequence_cannot_replace_new_cursor() {
    let db = create_db_pool_with_url(":memory:").await.unwrap();
    let mut checkpoint = SyncCheckpointDto {
        stream: "user:123:posts".into(),
        cursor_json: Some(r#"{"cursor":"new"}"#.into()),
        fetched_count: 40,
        last_sequence: Some(20),
        updated_at: now(2),
        job_id: None,
        run_id: None,
        generation: None,
        owner_token: None,
        owner: CheckpointOwner::AdHoc,
    };
    assert!(save_sync_checkpoint(&db, &checkpoint).await.unwrap());

    checkpoint.cursor_json = Some(r#"{"cursor":"old"}"#.into());
    checkpoint.fetched_count = 10;
    checkpoint.last_sequence = Some(10);
    checkpoint.updated_at = now(3);
    assert!(!save_sync_checkpoint(&db, &checkpoint).await.unwrap());

    let stored = get_sync_checkpoint(&db, &checkpoint.stream)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.cursor_json.as_deref(), Some(r#"{"cursor":"new"}"#));
    assert_eq!(stored.last_sequence, Some(20));
    assert_eq!(stored.fetched_count, 40);
}

#[tokio::test]
async fn startup_recovery_interrupts_run_and_requeues_job_without_losing_checkpoint() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("recovery.sqlite");
    let db = create_db_pool_with_url(path.to_str().unwrap())
        .await
        .unwrap();
    let job_id = enqueue_sync_job(&db, &job("user:123:posts")).await.unwrap();
    let claimed = claim_next_sync_job(
        &db,
        &ClaimRequest {
            owner_token: "owner-a".into(),
            now_epoch: 2_000_000_000,
            lease_until_epoch: 2_000_000_100,
            claimed_at: now(1),
        },
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(claimed.id, job_id);
    let run_id = create_sync_run(&db, job_id, "owner-a", claimed.generation, &now(1))
        .await
        .unwrap()
        .unwrap();
    let checkpoint = SyncCheckpointDto {
        stream: "user:123:posts".into(),
        cursor_json: Some(r#"{"cursor":"page-2"}"#.into()),
        fetched_count: 40,
        last_sequence: Some(20),
        updated_at: now(2),
        job_id: Some(job_id),
        run_id: Some(run_id),
        generation: Some(claimed.generation),
        owner_token: Some("owner-a".into()),
        owner: CheckpointOwner::Persistent {
            run_id,
            generation: claimed.generation,
            owner_token: "owner-a".into(),
        },
    };
    save_sync_checkpoint(&db, &checkpoint).await.unwrap();

    db.close().await;

    let db = create_db_pool_with_url(path.to_str().unwrap())
        .await
        .unwrap();
    let untouched = get_sync_job(&db, job_id).await.unwrap().unwrap();
    assert_eq!(untouched.status, "running", "unexpired lease is live");
    recover_interrupted_sync_jobs(&db, 2_000_000_101, &now(3))
        .await
        .unwrap();
    let stored = get_sync_job(&db, job_id).await.unwrap().unwrap();
    assert_eq!(stored.status, "pending");
    assert_eq!(stored.recovery_count, 1);
    let history = get_sync_run_history(&db, job_id, 10).await.unwrap();
    assert_eq!(history[0].id, run_id);
    assert_eq!(history[0].status, "interrupted");
    assert!(history[0].finished_at.is_some());
    assert_eq!(
        get_sync_checkpoint(&db, "user:123:posts")
            .await
            .unwrap()
            .unwrap()
            .cursor_json,
        checkpoint.cursor_json
    );
}

#[tokio::test]
async fn repeated_crashes_stop_at_recovery_limit() {
    let db = create_db_pool_with_url(":memory:").await.unwrap();
    let job_id = enqueue_sync_job(&db, &job("user:123:posts")).await.unwrap();

    for cycle in 0..2 {
        let owner = format!("owner-{cycle}");
        let claimed = claim_next_sync_job(
            &db,
            &ClaimRequest {
                owner_token: owner.clone(),
                now_epoch: i64::from(cycle) * 10,
                lease_until_epoch: i64::from(cycle) * 10 + 1,
                claimed_at: now(cycle * 2 + 1),
            },
        )
        .await
        .unwrap()
        .unwrap();
        create_sync_run(&db, job_id, &owner, claimed.generation, &now(cycle * 2 + 1))
            .await
            .unwrap()
            .unwrap();
        recover_interrupted_sync_jobs(&db, i64::from(cycle) * 10 + 2, &now(cycle * 2 + 2))
            .await
            .unwrap();
    }

    let stored = get_sync_job(&db, job_id).await.unwrap().unwrap();
    assert_eq!(stored.status, "failed");
    assert_eq!(stored.recovery_count, 2);
    assert!(
        claim_next_sync_job(
            &db,
            &ClaimRequest {
                owner_token: "late".into(),
                now_epoch: 99,
                lease_until_epoch: 100,
                claimed_at: now(9)
            }
        )
        .await
        .unwrap()
        .is_none()
    );
    assert_eq!(
        get_sync_run_history(&db, job_id, 10).await.unwrap().len(),
        2
    );
}

#[tokio::test]
async fn duplicate_run_creation_and_stale_owner_are_rejected() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("run-cas.sqlite");
    let db = create_db_pool_with_url(path.to_str().unwrap())
        .await
        .unwrap();
    let job_id = enqueue_sync_job(&db, &job("user:run-cas")).await.unwrap();
    let claimed = claim_next_sync_job(
        &db,
        &ClaimRequest {
            owner_token: "winner".into(),
            now_epoch: 1,
            lease_until_epoch: 10,
            claimed_at: now(1),
        },
    )
    .await
    .unwrap()
    .unwrap();
    let barrier = Arc::new(Barrier::new(3));
    let mut tasks = Vec::new();
    let generation = claimed.generation;
    for _ in 0..2 {
        let pool = db.clone();
        let barrier = barrier.clone();
        let started_at = now(1);
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            create_sync_run(&pool, job_id, "winner", generation, &started_at).await
        }));
    }
    barrier.wait().await;
    let mut run_ids = Vec::new();
    for task in tasks {
        if let Some(id) = task.await.unwrap().unwrap() {
            run_ids.push(id);
        }
    }
    assert_eq!(run_ids.len(), 1);
    let run_id = run_ids[0];
    assert!(
        !finish_sync_run(
            &db,
            &FinishRunRequest {
                job_id,
                run_id,
                owner_token: "stale".into(),
                generation: claimed.generation,
                next_status: SyncJobStatus::Completed,
                finished_at: now(2),
                stats_json: None,
                error: None,
            }
        )
        .await
        .unwrap()
    );
    assert_eq!(
        get_sync_job(&db, job_id).await.unwrap().unwrap().status,
        "running"
    );
}

#[tokio::test]
async fn stale_persistent_generation_cannot_write_checkpoint() {
    let db = create_db_pool_with_url(":memory:").await.unwrap();
    let job_id = enqueue_sync_job(&db, &job("user:checkpoint-owner"))
        .await
        .unwrap();
    let claimed = claim_next_sync_job(
        &db,
        &ClaimRequest {
            owner_token: "current".into(),
            now_epoch: 1,
            lease_until_epoch: 10,
            claimed_at: now(1),
        },
    )
    .await
    .unwrap()
    .unwrap();
    let run_id = create_sync_run(&db, job_id, "current", claimed.generation, &now(1))
        .await
        .unwrap()
        .unwrap();
    let stale = SyncCheckpointDto {
        stream: "user:checkpoint-owner".into(),
        cursor_json: Some(r#"{"cursor":"stale"}"#.into()),
        fetched_count: 20,
        last_sequence: Some(2),
        updated_at: now(2),
        job_id: Some(job_id),
        run_id: Some(run_id),
        generation: Some(claimed.generation - 1),
        owner_token: Some("stale".into()),
        owner: CheckpointOwner::Persistent {
            run_id,
            generation: claimed.generation - 1,
            owner_token: "stale".into(),
        },
    };
    assert!(!save_sync_checkpoint(&db, &stale).await.unwrap());
    assert!(
        get_sync_checkpoint(&db, &stale.stream)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn loaded_persistent_checkpoint_keeps_owner_and_rejects_adhoc_overwrite() {
    let db = create_db_pool_with_url(":memory:").await.unwrap();
    let job_id = enqueue_sync_job(&db, &job("user:persistent-stream"))
        .await
        .unwrap();
    let claimed = claim_next_sync_job(
        &db,
        &ClaimRequest {
            owner_token: "owner".into(),
            now_epoch: 1,
            lease_until_epoch: 10,
            claimed_at: now(1),
        },
    )
    .await
    .unwrap()
    .unwrap();
    let run_id = create_sync_run(&db, job_id, "owner", claimed.generation, &now(1))
        .await
        .unwrap()
        .unwrap();
    let persistent = SyncCheckpointDto {
        stream: "user:persistent-stream".into(),
        cursor_json: Some(r#"{"cursor":"owned"}"#.into()),
        fetched_count: 20,
        last_sequence: Some(2),
        updated_at: now(2),
        job_id: Some(job_id),
        run_id: Some(run_id),
        generation: Some(claimed.generation),
        owner_token: Some("owner".into()),
        owner: CheckpointOwner::Persistent {
            run_id,
            generation: claimed.generation,
            owner_token: "owner".into(),
        },
    };
    assert!(save_sync_checkpoint(&db, &persistent).await.unwrap());
    let loaded = get_sync_checkpoint(&db, &persistent.stream)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(loaded.owner, persistent.owner);

    let mut adhoc = loaded.clone();
    adhoc.fetched_count = 40;
    adhoc.owner = CheckpointOwner::AdHoc;
    adhoc.job_id = None;
    adhoc.run_id = None;
    adhoc.generation = None;
    adhoc.owner_token = None;
    assert!(!save_sync_checkpoint(&db, &adhoc).await.unwrap());
    assert_eq!(
        get_sync_checkpoint(&db, &persistent.stream)
            .await
            .unwrap()
            .unwrap()
            .cursor_json,
        persistent.cursor_json
    );
}

#[tokio::test]
async fn generic_transition_cannot_bypass_running_ownership() {
    let db = create_db_pool_with_url(":memory:").await.unwrap();
    let job_id = enqueue_sync_job(&db, &job("user:no-running-bypass"))
        .await
        .unwrap();
    assert!(
        transition_sync_job(
            &db,
            job_id,
            SyncJobStatus::Pending,
            SyncJobStatus::Running,
            &now(1),
            None,
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn legacy_run_insert_cannot_create_an_unowned_active_run() {
    let db = create_db_pool_with_url(":memory:").await.unwrap();
    let job_id = enqueue_sync_job(&db, &job("user:no-run-insert-bypass"))
        .await
        .unwrap();
    let run = weiback::storage::internal::entities::SyncRunDto {
        id: 0,
        job_id,
        status: "running".into(),
        started_at: now(1),
        finished_at: None,
        stats_json: None,
        error: None,
        attempt: 1,
        updated_at: None,
        owner_token: None,
        generation: 0,
        lease_until_epoch: None,
    };
    assert!(
        weiback::storage::internal::entities::save_sync_run(&db, &run)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn database_rejects_invalid_job_and_run_statuses() {
    let db = create_db_pool_with_url(":memory:").await.unwrap();
    let job_id = enqueue_sync_job(&db, &job("user:status-check"))
        .await
        .unwrap();
    assert!(
        sqlx::query("UPDATE sync_jobs SET status='bogus' WHERE id=?")
            .bind(job_id)
            .execute(&db)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("INSERT INTO sync_runs(job_id,status,started_at) VALUES(?,'bogus',?)")
            .bind(job_id)
            .bind(now(1))
            .execute(&db)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn finish_run_and_job_is_atomic() {
    let db = create_db_pool_with_url(":memory:").await.unwrap();
    let job_id = enqueue_sync_job(&db, &job("user:atomic")).await.unwrap();
    let claimed = claim_next_sync_job(
        &db,
        &ClaimRequest {
            owner_token: "owner".into(),
            now_epoch: 1,
            lease_until_epoch: 10,
            claimed_at: now(1),
        },
    )
    .await
    .unwrap()
    .unwrap();
    let run_id = create_sync_run(&db, job_id, "owner", claimed.generation, &now(1))
        .await
        .unwrap()
        .unwrap();
    assert!(
        finish_sync_run(
            &db,
            &FinishRunRequest {
                job_id,
                run_id,
                owner_token: "owner".into(),
                generation: claimed.generation,
                next_status: SyncJobStatus::Completed,
                finished_at: now(2),
                stats_json: None,
                error: None,
            }
        )
        .await
        .unwrap()
    );
    assert_eq!(
        get_sync_job(&db, job_id).await.unwrap().unwrap().status,
        "completed"
    );
    assert_eq!(
        get_sync_run_history(&db, job_id, 1).await.unwrap()[0].status,
        "completed"
    );
}

#[tokio::test]
async fn epoch_availability_and_enqueue_control_state_are_preserved() {
    let db = create_db_pool_with_url(":memory:").await.unwrap();
    let mut queued = job("user:epoch");
    queued.available_at_epoch = 200;
    let job_id = enqueue_sync_job(&db, &queued).await.unwrap();
    assert!(
        claim_next_sync_job(
            &db,
            &ClaimRequest {
                owner_token: "early".into(),
                now_epoch: 199,
                lease_until_epoch: 210,
                claimed_at: now(1)
            }
        )
        .await
        .unwrap()
        .is_none()
    );
    let claimed = claim_next_sync_job(
        &db,
        &ClaimRequest {
            owner_token: "owner".into(),
            now_epoch: 200,
            lease_until_epoch: 210,
            claimed_at: now(2),
        },
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(claimed.id, job_id);

    let mut replacement = job("user:epoch");
    replacement.priority = 999;
    enqueue_sync_job(&db, &replacement).await.unwrap();
    let stored = get_sync_job(&db, job_id).await.unwrap().unwrap();
    assert_eq!(stored.status, "running");
    assert_eq!(
        stored.priority, queued.priority,
        "running job is immutable to enqueue"
    );
}

#[tokio::test]
async fn recovery_limit_zero_and_one_fail_on_first_expired_lease() {
    for limit in [0, 1] {
        let db = create_db_pool_with_url(":memory:").await.unwrap();
        let mut queued = job(&format!("user:limit-{limit}"));
        queued.max_recovery_attempts = limit;
        let id = enqueue_sync_job(&db, &queued).await.unwrap();
        let claimed = claim_next_sync_job(
            &db,
            &ClaimRequest {
                owner_token: "owner".into(),
                now_epoch: 1,
                lease_until_epoch: 2,
                claimed_at: now(1),
            },
        )
        .await
        .unwrap()
        .unwrap();
        create_sync_run(&db, id, "owner", claimed.generation, &now(1))
            .await
            .unwrap()
            .unwrap();
        let recovered = recover_interrupted_sync_jobs(&db, 3, &now(2))
            .await
            .unwrap();
        assert_eq!(recovered.failed, 1, "limit={limit}");
        assert_eq!(
            get_sync_job(&db, id).await.unwrap().unwrap().status,
            "failed"
        );
    }
}

#[tokio::test]
async fn completed_resource_can_reenqueue_and_continue_owned_checkpoint() {
    let db = create_db_pool_with_url(":memory:").await.unwrap();
    let first_job = enqueue_sync_job(&db, &job("user:resume:posts"))
        .await
        .unwrap();
    let first_claim = claim_next_sync_job(
        &db,
        &ClaimRequest {
            owner_token: "owner-1".into(),
            now_epoch: 1,
            lease_until_epoch: 10,
            claimed_at: now(1),
        },
    )
    .await
    .unwrap()
    .unwrap();
    let first_run = create_sync_run(&db, first_job, "owner-1", first_claim.generation, &now(1))
        .await
        .unwrap()
        .unwrap();
    let first_checkpoint = SyncCheckpointDto {
        stream: "user:resume:posts".into(),
        cursor_json: Some(r#"{"cursor":"page-2"}"#.into()),
        fetched_count: 40,
        last_sequence: Some(2),
        updated_at: now(2),
        job_id: Some(first_job),
        run_id: Some(first_run),
        generation: Some(first_claim.generation),
        owner_token: Some("owner-1".into()),
        owner: CheckpointOwner::Persistent {
            run_id: first_run,
            generation: first_claim.generation,
            owner_token: "owner-1".into(),
        },
    };
    assert!(save_sync_checkpoint(&db, &first_checkpoint).await.unwrap());
    assert!(
        finish_sync_run(
            &db,
            &FinishRunRequest {
                job_id: first_job,
                run_id: first_run,
                owner_token: "owner-1".into(),
                generation: first_claim.generation,
                next_status: SyncJobStatus::Completed,
                finished_at: now(3),
                stats_json: None,
                error: None,
            },
        )
        .await
        .unwrap()
    );

    let second_job = enqueue_sync_job(&db, &job("user:resume:posts"))
        .await
        .unwrap();
    assert_ne!(second_job, first_job);
    let second_claim = claim_next_sync_job(
        &db,
        &ClaimRequest {
            owner_token: "owner-2".into(),
            now_epoch: 11,
            lease_until_epoch: 20,
            claimed_at: now(4),
        },
    )
    .await
    .unwrap()
    .unwrap();
    let second_run = create_sync_run(&db, second_job, "owner-2", second_claim.generation, &now(4))
        .await
        .unwrap()
        .unwrap();
    let mut continued = first_checkpoint.clone();
    continued.cursor_json = Some(r#"{"cursor":"page-3"}"#.into());
    continued.fetched_count = 60;
    continued.job_id = Some(second_job);
    continued.run_id = Some(second_run);
    continued.generation = Some(second_claim.generation);
    continued.owner_token = Some("owner-2".into());
    continued.owner = CheckpointOwner::Persistent {
        run_id: second_run,
        generation: second_claim.generation,
        owner_token: "owner-2".into(),
    };
    assert!(save_sync_checkpoint(&db, &continued).await.unwrap());

    let mut stale = continued.clone();
    stale.fetched_count = 80;
    stale.owner = first_checkpoint.owner.clone();
    stale.job_id = Some(first_job);
    stale.run_id = Some(first_run);
    stale.generation = Some(first_claim.generation);
    stale.owner_token = Some("owner-1".into());
    assert!(!save_sync_checkpoint(&db, &stale).await.unwrap());
    assert_eq!(
        get_sync_checkpoint(&db, "user:resume:posts")
            .await
            .unwrap()
            .unwrap()
            .cursor_json,
        continued.cursor_json
    );
}

#[tokio::test]
async fn run_lease_is_copied_from_claimed_job() {
    let db = create_db_pool_with_url(":memory:").await.unwrap();
    let job_id = enqueue_sync_job(&db, &job("user:lease-source"))
        .await
        .unwrap();
    let claimed = claim_next_sync_job(
        &db,
        &ClaimRequest {
            owner_token: "owner".into(),
            now_epoch: 1,
            lease_until_epoch: 10,
            claimed_at: now(1),
        },
    )
    .await
    .unwrap()
    .unwrap();
    let run_id = create_sync_run(&db, job_id, "owner", claimed.generation, &now(1))
        .await
        .unwrap()
        .unwrap();
    let lease: i64 = sqlx::query_scalar("SELECT lease_until_epoch FROM sync_runs WHERE id=?")
        .bind(run_id)
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(lease, 10);
}

#[tokio::test]
async fn claim_recovers_expired_lease_without_restart() {
    let db = create_db_pool_with_url(":memory:").await.unwrap();
    let job_id = enqueue_sync_job(&db, &job("user:live-recovery"))
        .await
        .unwrap();
    let first = claim_next_sync_job(
        &db,
        &ClaimRequest {
            owner_token: "expired".into(),
            now_epoch: 1,
            lease_until_epoch: 2,
            claimed_at: now(1),
        },
    )
    .await
    .unwrap()
    .unwrap();
    let first_run = create_sync_run(&db, job_id, "expired", first.generation, &now(1))
        .await
        .unwrap()
        .unwrap();

    let reclaimed = claim_next_sync_job(
        &db,
        &ClaimRequest {
            owner_token: "replacement".into(),
            now_epoch: 3,
            lease_until_epoch: 10,
            claimed_at: now(3),
        },
    )
    .await
    .unwrap()
    .expect("expired job must be recovered and claimed");
    assert_eq!(reclaimed.id, job_id);
    assert_eq!(reclaimed.owner_token.as_deref(), Some("replacement"));
    let old_status: String = sqlx::query_scalar("SELECT status FROM sync_runs WHERE id=?")
        .bind(first_run)
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(old_status, "interrupted");
}

#[tokio::test]
async fn expired_claim_without_run_is_bounded_by_its_own_budget_across_reopen() {
    let db = create_db_pool_with_url(":memory:").await.unwrap();
    let mut queued = job("user:startup-crash");
    queued.max_recovery_attempts = 1;
    let job_id = enqueue_sync_job(&db, &queued).await.unwrap();
    claim_next_sync_job(
        &db,
        &ClaimRequest {
            owner_token: "crashed-before-run".into(),
            now_epoch: 1,
            lease_until_epoch: 2,
            claimed_at: now(1),
        },
    )
    .await
    .unwrap()
    .unwrap();

    let summary = recover_interrupted_sync_jobs(&db, 3, &now(3))
        .await
        .unwrap();
    assert_eq!(summary.requeued, 0);
    assert_eq!(summary.failed, 1);
    let recovered = get_sync_job(&db, job_id).await.unwrap().unwrap();
    assert_eq!(recovered.status, "failed");
    assert_eq!(recovered.recovery_count, 0);
    let pre_run_count: i64 =
        sqlx::query_scalar("SELECT pre_run_recovery_count FROM sync_jobs WHERE id=?")
            .bind(job_id)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(pre_run_count, 1);
    assert!(
        get_sync_run_history(&db, job_id, 10)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn first_pre_run_crash_does_not_consume_execution_recovery_counter() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("pre-run-reopen.sqlite");
    let db = create_db_pool_with_url(path.to_str().unwrap())
        .await
        .unwrap();
    let mut queued = job("user:pre-run-reopen");
    queued.max_recovery_attempts = 2;
    let id = enqueue_sync_job(&db, &queued).await.unwrap();
    claim_next_sync_job(
        &db,
        &ClaimRequest {
            owner_token: "crashed".into(),
            now_epoch: 10,
            lease_until_epoch: 11,
            claimed_at: now(1),
        },
    )
    .await
    .unwrap()
    .unwrap();
    db.close().await;
    let db = create_db_pool_with_url(path.to_str().unwrap())
        .await
        .unwrap();
    let recovered = get_sync_job(&db, id).await.unwrap().unwrap();
    assert_eq!(recovered.recovery_count, 0);
    assert_eq!(recovered.status, "pending");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn enqueue_never_loses_resource_during_terminal_transition() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("enqueue-race.sqlite");
    let db = create_db_pool_with_url(path.to_str().unwrap())
        .await
        .unwrap();

    for cycle in 0..200 {
        let active = enqueue_sync_job(&db, &job("user:enqueue-race"))
            .await
            .unwrap();
        let barrier = Arc::new(Barrier::new(3));
        let finish_pool = db.clone();
        let finish_barrier = barrier.clone();
        let finish = tokio::spawn(async move {
            finish_barrier.wait().await;
            transition_sync_job(
                &finish_pool,
                active,
                SyncJobStatus::Pending,
                SyncJobStatus::Completed,
                &now(1),
                None,
            )
            .await
        });
        let enqueue_pool = db.clone();
        let enqueue_barrier = barrier.clone();
        let enqueue = tokio::spawn(async move {
            enqueue_barrier.wait().await;
            enqueue_sync_job(&enqueue_pool, &job("user:enqueue-race")).await
        });
        barrier.wait().await;
        finish.await.unwrap().unwrap();
        let enqueued_id = enqueue
            .await
            .unwrap()
            .unwrap_or_else(|error| panic!("cycle {cycle}: legal enqueue failed: {error}"));
        assert!(
            get_sync_job(&db, enqueued_id).await.unwrap().is_some(),
            "cycle {cycle}: enqueue returned a missing row"
        );
        let active_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sync_jobs WHERE resource_key=? AND status IN ('pending','running','paused','interrupted')",
        )
        .bind("user:enqueue-race")
        .fetch_one(&db)
        .await
        .unwrap();
        if active_count == 0 {
            enqueue_sync_job(&db, &job("user:enqueue-race"))
                .await
                .unwrap_or_else(|error| {
                    panic!("cycle {cycle}: retry after later terminal transition failed: {error}")
                });
        }
        let stabilized: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sync_jobs WHERE resource_key=? AND status IN ('pending','running','paused','interrupted')",
        )
        .bind("user:enqueue-race")
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(stabilized, 1, "cycle {cycle}");
    }
}
