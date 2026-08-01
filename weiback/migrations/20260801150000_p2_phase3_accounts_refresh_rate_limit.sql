-- P2 Phase3: account references, tiered refresh, and durable rate-limit gates.

CREATE TABLE accounts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    provider TEXT NOT NULL,
    uid TEXT NOT NULL,
    display_name TEXT,
    session_ref TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL,
    updated_at TEXT,
    UNIQUE(provider, uid)
);

INSERT INTO accounts(provider, uid, display_name, session_ref, enabled, created_at)
VALUES('legacy', 'legacy', 'Legacy account', 'legacy/session.json', 0, '1970-01-01T00:00:00Z');

ALTER TABLE monitored_users RENAME TO monitored_users_legacy;
CREATE TABLE monitored_users (
    account_id INTEGER NOT NULL,
    uid INTEGER NOT NULL,
    screen_name TEXT,
    refresh_strategy TEXT NOT NULL DEFAULT 'cold',
    tier TEXT NOT NULL DEFAULT 'cold' CHECK(tier IN ('hot','warm','cold')),
    interval_secs INTEGER NOT NULL DEFAULT 0 CHECK(interval_secs >= 0),
    jitter_secs INTEGER NOT NULL DEFAULT 0 CHECK(jitter_secs >= 0),
    next_refresh_epoch INTEGER NOT NULL DEFAULT 0 CHECK(next_refresh_epoch >= 0),
    last_refresh_epoch INTEGER,
    enabled INTEGER NOT NULL DEFAULT 1,
    last_refreshed_at TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT,
    UNIQUE(account_id, uid),
    FOREIGN KEY(account_id) REFERENCES accounts(id)
);
INSERT INTO monitored_users(
    account_id, uid, screen_name, refresh_strategy, tier, enabled,
    last_refreshed_at, created_at, updated_at
)
SELECT (SELECT id FROM accounts WHERE provider='legacy' AND uid='legacy'),
       uid, screen_name, refresh_strategy,
       CASE WHEN refresh_strategy IN ('hot','warm','cold') THEN refresh_strategy ELSE 'cold' END,
       0, last_refreshed_at, created_at, updated_at
FROM monitored_users_legacy;
DROP TABLE monitored_users_legacy;
CREATE INDEX idx_monitored_users_due
ON monitored_users(enabled, next_refresh_epoch, account_id, uid);

DROP INDEX idx_sync_jobs_one_active_resource;
DROP INDEX idx_sync_jobs_claim;
ALTER TABLE sync_jobs RENAME TO sync_jobs_phase2;
CREATE TABLE sync_jobs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    kind TEXT NOT NULL,
    priority INTEGER NOT NULL DEFAULT 0,
    schedule_config TEXT,
    enabled INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL,
    updated_at TEXT,
    resource_key TEXT NOT NULL,
    payload_json TEXT,
    status TEXT NOT NULL DEFAULT 'pending'
        CHECK(status IN ('pending', 'running', 'paused', 'interrupted', 'completed', 'failed', 'cancelled')),
    recovery_count INTEGER NOT NULL DEFAULT 0,
    pre_run_recovery_count INTEGER NOT NULL DEFAULT 0 CHECK(pre_run_recovery_count >= 0),
    max_recovery_attempts INTEGER NOT NULL DEFAULT 3 CHECK(max_recovery_attempts >= 0),
    available_at TEXT,
    available_at_epoch INTEGER NOT NULL DEFAULT 0 CHECK(available_at_epoch >= 0),
    claimed_at TEXT,
    owner_token TEXT,
    lease_until_epoch INTEGER,
    current_run_id INTEGER,
    generation INTEGER NOT NULL DEFAULT 0 CHECK(generation >= 0),
    last_error TEXT,
    account_id INTEGER NOT NULL REFERENCES accounts(id),
    endpoint_key TEXT NOT NULL DEFAULT '__legacy__',
    rate_limit_backoff_level INTEGER NOT NULL DEFAULT 0 CHECK(rate_limit_backoff_level >= 0),
    endpoint_gate_revision INTEGER NOT NULL DEFAULT 0 CHECK(endpoint_gate_revision >= 0),
    account_gate_revision INTEGER NOT NULL DEFAULT 0 CHECK(account_gate_revision >= 0)
);
INSERT INTO sync_jobs(
    id, name, kind, priority, schedule_config, enabled, created_at, updated_at,
    resource_key, payload_json, status, recovery_count, pre_run_recovery_count, max_recovery_attempts,
    available_at, available_at_epoch, claimed_at, owner_token, lease_until_epoch,
    current_run_id, generation, last_error, account_id
)
SELECT id, name, kind, priority, schedule_config,
       CASE WHEN status IN ('pending','running','paused','interrupted') THEN 0 ELSE enabled END,
       created_at, updated_at, resource_key, payload_json,
       CASE WHEN status IN ('pending','running','paused','interrupted') THEN 'failed' ELSE status END,
       recovery_count, pre_run_recovery_count, max_recovery_attempts,
       available_at, available_at_epoch, NULL, NULL, NULL, NULL, generation,
       CASE WHEN status IN ('pending','running','paused','interrupted')
            THEN COALESCE(last_error, 'legacy job archived: account and payload require explicit review')
            ELSE last_error END,
       (SELECT id FROM accounts WHERE provider='legacy' AND uid='legacy')
