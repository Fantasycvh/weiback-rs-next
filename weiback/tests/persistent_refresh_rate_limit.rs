use std::{
    collections::VecDeque,
    path::PathBuf,
    process::Command,
    sync::{Arc, Mutex},
    time::Duration,
};

use tempfile::tempdir;
use weiback::rate_limit::PendingRateLimitBarrier;
use weiback::refresh_scheduler::{
    RefreshScheduleConfig, deterministic_refresh_jitter, scan_due_monitored_users,
};
use weiback::sidecar::{RateLimitInfo, RateLimitScope, SpawnOptions};
use weiback::storage::database::create_db_pool_with_url;
use weiback::storage::internal::entities::{
    AccountDto, CheckpointOwner, ClaimRequest, FinishRunRequest, MonitoredUserDto, RefreshTier,
    SyncCheckpointDto, SyncJobSpec, SyncJobStatus, create_sync_run, delete_account,
    delete_monitored_user, enqueue_sync_job_spec, finish_sync_run, get_account, get_accounts,
    get_monitored_users, get_rate_limit_gate, get_sync_job, heartbeat_sync_run, resume_sync_job,
    save_account, save_monitored_user, save_sync_checkpoint, set_rate_limit_gate,
};
use weiback::sync_executor::{
    AccountSpawnResolver, JobExecutor, WorkerRegistry, account_session_resolver,
    secure_account_session_resolver,
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

fn scripted_options(python: PathBuf, script: &str) -> SpawnOptions {
    SpawnOptions {
        program: python,
        args: vec!["-u".into(), "-c".into(), script.into()],
        env: vec![("PYTHONUTF8".into(), "1".into())],
        cwd: None,
        handshake_timeout: Duration::from_secs(5),
    }
}

fn account(provider: &str, uid: &str, session_ref: &str) -> AccountDto {
    AccountDto {
        id: 0,
        provider: provider.into(),
        uid: uid.into(),
        display_name: Some(format!("account-{uid}")),
        session_ref: session_ref.into(),
        enabled: true,
        created_at: "2026-08-01T00:00:00Z".into(),
        updated_at: None,
    }
}

fn monitored(account_id: i64, uid: i64, tier: RefreshTier, due: i64) -> MonitoredUserDto {
    MonitoredUserDto {
        account_id,
        uid,
        screen_name: Some(format!("user-{uid}")),
        refresh_strategy: tier.as_str().into(),
        tier,
        interval_secs: 0,
        jitter_secs: 0,
        next_refresh_epoch: due,
        last_refresh_epoch: None,
        enabled: true,
        last_refreshed_at: None,
        created_at: "2026-08-01T00:00:00Z".into(),
        updated_at: None,
    }
}

#[tokio::test]
async fn accounts_upsert_persists_references_without_secret_columns() {
    let db = create_db_pool_with_url(":memory:").await.unwrap();
    let id = save_account(&db, &account("weibo", "100", "sessions/a.json"))
        .await
        .unwrap();
    let mut changed = account("weibo", "100", "sessions/b.json");
    changed.display_name = Some("updated".into());
    assert_eq!(save_account(&db, &changed).await.unwrap(), id);
    let stored = get_account(&db, id).await.unwrap().unwrap();
    assert_eq!(stored.session_ref, "sessions/b.json");
    assert_eq!(
        get_accounts(&db).await.unwrap().len(),
        2,
        "legacy + explicit account"
    );
    let columns: Vec<String> = sqlx::query_scalar("SELECT name FROM pragma_table_info('accounts')")
        .fetch_all(&db)
        .await
        .unwrap();
    assert!(
        !columns
            .iter()
            .any(|column| matches!(column.as_str(), "cookie" | "token" | "password" | "secret"))
    );
    assert!(delete_account(&db, id).await.unwrap());
}

#[tokio::test]
async fn monitored_users_include_disabled_rows_and_can_be_deleted() {
    let db = create_db_pool_with_url(":memory:").await.unwrap();
    let account_id = save_account(&db, &account("weibo", "100", "sessions/a.json"))
        .await
        .unwrap();
    let mut disabled = monitored(account_id, 123, RefreshTier::Warm, 10);
    disabled.enabled = false;
    save_monitored_user(&db, &disabled).await.unwrap();

    let users = get_monitored_users(&db).await.unwrap();
    assert_eq!(users.len(), 1);
    assert!(!users[0].enabled);
    assert!(delete_monitored_user(&db, account_id, 123).await.unwrap());
    assert!(!delete_monitored_user(&db, account_id, 123).await.unwrap());
    assert!(get_monitored_users(&db).await.unwrap().is_empty());
}

#[tokio::test]
async fn saving_stale_monitor_config_preserves_scheduler_runtime_fields() {
    let db = create_db_pool_with_url(":memory:").await.unwrap();
    let account_id = save_account(&db, &account("weibo", "100", "sessions/a.json"))
        .await
        .unwrap();
    let original = monitored(account_id, 123, RefreshTier::Warm, 10);
    save_monitored_user(&db, &original).await.unwrap();
    scan_due_monitored_users(&db, 10, &RefreshScheduleConfig::default())
        .await
        .unwrap();
    let advanced = get_monitored_users(&db).await.unwrap().remove(0);
    assert!(advanced.next_refresh_epoch > 10);

    let mut stale_edit = original;
    stale_edit.screen_name = Some("renamed".into());
    stale_edit.tier = RefreshTier::Hot;
    save_monitored_user(&db, &stale_edit).await.unwrap();

    let stored = get_monitored_users(&db).await.unwrap().remove(0);
    assert_eq!(stored.screen_name.as_deref(), Some("renamed"));
    assert_eq!(stored.tier, RefreshTier::Hot);
    assert_eq!(stored.next_refresh_epoch, advanced.next_refresh_epoch);
    assert_eq!(stored.last_refresh_epoch, advanced.last_refresh_epoch);
}

#[tokio::test]
async fn due_monitor_does_not_advance_when_active_job_is_paused_or_interrupted() {
    let db = create_db_pool_with_url(":memory:").await.unwrap();
    let account_id = save_account(&db, &account("weibo", "paused", "sessions/a.json"))
        .await
        .unwrap();
    save_monitored_user(&db, &monitored(account_id, 123, RefreshTier::Hot, 10))
        .await
        .unwrap();
    let job_id = enqueue_sync_job_spec(
        &db,
        &SyncJobSpec::CollectUserPosts {
            account_id,
            uid: 123,
            max_pages: None,
            priority: 1,
        },
        0,
        "created",
    )
    .await
    .unwrap();
    for status in ["paused", "interrupted"] {
        sqlx::query("UPDATE sync_jobs SET status=? WHERE id=?")
            .bind(status)
            .bind(job_id)
            .execute(&db)
            .await
            .unwrap();
        assert_eq!(
            scan_due_monitored_users(&db, 10, &RefreshScheduleConfig::default())
                .await
                .unwrap()
                .enqueued,
            0
        );
        assert_eq!(
            get_monitored_users(&db).await.unwrap()[0].next_refresh_epoch,
            10
        );
    }
}

#[tokio::test]
async fn disabling_account_fences_owned_run_and_rejects_late_writes() {
    let db = create_db_pool_with_url(":memory:").await.unwrap();
    let account_id = save_account(&db, &account("weibo", "disable", "sessions/a.json"))
        .await
        .unwrap();
    let job_id = enqueue_sync_job_spec(
        &db,
        &SyncJobSpec::CollectUserPosts {
            account_id,
            uid: 123,
            max_pages: None,
            priority: 1,
        },
        0,
        "created",
    )
    .await
    .unwrap();
    let claimed = weiback::storage::internal::entities::claim_next_sync_job(
        &db,
        &ClaimRequest {
            owner_token: "disabled-owner".into(),
            now_epoch: 1,
            lease_until_epoch: 20,
            claimed_at: "claimed".into(),
        },
    )
    .await
    .unwrap()
    .unwrap();
    let run_id = create_sync_run(&db, job_id, "disabled-owner", claimed.generation, "started")
        .await
        .unwrap()
        .unwrap();

    let mut disabled = get_account(&db, account_id).await.unwrap().unwrap();
    disabled.enabled = false;
    disabled.updated_at = Some("disabled".into());
    assert_eq!(save_account(&db, &disabled).await.unwrap(), account_id);

    let stored = get_sync_job(&db, job_id).await.unwrap().unwrap();
    assert_eq!(stored.status, "paused");
    assert!(!stored.enabled);
    assert_eq!(stored.owner_token, None);
    assert_eq!(stored.current_run_id, None);
    assert!(stored.generation > claimed.generation);
    let run_status: String = sqlx::query_scalar("SELECT status FROM sync_runs WHERE id=?")
        .bind(run_id)
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(run_status, "paused");
    assert!(
        !heartbeat_sync_run(
            &db,
            job_id,
            run_id,
            "disabled-owner",
            claimed.generation,
            30,
            "late",
        )
        .await
        .unwrap()
    );
    assert!(
        !save_sync_checkpoint(
            &db,
            &SyncCheckpointDto {
                stream: format!("account:{account_id}:user:123:posts"),
                cursor_json: Some("{}".into()),
                fetched_count: 1,
                last_sequence: Some(1),
                updated_at: "late".into(),
                job_id: Some(job_id),
                run_id: Some(run_id),
                generation: Some(claimed.generation),
                owner_token: Some("disabled-owner".into()),
                owner: CheckpointOwner::Persistent {
                    run_id,
                    generation: claimed.generation,
                    owner_token: "disabled-owner".into(),
                },
            },
        )
        .await
        .unwrap()
    );

    disabled.enabled = true;
    save_account(&db, &disabled).await.unwrap();
    resume_sync_job(&db, job_id, "resumed").await.unwrap();
    let reclaimed = weiback::storage::internal::entities::claim_next_sync_job(
        &db,
        &ClaimRequest {
            owner_token: "new-owner".into(),
            now_epoch: 30,
            lease_until_epoch: 40,
            claimed_at: "reclaimed".into(),
        },
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(reclaimed.id, job_id);
    assert!(reclaimed.enabled);
}

#[tokio::test]
async fn typed_job_specs_create_canonical_keys_and_reject_invalid_low_level_jobs() {
    let db = create_db_pool_with_url(":memory:").await.unwrap();
    let account_id = save_account(&db, &account("weibo", "100", "sessions/a.json"))
        .await
        .unwrap();
    let id = enqueue_sync_job_spec(
        &db,
        &SyncJobSpec::CollectUserPosts {
            account_id,
            uid: 123,
            max_pages: Some(4),
            priority: 7,
        },
        10,
        "2026-08-01T00:00:10Z",
    )
    .await
    .unwrap();
    let job = get_sync_job(&db, id).await.unwrap().unwrap();
    assert_eq!(
        job.resource_key,
        format!("account:{account_id}:user:123:posts")
    );
    assert_eq!(job.endpoint_key, "collect_user_posts");
    assert_eq!(job.account_id, account_id);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(job.payload_json.as_deref().unwrap()).unwrap()["uid"],
        123
    );

    let replies = enqueue_sync_job_spec(
        &db,
        &SyncJobSpec::CollectCommentReplies {
            account_id,
            post_id: 456,
            root_comment_id: 789,
            max_pages: Some(2),
            priority: 8,
        },
        10,
        "2026-08-01T00:00:10Z",
    )
    .await
    .unwrap();
    let replies = get_sync_job(&db, replies).await.unwrap().unwrap();
    assert_eq!(
        replies.resource_key,
        format!("account:{account_id}:post:456:comment:789:replies")
    );
    let payload: serde_json::Value =
        serde_json::from_str(replies.payload_json.as_deref().unwrap()).unwrap();
    assert_eq!(payload["post_id"], 456);
    assert_eq!(payload["root_comment_id"], 789);

    let mut invalid = job.clone();
    invalid.id = 0;
    invalid.resource_key = "wrong".into();
    assert!(
        weiback::storage::internal::entities::save_sync_job(&db, &invalid)
            .await
            .is_err()
    );
    invalid.kind = "unknown".into();
    assert!(
        weiback::storage::internal::entities::save_sync_job(&db, &invalid)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn refresh_tiers_jitter_scheduler_and_disabled_behavior_are_deterministic() {
    let db = create_db_pool_with_url(":memory:").await.unwrap();
    let account_id = save_account(&db, &account("weibo", "100", "sessions/a.json"))
        .await
        .unwrap();
    let config = RefreshScheduleConfig {
        hot_interval_secs: 60,
        warm_interval_secs: 600,
        cold_interval_secs: 3600,
        hot_jitter_secs: 5,
        warm_jitter_secs: 30,
        cold_jitter_secs: 120,
    };
    for (index, tier) in [RefreshTier::Hot, RefreshTier::Warm, RefreshTier::Cold]
        .into_iter()
        .enumerate()
    {
        save_monitored_user(&db, &monitored(account_id, 1000 + index as i64, tier, 100))
            .await
            .unwrap();
        let a = deterministic_refresh_jitter(
            account_id,
            1000 + index as i64,
            tier,
            1,
            config.jitter_secs(tier),
        );
        let b = deterministic_refresh_jitter(
            account_id,
            1000 + index as i64,
            tier,
            1,
            config.jitter_secs(tier),
        );
        assert_eq!(a, b);
        assert!(a.abs() <= config.jitter_secs(tier));
    }
    let mut disabled = monitored(account_id, 2000, RefreshTier::Hot, 100);
    disabled.enabled = false;
    save_monitored_user(&db, &disabled).await.unwrap();
    let first = scan_due_monitored_users(&db, 100, &config).await.unwrap();
    assert_eq!(first.enqueued, 3);
    let scheduled: Vec<(String, i64)> = sqlx::query_as(
        "SELECT tier,next_refresh_epoch FROM monitored_users WHERE enabled=1 ORDER BY tier",
    )
    .fetch_all(&db)
    .await
    .unwrap();
    for (tier, next) in scheduled {
        let tier = match tier.as_str() {
            "hot" => RefreshTier::Hot,
            "warm" => RefreshTier::Warm,
            "cold" => RefreshTier::Cold,
            other => panic!("unknown tier {other}"),
        };
        let center = 100 + config.interval_secs(tier);
        assert!(
            (center - config.jitter_secs(tier)..=center + config.jitter_secs(tier)).contains(&next)
        );
    }
    let second = scan_due_monitored_users(&db, 100, &config).await.unwrap();
    assert_eq!(second.enqueued, 0);
    let disabled_jobs: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sync_jobs WHERE resource_key LIKE '%user:2000:posts'",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(disabled_jobs, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_refresh_scans_enqueue_each_due_user_once_and_survive_reopen() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("refresh.sqlite");
    let db = create_db_pool_with_url(path.to_str().unwrap())
        .await
        .unwrap();
    let account_id = save_account(&db, &account("weibo", "100", "sessions/a.json"))
        .await
        .unwrap();
    save_monitored_user(&db, &monitored(account_id, 123, RefreshTier::Hot, 10))
        .await
        .unwrap();
    let config = RefreshScheduleConfig::default();
    let (a, b) = tokio::join!(
        scan_due_monitored_users(&db, 10, &config),
        scan_due_monitored_users(&db, 10, &config)
    );
    assert_eq!(a.unwrap().enqueued + b.unwrap().enqueued, 1);
    db.close().await;
    let db = create_db_pool_with_url(path.to_str().unwrap())
        .await
        .unwrap();
    assert_eq!(
        scan_due_monitored_users(&db, 10, &config)
            .await
            .unwrap()
            .enqueued,
        0
    );
}

#[tokio::test]
async fn claim_respects_account_and_endpoint_gates_without_priority_starvation() {
    let db = create_db_pool_with_url(":memory:").await.unwrap();
    let a1 = save_account(&db, &account("weibo", "1", "sessions/1.json"))
        .await
        .unwrap();
    let a2 = save_account(&db, &account("weibo", "2", "sessions/2.json"))
        .await
        .unwrap();
    let blocked = enqueue_sync_job_spec(
        &db,
        &SyncJobSpec::CollectUserPosts {
            account_id: a1,
            uid: 1,
            max_pages: None,
            priority: 100,
        },
        0,
        "x",
    )
    .await
    .unwrap();
    let runnable = enqueue_sync_job_spec(
        &db,
        &SyncJobSpec::CollectUserPosts {
            account_id: a2,
            uid: 2,
            max_pages: None,
            priority: 1,
        },
        0,
        "x",
    )
    .await
    .unwrap();
    set_rate_limit_gate(&db, a1, "__account__", 100, 1, Some(90), "x")
        .await
        .unwrap();
    let claim = ClaimRequest {
        owner_token: "owner".into(),
        now_epoch: 10,
        lease_until_epoch: 20,
        claimed_at: "x".into(),
    };
    let won = weiback::storage::internal::entities::claim_next_sync_job_with_gates(&db, &claim, 5)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(won.id, runnable);
    assert_eq!(
        get_sync_job(&db, blocked).await.unwrap().unwrap().status,
        "pending"
    );
    assert_eq!(
        get_rate_limit_gate(&db, a2, "collect_user_posts")
            .await
            .unwrap()
            .unwrap()
            .next_allowed_epoch,
        15
    );

    set_rate_limit_gate(&db, a2, "__account__", 100, 1, None, "x")
        .await
        .unwrap();
    assert!(
        weiback::storage::internal::entities::claim_next_sync_job_with_gates(
            &db,
            &ClaimRequest {
                owner_token: "blocked".into(),
                now_epoch: 20,
                lease_until_epoch: 30,
                claimed_at: "x".into(),
            },
            5,
        )
        .await
        .unwrap()
        .is_none()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_claim_reserves_endpoint_gate_for_one_winner() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("claim-gate.sqlite");
    let db = create_db_pool_with_url(path.to_str().unwrap())
        .await
        .unwrap();
    let account_id = save_account(&db, &account("weibo", "1", "sessions/1.json"))
        .await
        .unwrap();
    for uid in [1, 2] {
        enqueue_sync_job_spec(
            &db,
            &SyncJobSpec::CollectUserPosts {
                account_id,
                uid,
                max_pages: None,
                priority: 1,
            },
            0,
            "x",
        )
        .await
        .unwrap();
    }
    let a = db.clone();
    let b = db.clone();
    let first = tokio::spawn(async move {
        weiback::storage::internal::entities::claim_next_sync_job_with_gates(
            &a,
            &ClaimRequest {
                owner_token: "a".into(),
                now_epoch: 10,
                lease_until_epoch: 20,
                claimed_at: "x".into(),
            },
            5,
        )
        .await
    });
    let second = tokio::spawn(async move {
        weiback::storage::internal::entities::claim_next_sync_job_with_gates(
            &b,
            &ClaimRequest {
                owner_token: "b".into(),
                now_epoch: 10,
                lease_until_epoch: 20,
                claimed_at: "x".into(),
            },
            5,
        )
        .await
    });
    let winners = [
        first.await.unwrap().unwrap(),
        second.await.unwrap().unwrap(),
    ]
    .into_iter()
    .flatten()
    .count();
    assert_eq!(winners, 1);
}

#[tokio::test]
async fn rate_limit_gate_never_shortens_and_persists_across_reopen() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("gate.sqlite");
    let db = create_db_pool_with_url(path.to_str().unwrap())
        .await
        .unwrap();
    let account_id = save_account(&db, &account("weibo", "1", "sessions/1.json"))
        .await
        .unwrap();
    set_rate_limit_gate(&db, account_id, "collect_user_posts", 100, 2, Some(90), "x")
        .await
        .unwrap();
    set_rate_limit_gate(&db, account_id, "collect_user_posts", 50, 1, Some(40), "y")
        .await
        .unwrap();
    db.close().await;
    let db = create_db_pool_with_url(path.to_str().unwrap())
        .await
        .unwrap();
    let gate = get_rate_limit_gate(&db, account_id, "collect_user_posts")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(gate.next_allowed_epoch, 100);
    assert_eq!(gate.backoff_level, 2);
}

#[tokio::test]
async fn request_endpoint_and_account_scopes_and_missing_retry_after_update_expected_gates() {
    let db = create_db_pool_with_url(":memory:").await.unwrap();
    let account_id = save_account(&db, &account("weibo", "1", "sessions/1.json"))
        .await
        .unwrap();
    for (uid, scope, expected_key) in [
        (2, RateLimitScope::Endpoint, "collect_user_posts"),
        (3, RateLimitScope::Account, "__account__"),
    ] {
        let job_id = enqueue_sync_job_spec(
            &db,
            &SyncJobSpec::CollectUserPosts {
                account_id,
                uid,
                max_pages: None,
                priority: 1,
            },
            0,
            "x",
        )
        .await
        .unwrap();
        weiback::rate_limit::apply_rate_limit_to_pending_job(
            &db,
            job_id,
            &RateLimitInfo {
                scope,
                retry_after_ms: None,
            },
            10,
            "x",
        )
        .await
        .unwrap();
        let first = get_rate_limit_gate(&db, account_id, expected_key)
            .await
            .unwrap()
            .unwrap();
        assert!(first.next_allowed_epoch > 10);
        weiback::rate_limit::apply_rate_limit_to_pending_job(
            &db,
            job_id,
            &RateLimitInfo {
                scope,
                retry_after_ms: None,
            },
            10,
            "y",
        )
        .await
        .unwrap();
        let second = get_rate_limit_gate(&db, account_id, expected_key)
            .await
            .unwrap()
            .unwrap();
        assert!(second.backoff_level > first.backoff_level);
        assert!(second.next_allowed_epoch >= first.next_allowed_epoch);
    }
}

#[test]
fn retry_after_rounds_up_and_exponential_backoff_is_bounded_and_deterministic() {
    assert_eq!(weiback::rate_limit::retry_after_epoch(10, 1), 11);
    assert_eq!(weiback::rate_limit::retry_after_epoch(10, 1001), 12);
    let a = weiback::rate_limit::backoff_delay_secs(7, "collect_user_posts", 4, 300);
    let b = weiback::rate_limit::backoff_delay_secs(7, "collect_user_posts", 4, 300);
    assert_eq!(a, b);
    assert!((1..=300).contains(&a));
}

#[tokio::test]
async fn executor_resolves_account_session_and_rate_limit_requeues_with_checkpoint() {
    let db = create_db_pool_with_url(":memory:").await.unwrap();
    let account_id = save_account(&db, &account("weibo", "100", "sessions/account.json"))
        .await
        .unwrap();
    let job_id = enqueue_sync_job_spec(
        &db,
        &SyncJobSpec::CollectUserPosts {
            account_id,
            uid: 123,
            max_pages: None,
            priority: 1,
        },
        0,
        "x",
    )
    .await
    .unwrap();
    let resolver: AccountSpawnResolver = Arc::new(|account| {
        assert_eq!(account.session_ref, "sessions/account.json");
        Ok(SpawnOptions {
            program: PathBuf::from("fixture"),
            ..SpawnOptions::default()
        })
    });
    let executor = JobExecutor::new(db.clone(), Arc::new(WorkerRegistry::new()))
        .with_account_resolver(resolver);
    assert_eq!(
        executor
            .resolve_spawn_options(job_id)
            .await
            .unwrap()
            .program,
        PathBuf::from("fixture")
    );

    let template = SpawnOptions {
        program: PathBuf::from("collector"),
        ..SpawnOptions::default()
    };
    let session_root = tempdir().unwrap();
    std::fs::create_dir(session_root.path().join("sessions")).unwrap();
    std::fs::write(session_root.path().join("sessions/account.json"), "{}").unwrap();
    let standard =
        JobExecutor::new(db.clone(), Arc::new(WorkerRegistry::new())).with_account_resolver(
            secure_account_session_resolver(session_root.path().to_path_buf(), template),
        );
    let resolved = standard.resolve_spawn_options(job_id).await.unwrap();
    assert!(resolved.env.iter().any(|(key, value)| {
        key == "WEIBACK_COLLECTOR_SESSION_PATH"
            && PathBuf::from(value).ends_with("sessions/account.json")
    }));

    let info = RateLimitInfo {
        scope: RateLimitScope::Endpoint,
        retry_after_ms: Some(1001),
    };
    weiback::rate_limit::apply_rate_limit_to_pending_job(&db, job_id, &info, 10, "x")
        .await
        .unwrap();
    let job = get_sync_job(&db, job_id).await.unwrap().unwrap();
    assert_eq!(job.status, "pending");
    assert_eq!(job.available_at_epoch, 12);
}

#[tokio::test]
async fn persistent_rate_limit_kills_sidecar_preserves_checkpoint_and_resumes_after_gate() {
    let Some(py) = python() else {
        eprintln!("python not found, skipping");
        return;
    };
    let db = create_db_pool_with_url(":memory:").await.unwrap();
    let account_id = save_account(&db, &account("weibo", "100", "sessions/account.json"))
        .await
        .unwrap();
    let job_id = enqueue_sync_job_spec(
        &db,
        &SyncJobSpec::CollectUserPosts {
            account_id,
            uid: 123,
            max_pages: None,
            priority: 1,
        },
        0,
        "x",
    )
    .await
    .unwrap();
    let stream = format!("account:{account_id}:user:123:posts");
    let protocol_stream = "user:123:posts";
    let first_script = format!(
        "import json,sys,time\nprint('{{\"protocol_version\":1,\"request_id\":null,\"event_id\":\"019fbbd7-ea26-7b7c-b113-c89ac2791001\",\"type\":\"ready\",\"occurred_at\":\"2026-08-01T00:00:00Z\",\"payload\":{{\"sidecar_name\":\"x\",\"sidecar_version\":\"1\",\"protocol_version\":1}}}}')\nprint('{{\"protocol_version\":1,\"request_id\":null,\"event_id\":\"019fbbd7-ea26-7b7c-b113-c89ac2791002\",\"type\":\"capabilities\",\"occurred_at\":\"2026-08-01T00:00:00Z\",\"payload\":{{\"protocol_versions\":[1],\"commands\":[\"hello\",\"collect_user_posts\"]}}}}')\nsys.stdout.flush();sys.stdin.readline();cmd=json.loads(sys.stdin.readline());rid=cmd['request_id'];base={{'protocol_version':1,'request_id':rid,'stream':'{protocol_stream}','occurred_at':'2026-08-01T00:00:01Z'}}\nprint(json.dumps({{**base,'event_id':'019fbbd7-ea26-7b7c-b113-c89ac2791003','type':'started','sequence':1,'payload':{{}}}}))\nprint(json.dumps({{**base,'event_id':'019fbbd7-ea26-7b7c-b113-c89ac2791004','type':'post','sequence':2,'payload':{{'id':'93001','uid':'123','text':'before limit'}}}}))\nprint(json.dumps({{**base,'event_id':'019fbbd7-ea26-7b7c-b113-c89ac2791005','type':'checkpoint','sequence':3,'payload':{{'cursor':{{'max_id':'saved-limit','max_id_type':0}},'fetched_count':20}}}}))\nprint(json.dumps({{**base,'event_id':'019fbbd7-ea26-7b7c-b113-c89ac2791006','type':'rate_limited','sequence':4,'payload':{{'scope':'request','retry_after_ms':1,'retryable':True}}}}));sys.stdout.flush();time.sleep(30)\n"
    );
    let second_script = format!(
        "import json,sys\nprint('{{\"protocol_version\":1,\"request_id\":null,\"event_id\":\"019fbbd7-ea26-7b7c-b113-c89ac2791011\",\"type\":\"ready\",\"occurred_at\":\"2026-08-01T00:00:00Z\",\"payload\":{{\"sidecar_name\":\"x\",\"sidecar_version\":\"1\",\"protocol_version\":1}}}}')\nprint('{{\"protocol_version\":1,\"request_id\":null,\"event_id\":\"019fbbd7-ea26-7b7c-b113-c89ac2791012\",\"type\":\"capabilities\",\"occurred_at\":\"2026-08-01T00:00:00Z\",\"payload\":{{\"protocol_versions\":[1],\"commands\":[\"hello\",\"collect_user_posts\"]}}}}')\nsys.stdout.flush();sys.stdin.readline();cmd=json.loads(sys.stdin.readline());assert cmd['payload']['checkpoint']['max_id']=='saved-limit';rid=cmd['request_id'];base={{'protocol_version':1,'request_id':rid,'stream':'{protocol_stream}','occurred_at':'2026-08-01T00:00:02Z'}}\nprint(json.dumps({{**base,'event_id':'019fbbd7-ea26-7b7c-b113-c89ac2791013','type':'started','sequence':1,'payload':{{}}}}))\nprint(json.dumps({{**base,'event_id':'019fbbd7-ea26-7b7c-b113-c89ac2791014','type':'checkpoint','sequence':2,'payload':{{'cursor':{{'max_id':'done','max_id_type':0}},'fetched_count':40}}}}))\nprint(json.dumps({{**base,'event_id':'019fbbd7-ea26-7b7c-b113-c89ac2791015','type':'done','sequence':3,'payload':{{'status':'completed','fetched_count':40}}}}));sys.stdout.flush()\n"
    );
    let options = Arc::new(Mutex::new(VecDeque::from([
        scripted_options(py.clone(), &first_script),
        scripted_options(py, &second_script),
    ])));
    let resolver: AccountSpawnResolver = Arc::new(move |account| {
        assert_eq!(account.session_ref, "sessions/account.json");
        options
            .lock()
            .map_err(|error| weiback::error::Error::Lock(error.to_string()))?
            .pop_front()
            .ok_or_else(|| weiback::error::Error::InconsistentTask("no fixture options".into()))
    });
    let executor = JobExecutor::new(db.clone(), Arc::new(WorkerRegistry::new()))
        .with_account_resolver(resolver);
    let limited = executor.run_next().await.unwrap().unwrap();
    assert_eq!(limited.status, "pending");
    let checkpoint = weiback::storage::internal::entities::get_sync_checkpoint(&db, &stream)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(checkpoint.fetched_count, 20);
    assert!(
        get_rate_limit_gate(&db, account_id, "collect_user_posts")
            .await
            .unwrap()
            .is_none()
    );
    let available = get_sync_job(&db, job_id)
        .await
        .unwrap()
        .unwrap()
        .available_at_epoch;
    let wait = available.saturating_sub(chrono::Utc::now().timestamp());
    if wait > 0 {
        tokio::time::sleep(Duration::from_secs(wait as u64)).await;
    }
    let completed = executor.run_next().await.unwrap().unwrap();
    assert_eq!(completed.job_id, job_id);
    assert_eq!(completed.status, "completed");
    assert!(
        get_rate_limit_gate(&db, account_id, "collect_user_posts")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn disabled_account_is_excluded_from_scheduler_claim_and_session_resolution() {
    let db = create_db_pool_with_url(":memory:").await.unwrap();
    let mut disabled = account("weibo", "disabled", "sessions/disabled.json");
    disabled.enabled = false;
    let account_id = save_account(&db, &disabled).await.unwrap();
    let mut disabled_monitor = monitored(account_id, 42, RefreshTier::Hot, 0);
    disabled_monitor.enabled = false;
    save_monitored_user(&db, &disabled_monitor).await.unwrap();
    assert_eq!(
        scan_due_monitored_users(&db, 10, &RefreshScheduleConfig::default())
            .await
            .unwrap()
            .enqueued,
        0
    );
    sqlx::query("UPDATE accounts SET enabled=1 WHERE id=?")
        .bind(account_id)
        .execute(&db)
        .await
        .unwrap();
    let job_id = enqueue_sync_job_spec(
        &db,
        &SyncJobSpec::CollectUserPosts {
            account_id,
            uid: 42,
            max_pages: None,
            priority: 1,
        },
        0,
        "x",
    )
    .await
    .unwrap();
    sqlx::query("UPDATE accounts SET enabled=0 WHERE id=?")
        .bind(account_id)
        .execute(&db)
        .await
        .unwrap();
    assert!(
        weiback::storage::internal::entities::claim_next_sync_job_with_gates(
            &db,
            &ClaimRequest {
                owner_token: "disabled".into(),
                now_epoch: 10,
                lease_until_epoch: 20,
                claimed_at: "x".into(),
            },
            5,
        )
        .await
        .unwrap()
        .is_none()
    );
    let executor = JobExecutor::new(db, Arc::new(WorkerRegistry::new()))
        .with_account_resolver(account_session_resolver(SpawnOptions::default()));
    assert!(executor.resolve_spawn_options(job_id).await.is_err());
}

#[tokio::test]
async fn request_scope_updates_only_job_and_never_creates_shared_gate() {
    let db = create_db_pool_with_url(":memory:").await.unwrap();
    let account_id = save_account(&db, &account("weibo", "request", "sessions/request.json"))
        .await
        .unwrap();
    let job_id = enqueue_sync_job_spec(
        &db,
        &SyncJobSpec::CollectUserPosts {
            account_id,
            uid: 1,
            max_pages: None,
            priority: 1,
        },
        0,
        "x",
    )
    .await
    .unwrap();
    weiback::rate_limit::apply_rate_limit_to_pending_job(
        &db,
        job_id,
        &RateLimitInfo {
            scope: RateLimitScope::Request,
            retry_after_ms: Some(1),
        },
        10,
        "x",
    )
    .await
    .unwrap();
    assert!(
        get_rate_limit_gate(&db, account_id, "collect_user_posts")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        get_sync_job(&db, job_id)
            .await
            .unwrap()
            .unwrap()
            .available_at_epoch
            > 11
    );
}

#[test]
fn unknown_and_global_rate_limit_scopes_are_protocol_errors() {
    assert_eq!(
        RateLimitScope::parse_protocol("request").unwrap(),
        RateLimitScope::Request
    );
    assert_eq!(
        RateLimitScope::parse_protocol("endpoint").unwrap(),
        RateLimitScope::Endpoint
    );
    assert_eq!(
        RateLimitScope::parse_protocol("account").unwrap(),
        RateLimitScope::Account
    );
    assert!(RateLimitScope::parse_protocol("global").is_err());
    assert!(RateLimitScope::parse_protocol("mystery").is_err());
}

#[test]
fn retry_after_is_never_shorter_than_local_exponential_backoff() {
    for retry_after_ms in [0, 1, 1001] {
        let delay = weiback::rate_limit::rate_limit_delay_secs(
            7,
            "collect_user_posts",
            4,
            Some(retry_after_ms),
            300,
        );
        assert!(delay >= weiback::rate_limit::backoff_delay_secs(7, "collect_user_posts", 4, 300));
    }
}

#[tokio::test]
async fn pending_rate_limit_cannot_revoke_owner_when_claim_wins_race() {
    let db = create_db_pool_with_url(":memory:").await.unwrap();
    let account_id = save_account(&db, &account("weibo", "race", "sessions/race.json"))
        .await
        .unwrap();
    let job_id = enqueue_sync_job_spec(
        &db,
        &SyncJobSpec::CollectUserPosts {
            account_id,
            uid: 1,
            max_pages: None,
            priority: 1,
        },
        0,
        "x",
    )
    .await
    .unwrap();
    let barrier = PendingRateLimitBarrier::new();
    let rate_pool = db.clone();
    let rate_barrier = barrier.clone();
    let rate = tokio::spawn(async move {
        weiback::rate_limit::apply_rate_limit_to_pending_job_with_barrier(
            &rate_pool,
            job_id,
            &RateLimitInfo {
                scope: RateLimitScope::Endpoint,
                retry_after_ms: Some(1000),
            },
            10,
            "x",
            &rate_barrier,
        )
        .await
    });
    barrier.wait_until_entered().await;
    let claimed = weiback::storage::internal::entities::claim_next_sync_job_with_gates(
        &db,
        &ClaimRequest {
            owner_token: "winner".into(),
            now_epoch: 10,
            lease_until_epoch: 20,
            claimed_at: "x".into(),
        },
        0,
    )
    .await
    .unwrap()
    .unwrap();
    barrier.release();
    assert!(rate.await.unwrap().is_err());
    let stored = get_sync_job(&db, job_id).await.unwrap().unwrap();
    assert_eq!(stored.status, "running");
    assert_eq!(stored.owner_token, claimed.owner_token);
    assert!(
        get_rate_limit_gate(&db, account_id, "collect_user_posts")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn same_second_new_limit_revision_survives_older_success() {
    let db = create_db_pool_with_url(":memory:").await.unwrap();
    let account_id = save_account(&db, &account("weibo", "revision", "sessions/revision.json"))
        .await
        .unwrap();
    set_rate_limit_gate(
        &db,
        account_id,
        "collect_user_posts",
        100,
        1,
        None,
        "same-second",
    )
    .await
    .unwrap();
    let job_id = enqueue_sync_job_spec(
        &db,
        &SyncJobSpec::CollectUserPosts {
            account_id,
            uid: 9,
            max_pages: None,
            priority: 1,
        },
        0,
        "same-second",
    )
    .await
    .unwrap();
    let claimed = weiback::storage::internal::entities::claim_next_sync_job(
        &db,
        &ClaimRequest {
            owner_token: "old-run".into(),
            now_epoch: 101,
            lease_until_epoch: 200,
            claimed_at: "2026-08-01T00:00:00Z".into(),
        },
    )
    .await
    .unwrap()
    .unwrap();
    let run_id = create_sync_run(&db, job_id, "old-run", claimed.generation, "same-second")
        .await
        .unwrap()
        .unwrap();
    set_rate_limit_gate(
        &db,
        account_id,
        "collect_user_posts",
        300,
        2,
        None,
        "same-second",
    )
    .await
    .unwrap();
    assert!(
        finish_sync_run(
            &db,
            &FinishRunRequest {
                job_id,
                run_id,
                owner_token: "old-run".into(),
                generation: claimed.generation,
                next_status: SyncJobStatus::Completed,
                finished_at: "same-second".into(),
                stats_json: None,
                error: None,
            },
        )
        .await
        .unwrap()
    );
    let gate = get_rate_limit_gate(&db, account_id, "collect_user_posts")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(gate.backoff_level, 2);
    assert!(gate.revision >= 2);
}

#[tokio::test]
async fn successful_run_resets_only_the_captured_account_gate_revision() {
    let db = create_db_pool_with_url(":memory:").await.unwrap();
    let account_id = save_account(
        &db,
        &account(
            "weibo",
            "account-revision",
            "sessions/account-revision.json",
        ),
    )
    .await
    .unwrap();
    set_rate_limit_gate(&db, account_id, "__account__", 100, 1, None, "first")
        .await
        .unwrap();
    let job_id = enqueue_sync_job_spec(
        &db,
        &SyncJobSpec::CollectUserPosts {
            account_id,
            uid: 90,
            max_pages: None,
            priority: 1,
        },
        0,
        "first",
    )
    .await
    .unwrap();
    let claimed = weiback::storage::internal::entities::claim_next_sync_job(
        &db,
        &ClaimRequest {
            owner_token: "account-old-run".into(),
            now_epoch: 101,
            lease_until_epoch: 200,
            claimed_at: "same-second".into(),
        },
    )
    .await
    .unwrap()
    .unwrap();
    let run_id = create_sync_run(
        &db,
        job_id,
        "account-old-run",
        claimed.generation,
        "same-second",
    )
    .await
    .unwrap()
    .unwrap();

    assert!(
        finish_sync_run(
            &db,
            &FinishRunRequest {
                job_id,
                run_id,
                owner_token: "account-old-run".into(),
                generation: claimed.generation,
                next_status: SyncJobStatus::Completed,
                finished_at: "same-second".into(),
                stats_json: None,
                error: None,
            },
        )
        .await
        .unwrap()
    );
    assert_eq!(
        get_rate_limit_gate(&db, account_id, "__account__")
            .await
            .unwrap()
            .unwrap()
            .backoff_level,
        0
    );

    let second_job_id = enqueue_sync_job_spec(
        &db,
        &SyncJobSpec::CollectUserPosts {
            account_id,
            uid: 91,
            max_pages: None,
            priority: 1,
        },
        0,
        "same-second",
    )
    .await
    .unwrap();
    let second_claimed = weiback::storage::internal::entities::claim_next_sync_job(
        &db,
        &ClaimRequest {
            owner_token: "account-second-old-run".into(),
            now_epoch: 101,
            lease_until_epoch: 200,
            claimed_at: "same-second".into(),
        },
    )
    .await
    .unwrap()
    .unwrap();
    let second_run_id = create_sync_run(
        &db,
        second_job_id,
        "account-second-old-run",
        second_claimed.generation,
        "same-second",
    )
    .await
    .unwrap()
    .unwrap();
    set_rate_limit_gate(&db, account_id, "__account__", 300, 2, None, "same-second")
        .await
        .unwrap();
    assert!(
        finish_sync_run(
            &db,
            &FinishRunRequest {
                job_id: second_job_id,
                run_id: second_run_id,
                owner_token: "account-second-old-run".into(),
                generation: second_claimed.generation,
                next_status: SyncJobStatus::Completed,
                finished_at: "same-second".into(),
                stats_json: None,
                error: None,
            },
        )
        .await
        .unwrap()
    );
    let newer_gate = get_rate_limit_gate(&db, account_id, "__account__")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(newer_gate.backoff_level, 2);
    assert!(newer_gate.revision >= 2);
}

#[tokio::test]
async fn gate_reset_failure_rolls_back_successful_finish() {
    let db = create_db_pool_with_url(":memory:").await.unwrap();
    let account_id = save_account(&db, &account("weibo", "rollback", "sessions/rollback.json"))
        .await
        .unwrap();
    set_rate_limit_gate(&db, account_id, "collect_user_posts", 1, 1, None, "x")
        .await
        .unwrap();
    let job_id = enqueue_sync_job_spec(
        &db,
        &SyncJobSpec::CollectUserPosts {
            account_id,
            uid: 10,
            max_pages: None,
            priority: 1,
        },
        0,
        "x",
    )
    .await
    .unwrap();
    let claimed = weiback::storage::internal::entities::claim_next_sync_job(
        &db,
        &ClaimRequest {
            owner_token: "rollback-owner".into(),
            now_epoch: 2,
            lease_until_epoch: 20,
            claimed_at: "x".into(),
        },
    )
    .await
    .unwrap()
    .unwrap();
    let run_id = create_sync_run(&db, job_id, "rollback-owner", claimed.generation, "x")
        .await
        .unwrap()
        .unwrap();
    sqlx::query(
        "CREATE TRIGGER fail_gate_reset BEFORE UPDATE OF backoff_level ON rate_limit_gates \
         WHEN NEW.backoff_level=0 BEGIN SELECT RAISE(ABORT,'reset failed'); END",
    )
    .execute(&db)
    .await
    .unwrap();
    assert!(
        finish_sync_run(
            &db,
            &FinishRunRequest {
                job_id,
                run_id,
                owner_token: "rollback-owner".into(),
                generation: claimed.generation,
                next_status: SyncJobStatus::Completed,
                finished_at: "x".into(),
                stats_json: None,
                error: None,
            },
        )
        .await
        .is_err()
    );
    assert_eq!(
        get_sync_job(&db, job_id).await.unwrap().unwrap().status,
        "running"
    );
    let run_status: String = sqlx::query_scalar("SELECT status FROM sync_runs WHERE id=?")
        .bind(run_id)
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(run_status, "running");
}

#[tokio::test]
async fn request_backoff_resets_on_success_and_next_failure_starts_at_one() {
    let db = create_db_pool_with_url(":memory:").await.unwrap();
    let account_id = save_account(
        &db,
        &account("weibo", "request-reset", "sessions/reset.json"),
    )
    .await
    .unwrap();
    let job_id = enqueue_sync_job_spec(
        &db,
        &SyncJobSpec::CollectUserPosts {
            account_id,
            uid: 11,
            max_pages: None,
            priority: 1,
        },
        0,
        "x",
    )
    .await
    .unwrap();
    weiback::rate_limit::apply_rate_limit_to_pending_job(
        &db,
        job_id,
        &RateLimitInfo {
            scope: RateLimitScope::Request,
            retry_after_ms: None,
        },
        1,
        "x",
    )
    .await
    .unwrap();
    let claimed = weiback::storage::internal::entities::claim_next_sync_job(
        &db,
        &ClaimRequest {
            owner_token: "reset-owner".into(),
            now_epoch: 10,
            lease_until_epoch: 20,
            claimed_at: "x".into(),
        },
    )
    .await
    .unwrap()
    .unwrap();
    let run_id = create_sync_run(&db, job_id, "reset-owner", claimed.generation, "x")
        .await
        .unwrap()
        .unwrap();
    finish_sync_run(
        &db,
        &FinishRunRequest {
            job_id,
            run_id,
            owner_token: "reset-owner".into(),
            generation: claimed.generation,
            next_status: SyncJobStatus::Completed,
            finished_at: "x".into(),
            stats_json: None,
            error: None,
        },
    )
    .await
    .unwrap();
    let level: i64 =
        sqlx::query_scalar("SELECT rate_limit_backoff_level FROM sync_jobs WHERE id=?")
            .bind(job_id)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(level, 0);

    let next_job = enqueue_sync_job_spec(
        &db,
        &SyncJobSpec::CollectUserPosts {
            account_id,
            uid: 11,
            max_pages: None,
            priority: 1,
        },
        0,
        "y",
    )
    .await
    .unwrap();
    weiback::rate_limit::apply_rate_limit_to_pending_job(
        &db,
        next_job,
        &RateLimitInfo {
            scope: RateLimitScope::Request,
            retry_after_ms: None,
        },
        20,
        "y",
    )
    .await
    .unwrap();
    let next_level: i64 =
        sqlx::query_scalar("SELECT rate_limit_backoff_level FROM sync_jobs WHERE id=?")
            .bind(next_job)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(next_level, 1);
}

#[tokio::test]
async fn repeated_rate_limits_exhaust_the_persisted_recovery_budget() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("rate-limit-budget.sqlite");
    let mut db = create_db_pool_with_url(path.to_str().unwrap())
        .await
        .unwrap();
    let account_id = save_account(&db, &account("weibo", "budget", "sessions/budget.json"))
        .await
        .unwrap();
    let job_id = enqueue_sync_job_spec(
        &db,
        &SyncJobSpec::CollectUserPosts {
            account_id,
            uid: 77,
            max_pages: None,
            priority: 1,
        },
        0,
        "created",
    )
    .await
    .unwrap();
    sqlx::query("UPDATE sync_jobs SET max_recovery_attempts=2 WHERE id=?")
        .bind(job_id)
        .execute(&db)
        .await
        .unwrap();

    for attempt in 1..=2 {
        let now_epoch = i64::from(attempt) * 10_000;
        let owner = format!("rate-owner-{attempt}");
        let claimed = weiback::storage::internal::entities::claim_next_sync_job(
            &db,
            &ClaimRequest {
                owner_token: owner.clone(),
                now_epoch,
                lease_until_epoch: now_epoch + 10,
                claimed_at: format!("attempt-{attempt}"),
            },
        )
        .await
        .unwrap()
        .unwrap();
        let run_id = create_sync_run(
            &db,
            job_id,
            &owner,
            claimed.generation,
            &format!("attempt-{attempt}"),
        )
        .await
        .unwrap()
        .unwrap();
        weiback::rate_limit::finish_rate_limited_run(
            &db,
            &FinishRunRequest {
                job_id,
                run_id,
                owner_token: owner,
                generation: claimed.generation,
                next_status: SyncJobStatus::Interrupted,
                finished_at: format!("attempt-{attempt}"),
                stats_json: None,
                error: Some("rate limited".into()),
            },
            &RateLimitInfo {
                scope: RateLimitScope::Request,
                retry_after_ms: Some(1),
            },
            now_epoch,
        )
        .await
        .unwrap();
        if attempt == 1 {
            db.close().await;
            db = create_db_pool_with_url(path.to_str().unwrap())
                .await
                .unwrap();
            let stored = get_sync_job(&db, job_id).await.unwrap().unwrap();
            assert_eq!(stored.status, "pending");
            assert_eq!(stored.recovery_count, 1);
        }
    }
    let exhausted = get_sync_job(&db, job_id).await.unwrap().unwrap();
    assert_eq!(exhausted.status, "failed");
    assert_eq!(exhausted.recovery_count, 2);
}

#[tokio::test]
async fn expired_owner_cannot_persist_rate_limit_or_end_run() {
    let db = create_db_pool_with_url(":memory:").await.unwrap();
    let account_id = save_account(
        &db,
        &account("weibo", "expired-rate", "sessions/expired-rate.json"),
    )
    .await
    .unwrap();
    let job_id = enqueue_sync_job_spec(
        &db,
        &SyncJobSpec::CollectUserPosts {
            account_id,
            uid: 78,
            max_pages: None,
            priority: 1,
        },
        0,
        "created",
    )
    .await
    .unwrap();
    let claimed = weiback::storage::internal::entities::claim_next_sync_job(
        &db,
        &ClaimRequest {
            owner_token: "expired-rate-owner".into(),
            now_epoch: 1,
            lease_until_epoch: 2,
            claimed_at: "claimed".into(),
        },
    )
    .await
    .unwrap()
    .unwrap();
    let run_id = create_sync_run(&db, job_id, "expired-rate-owner", claimed.generation, "run")
        .await
        .unwrap()
        .unwrap();

    let result = weiback::rate_limit::finish_rate_limited_run(
        &db,
        &FinishRunRequest {
            job_id,
            run_id,
            owner_token: "expired-rate-owner".into(),
            generation: claimed.generation,
            next_status: SyncJobStatus::Interrupted,
            finished_at: "expired".into(),
            stats_json: None,
            error: Some("rate limited".into()),
        },
        &RateLimitInfo {
            scope: RateLimitScope::Endpoint,
            retry_after_ms: Some(1000),
        },
        2,
    )
    .await;
    assert!(result.is_err());
    let stored = get_sync_job(&db, job_id).await.unwrap().unwrap();
    assert_eq!(stored.status, "running");
    assert_eq!(stored.recovery_count, 0);
    assert!(
        get_rate_limit_gate(&db, account_id, "collect_user_posts")
            .await
            .unwrap()
            .is_none()
    );
    let run_status: String = sqlx::query_scalar("SELECT status FROM sync_runs WHERE id=?")
        .bind(run_id)
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(run_status, "running");
}

#[tokio::test]
async fn session_ref_rejects_absolute_parent_and_windows_escape_paths() {
    let root = tempdir().unwrap();
    let missing_root = root.path().join("missing-root");
    let missing_root_resolver =
        secure_account_session_resolver(missing_root, SpawnOptions::default());
    assert!(missing_root_resolver(&account("weibo", "x", "session.json")).is_err());
    let resolver =
        secure_account_session_resolver(root.path().to_path_buf(), SpawnOptions::default());
    assert!(resolver(&account("weibo", "x", "sessions/missing.json")).is_err());
    for invalid in [
        "../outside.json",
        "sessions/../../outside.json",
        "sessions\\..\\outside.json",
        "C:\\secret\\session.json",
        "\\\\server\\share\\session.json",
        "/absolute/session.json",
    ] {
        assert!(
            resolver(&account("weibo", "x", invalid)).is_err(),
            "{invalid}"
        );
    }
    std::fs::create_dir(root.path().join("sessions")).unwrap();
    std::fs::write(root.path().join("sessions/account.json"), "{}").unwrap();
    let valid = resolver(&account("weibo", "x", "sessions/account.json")).unwrap();
    let canonical_root = std::fs::canonicalize(root.path()).unwrap();
    assert!(valid.env.iter().any(|(key, value)| {
        key == "WEIBACK_COLLECTOR_SESSION_PATH" && PathBuf::from(value).starts_with(&canonical_root)
    }));

    let outside = tempdir().unwrap();
    let outside_file = outside.path().join("outside.json");
    std::fs::write(&outside_file, "{}").unwrap();
    let link = root.path().join("sessions/escape.json");
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&outside_file, &link).unwrap();
        assert!(resolver(&account("weibo", "x", "sessions/escape.json")).is_err());
    }
    #[cfg(windows)]
    {
        if std::os::windows::fs::symlink_file(&outside_file, &link).is_ok() {
            assert!(resolver(&account("weibo", "x", "sessions/escape.json")).is_err());
        }
    }
}

#[tokio::test]
async fn database_rejects_missing_or_disabled_accounts_for_monitors_jobs_and_gates() {
    let db = create_db_pool_with_url(":memory:").await.unwrap();
    assert!(
        sqlx::query(
            "INSERT INTO rate_limit_gates(account_id,endpoint_key,updated_at) VALUES(999,'x','x')"
        )
        .execute(&db)
        .await
        .is_err()
    );
    let mut disabled = account("weibo", "db-disabled", "sessions/x.json");
    disabled.enabled = false;
    let account_id = save_account(&db, &disabled).await.unwrap();
    assert!(
        sqlx::query("INSERT INTO monitored_users(account_id,uid,created_at) VALUES(?,?,?)")
            .bind(account_id)
            .bind(1)
            .bind("x")
            .execute(&db)
            .await
            .is_err()
    );
    assert!(sqlx::query("INSERT INTO sync_jobs(resource_key,name,kind,status,enabled,created_at,account_id,endpoint_key) VALUES('x','x','collect_user_posts','pending',1,'x',?,'collect_user_posts')")
        .bind(account_id).execute(&db).await.is_err());
}
