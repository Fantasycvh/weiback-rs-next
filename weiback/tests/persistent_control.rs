use weiback::storage::database::create_db_pool_with_url;
use weiback::storage::internal::entities::transactional::CommitPlan;
use weiback::storage::internal::entities::{
    CheckpointOwner, ClaimRequest, FinishRunRequest, JobControlResult, MediaDto, SyncCheckpointDto,
    SyncJobDto, SyncJobStatus, cancel_sync_job, claim_next_sync_job, create_sync_run,
    create_sync_run_at, enqueue_test_sync_job as enqueue_sync_job, finish_sync_run_at,
    get_sync_checkpoint, get_sync_job, get_sync_run_history, heartbeat_sync_run,
    heartbeat_sync_run_at, pause_sync_job, recover_interrupted_sync_jobs, resume_sync_job,
    retry_sync_job, save_sync_checkpoint, save_sync_checkpoint_at,
};

fn now(second: u8) -> String {
    format!("2026-08-01T00:00:{second:02}Z")
}

fn job(resource_key: &str) -> SyncJobDto {
    SyncJobDto {
        id: 0,
        resource_key: resource_key.into(),
        name: "持久控制测试".into(),
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
        created_at: now(0),
        updated_at: None,
        account_id: 1,
        endpoint_key: "__legacy__".into(),
        endpoint_gate_revision: 0,
        account_gate_revision: 0,
    }
}

async fn running_job(resource_key: &str) -> (sqlx::SqlitePool, i64, i64, i64) {
    let db = create_db_pool_with_url(":memory:").await.unwrap();
    let job_id = enqueue_sync_job(&db, &job(resource_key)).await.unwrap();
    let claimed = claim_next_sync_job(
        &db,
        &ClaimRequest {
            owner_token: "owner-a".into(),
            now_epoch: 1,
            lease_until_epoch: 10,
            claimed_at: now(1),
        },
    )
    .await
    .unwrap()
    .unwrap();
    let run_id = create_sync_run(&db, job_id, "owner-a", claimed.generation, &now(1))
        .await
        .unwrap()
        .unwrap();
    (db, job_id, run_id, claimed.generation)
}

#[tokio::test]
async fn legal_control_transitions_and_duplicate_controls_are_idempotent() {
    let db = create_db_pool_with_url(":memory:").await.unwrap();
    let job_id = enqueue_sync_job(&db, &job("user:control:posts"))
        .await
        .unwrap();

    assert_eq!(
        pause_sync_job(&db, job_id, &now(1)).await.unwrap(),
        JobControlResult::Changed
    );
    assert_eq!(
        pause_sync_job(&db, job_id, &now(2)).await.unwrap(),
        JobControlResult::AlreadyApplied
    );
    assert_eq!(
        resume_sync_job(&db, job_id, &now(3)).await.unwrap(),
        JobControlResult::Changed
    );
    assert_eq!(
        resume_sync_job(&db, job_id, &now(4)).await.unwrap(),
        JobControlResult::AlreadyApplied
    );
    assert_eq!(
        cancel_sync_job(&db, job_id, &now(5)).await.unwrap(),
        JobControlResult::Changed
    );
    assert_eq!(
        cancel_sync_job(&db, job_id, &now(6)).await.unwrap(),
        JobControlResult::AlreadyApplied
    );
    assert!(resume_sync_job(&db, job_id, &now(7)).await.is_err());

    let failed_id = enqueue_sync_job(&db, &job("user:retry:posts"))
        .await
        .unwrap();
    sqlx::query("UPDATE sync_jobs SET status='failed' WHERE id=?")
        .bind(failed_id)
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(
        retry_sync_job(&db, failed_id, &now(8)).await.unwrap(),
        JobControlResult::Changed
    );
    assert_eq!(
        retry_sync_job(&db, failed_id, &now(9)).await.unwrap(),
        JobControlResult::AlreadyApplied
    );
}

#[tokio::test]
async fn running_pause_and_cancel_end_run_and_revoke_owner_in_one_transaction() {
    for (resource, cancel, expected) in [
        ("user:pause-running:posts", false, "paused"),
        ("user:cancel-running:posts", true, "cancelled"),
    ] {
        let (db, job_id, run_id, generation) = running_job(resource).await;
        let result = if cancel {
            cancel_sync_job(&db, job_id, &now(2)).await.unwrap()
        } else {
            pause_sync_job(&db, job_id, &now(2)).await.unwrap()
        };
        assert_eq!(result, JobControlResult::Changed);

        let stored = get_sync_job(&db, job_id).await.unwrap().unwrap();
        assert_eq!(stored.status, expected);
        assert_eq!(stored.generation, generation + 1);
        assert_eq!(stored.owner_token, None);
        assert_eq!(stored.current_run_id, None);
        assert_eq!(stored.lease_until_epoch, None);
        let run = get_sync_run_history(&db, job_id, 1)
            .await
            .unwrap()
            .remove(0);
        assert_eq!(run.id, run_id);
        assert_eq!(run.status, expected);
        assert!(run.finished_at.is_some());
    }
}