FROM sync_jobs_phase2;
DROP TABLE sync_jobs_phase2;
CREATE UNIQUE INDEX idx_sync_jobs_one_active_resource
ON sync_jobs(resource_key)
WHERE status IN ('pending', 'running', 'paused', 'interrupted');
CREATE INDEX idx_sync_jobs_claim
ON sync_jobs(status, enabled, available_at_epoch, priority DESC, id ASC);
CREATE INDEX idx_sync_jobs_account_endpoint
ON sync_jobs(account_id, endpoint_key, status, available_at_epoch, priority DESC, id ASC);

CREATE TABLE rate_limit_gates (
    account_id INTEGER NOT NULL,
    endpoint_key TEXT NOT NULL,
    next_allowed_epoch INTEGER NOT NULL DEFAULT 0 CHECK(next_allowed_epoch >= 0),
    backoff_level INTEGER NOT NULL DEFAULT 0 CHECK(backoff_level >= 0),
    retry_after_epoch INTEGER,
    updated_at TEXT NOT NULL,
    updated_at_epoch INTEGER NOT NULL DEFAULT 0 CHECK(updated_at_epoch >= 0),
    revision INTEGER NOT NULL DEFAULT 0 CHECK(revision >= 0),
    PRIMARY KEY(account_id, endpoint_key),
    FOREIGN KEY(account_id) REFERENCES accounts(id)
);
CREATE INDEX idx_rate_limit_gates_next
ON rate_limit_gates(next_allowed_epoch, account_id, endpoint_key);

CREATE TRIGGER validate_monitored_user_account_insert
BEFORE INSERT ON monitored_users
WHEN NEW.enabled=1 AND NOT EXISTS(SELECT 1 FROM accounts WHERE id=NEW.account_id AND enabled=1)
BEGIN
    SELECT RAISE(ABORT, 'monitored user requires enabled account');
END;
CREATE TRIGGER validate_monitored_user_account_update
BEFORE UPDATE OF account_id, enabled ON monitored_users
WHEN NEW.enabled=1 AND NOT EXISTS(SELECT 1 FROM accounts WHERE id=NEW.account_id AND enabled=1)
BEGIN
    SELECT RAISE(ABORT, 'monitored user requires enabled account');
END;

CREATE TRIGGER validate_sync_job_account_insert
BEFORE INSERT ON sync_jobs
WHEN NEW.enabled=1 AND NOT EXISTS(SELECT 1 FROM accounts WHERE id=NEW.account_id AND enabled=1)
BEGIN
    SELECT RAISE(ABORT, 'sync job requires enabled account');
END;
CREATE TRIGGER validate_sync_job_account_update
BEFORE UPDATE OF account_id, enabled ON sync_jobs
WHEN NEW.enabled=1 AND NOT EXISTS(SELECT 1 FROM accounts WHERE id=NEW.account_id AND enabled=1)
BEGIN
    SELECT RAISE(ABORT, 'sync job requires enabled account');
END;

CREATE TRIGGER validate_rate_limit_account_insert
BEFORE INSERT ON rate_limit_gates
WHEN NOT EXISTS(SELECT 1 FROM accounts WHERE id=NEW.account_id AND enabled=1)
BEGIN
    SELECT RAISE(ABORT, 'rate-limit gate requires enabled account');
END;
CREATE TRIGGER validate_rate_limit_account_update
BEFORE UPDATE OF account_id ON rate_limit_gates
WHEN NOT EXISTS(SELECT 1 FROM accounts WHERE id=NEW.account_id AND enabled=1)
BEGIN
    SELECT RAISE(ABORT, 'rate-limit gate requires enabled account');
END;

CREATE TRIGGER disable_account_dependents
AFTER UPDATE OF enabled ON accounts
WHEN OLD.enabled=1 AND NEW.enabled=0
BEGIN
    UPDATE monitored_users SET enabled=0, updated_at=COALESCE(NEW.updated_at, updated_at)
    WHERE account_id=NEW.id AND enabled=1;
    UPDATE sync_runs SET status='paused', finished_at=COALESCE(NEW.updated_at, updated_at),
        updated_at=COALESCE(NEW.updated_at, updated_at),
        error=COALESCE(error, 'account disabled')
    WHERE id IN (SELECT current_run_id FROM sync_jobs
        WHERE account_id=NEW.id AND status='running' AND current_run_id IS NOT NULL)
      AND status='running';
    UPDATE sync_jobs SET enabled=0,
        status=CASE WHEN status='running' THEN 'paused' ELSE status END,
        generation=CASE WHEN status='running' THEN generation+1 ELSE generation END,
        owner_token=NULL,current_run_id=NULL,lease_until_epoch=NULL,
        last_error=CASE WHEN status='running' THEN 'account disabled' ELSE last_error END,
        updated_at=COALESCE(NEW.updated_at, updated_at)
    WHERE account_id=NEW.id AND enabled=1;
END;