#[tokio::test]
async fn claimed_job_without_run_can_be_controlled_and_cannot_start_after_fence() {
    for (resource, cancel, expected) in [
        ("user:claim-pause", false, "paused"),
        ("user:claim-cancel", true, "cancelled"),
    ] {
        let db = create_db_pool_with_url(":memory:").await.unwrap();
        let job_id = enqueue_sync_job(&db, &job(resource)).await.unwrap();
        let claimed = claim_next_sync_job(
            &db,
            &ClaimRequest {
                owner_token: "claim-owner".into(),
                now_epoch: 1,
                lease_until_epoch: 10,
                claimed_at: now(1),
            },
        )
        .await
        .unwrap()
        .unwrap();
        let result = if cancel {
            cancel_sync_job(&db, job_id, &now(2)).await.unwrap()
        } else {
            pause_sync_job(&db, job_id, &now(2)).await.unwrap()
        };
        assert_eq!(result, JobControlResult::Changed);
        let stored = get_sync_job(&db, job_id).await.unwrap().unwrap();
        assert_eq!(stored.status, expected);
        assert_eq!(stored.current_run_id, None);
        assert!(
            create_sync_run_at(&db, job_id, "claim-owner", claimed.generation, 2, &now(2),)
                .await
                .unwrap()
                .is_none()
        );
    }
}

#[tokio::test]
async fn expired_lease_cannot_create_or_revive_run() {
    let db = create_db_pool_with_url(":memory:").await.unwrap();
    let job_id = enqueue_sync_job(&db, &job("user:expired-create"))
        .await
        .unwrap();
    let claimed = claim_next_sync_job(
        &db,
        &ClaimRequest {
            owner_token: "expired-owner".into(),
            now_epoch: 1,
            lease_until_epoch: 2,
            claimed_at: now(1),
        },
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        create_sync_run_at(&db, job_id, "expired-owner", claimed.generation, 3, &now(3),)
            .await
            .unwrap()
            .is_none()
    );

    let live_job = enqueue_sync_job(&db, &job("user:expired-heartbeat"))
        .await
        .unwrap();
    let live_claim = claim_next_sync_job(
        &db,
        &ClaimRequest {
            owner_token: "heartbeat-owner".into(),
            now_epoch: 1,
            lease_until_epoch: 2,
            claimed_at: now(1),
        },
    )
    .await
    .unwrap()
    .unwrap();
    let run_id = create_sync_run_at(
        &db,
        live_job,
        "heartbeat-owner",
        live_claim.generation,
        1,
        &now(1),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        !heartbeat_sync_run_at(
            &db,
            live_job,
            run_id,
            "heartbeat-owner",
            live_claim.generation,
            3,
            10,
            &now(3),
        )
        .await
        .unwrap()
    );

    let finish = FinishRunRequest {
        job_id: live_job,
        run_id,
        owner_token: "heartbeat-owner".into(),
        generation: live_claim.generation,
        next_status: SyncJobStatus::Completed,
        finished_at: now(3),
        stats_json: None,
        error: None,
    };
    assert!(!finish_sync_run_at(&db, &finish, 2).await.unwrap());
    let checkpoint = SyncCheckpointDto {
        stream: "user:expired-heartbeat".into(),
        cursor_json: Some(r#"{"cursor":"stale"}"#.into()),
        fetched_count: 1,
        last_sequence: Some(1),
        updated_at: now(3),
        job_id: Some(live_job),
        run_id: Some(run_id),
        generation: Some(live_claim.generation),
        owner_token: Some("heartbeat-owner".into()),
        owner: CheckpointOwner::Persistent {
            run_id,
            generation: live_claim.generation,
            owner_token: "heartbeat-owner".into(),
        },
    };
    assert!(!save_sync_checkpoint_at(&db, &checkpoint, 2).await.unwrap());
    assert!(
        get_sync_checkpoint(&db, &checkpoint.stream)
            .await
            .unwrap()
            .is_none()
    );

    let summary = recover_interrupted_sync_jobs(&db, 2, &now(3))
        .await
        .unwrap();
    assert_eq!(summary.requeued, 2);
    assert_eq!(
        get_sync_job(&db, live_job).await.unwrap().unwrap().status,
        "pending"
    );
}

#[tokio::test]
async fn expired_lease_rolls_back_business_data_with_checkpoint() {
    let (db, job_id, run_id, generation) = running_job("user:expired-batch:posts").await;
    let checkpoint = SyncCheckpointDto {
        stream: "user:expired-batch:posts".into(),
        cursor_json: Some(r#"{"cursor":"expired"}"#.into()),
        fetched_count: 1,
        last_sequence: Some(1),
        updated_at: now(11),
        job_id: Some(job_id),
        run_id: Some(run_id),
        generation: Some(generation),
        owner_token: Some("owner-a".into()),
        owner: CheckpointOwner::Persistent {
            run_id,
            generation,
            owner_token: "owner-a".into(),
        },
    };
    let plan = CommitPlan {
        request_id: Some("expired-request".into()),
        stream: checkpoint.stream.clone(),
        sequence: 1,
        event_id: "expired-event".into(),
        users: vec![],
        posts: vec![],
        comments: vec![],
        media: vec![MediaDto {
            id: 0,
            owner_type: "post".into(),
            owner_id: None,
            media_type: "image".into(),
            url: "https://example.com/expired.jpg".into(),
            local_path: None,
            status: "pending".into(),
            retry_count: 0,
            last_error: None,
            created_at: now(11),
            updated_at: None,
        }],
        checkpoint,
        processed_at: now(11),
    };

    assert!(plan.execute_at(&db, 10).await.is_err());
    let media_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM media")
        .fetch_one(&db)
        .await
        .unwrap();
    let event_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM processed_events")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(media_count, 0);
    assert_eq!(event_count, 0);
    assert!(
        get_sync_checkpoint(&db, "user:expired-batch:posts")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn illegal_control_transitions_do_not_mutate_state() {
    let db = create_db_pool_with_url(":memory:").await.unwrap();
    let job_id = enqueue_sync_job(&db, &job("user:illegal:posts"))
        .await
        .unwrap();
    sqlx::query("UPDATE sync_jobs SET status='completed' WHERE id=?")
        .bind(job_id)
        .execute(&db)
        .await
        .unwrap();
    assert!(pause_sync_job(&db, job_id, &now(1)).await.is_err());
    assert!(resume_sync_job(&db, job_id, &now(1)).await.is_err());
    assert!(retry_sync_job(&db, job_id, &now(1)).await.is_err());
    assert!(cancel_sync_job(&db, job_id, &now(1)).await.is_err());
    assert_eq!(
        get_sync_job(&db, job_id).await.unwrap().unwrap().status,
        "completed"
    );
}

#[tokio::test]
async fn heartbeat_updates_job_and_run_atomically_and_stale_owner_fails() {
    let (db, job_id, run_id, generation) = running_job("user:heartbeat:posts").await;
    assert!(
        heartbeat_sync_run(&db, job_id, run_id, "owner-a", generation, 20, &now(2))
            .await
            .unwrap()
    );
    let job = get_sync_job(&db, job_id).await.unwrap().unwrap();
    let run = get_sync_run_history(&db, job_id, 1)
        .await
        .unwrap()
        .remove(0);
    assert_eq!(job.lease_until_epoch, Some(20));
    assert_eq!(run.lease_until_epoch, Some(20));

    assert!(
        heartbeat_sync_run(&db, job_id, run_id, "owner-a", generation, 15, &now(3))
            .await
            .unwrap()
    );
    assert_eq!(
        get_sync_job(&db, job_id)
            .await
            .unwrap()
            .unwrap()
            .lease_until_epoch,
        Some(20)
    );

    assert!(
        !heartbeat_sync_run(&db, job_id, run_id, "stale", generation, 30, &now(3))
            .await
            .unwrap()
    );
    assert_eq!(
        get_sync_job(&db, job_id)
            .await
            .unwrap()
            .unwrap()
            .lease_until_epoch,
        Some(20)
    );
}

#[tokio::test]
async fn persistent_empty_page_can_advance_cursor_without_count_growth() {
    let (db, job_id, run_id, generation) = running_job("user:empty-page:posts").await;
    let mut checkpoint = SyncCheckpointDto {
        stream: "user:empty-page:posts".into(),
        cursor_json: Some(r#"{"cursor":{"max_id":"page-1"}}"#.into()),
        fetched_count: 20,
        last_sequence: Some(2),
        updated_at: now(2),
        job_id: Some(job_id),
        run_id: Some(run_id),
        generation: Some(generation),
        owner_token: Some("owner-a".into()),
        owner: CheckpointOwner::Persistent {
            run_id,
            generation,
            owner_token: "owner-a".into(),
        },
    };
    assert!(save_sync_checkpoint(&db, &checkpoint).await.unwrap());
    checkpoint.cursor_json = Some(r#"{"cursor":{"max_id":"page-2-empty"}}"#.into());
    checkpoint.last_sequence = Some(3);
    checkpoint.updated_at = now(3);
    assert!(save_sync_checkpoint(&db, &checkpoint).await.unwrap());
    assert_eq!(
        get_sync_checkpoint(&db, &checkpoint.stream)
            .await
            .unwrap()
            .unwrap()
            .cursor_json,
        checkpoint.cursor_json
    );

    checkpoint.owner = CheckpointOwner::Persistent {
        run_id,
        generation,
        owner_token: "stale".into(),
    };
    checkpoint.cursor_json = Some(r#"{"cursor":{"max_id":"stale"}}"#.into());
    assert!(!save_sync_checkpoint(&db, &checkpoint).await.unwrap());
}
