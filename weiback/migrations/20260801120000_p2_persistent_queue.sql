-- P2 phase 1: turn the P0-C sync tables into a durable SQLite queue.

ALTER TABLE sync_jobs ADD COLUMN resource_key TEXT NOT NULL DEFAULT '';
ALTER TABLE sync_jobs ADD COLUMN payload_json TEXT;
ALTER TABLE sync_jobs ADD COLUMN status TEXT NOT NULL DEFAULT 'pending'
CHECK(status IN ('pending', 'running', 'paused', 'interrupted', 'completed', 'failed', 'cancelled'));
ALTER TABLE sync_jobs ADD COLUMN recovery_count INTEGER NOT NULL DEFAULT 0;
ALTER TABLE sync_jobs ADD COLUMN pre_run_recovery_count INTEGER NOT NULL DEFAULT 0
CHECK(pre_run_recovery_count >= 0);
ALTER TABLE sync_jobs ADD COLUMN max_recovery_attempts INTEGER NOT NULL DEFAULT 3
CHECK(max_recovery_attempts >= 0);
ALTER TABLE sync_jobs ADD COLUMN available_at TEXT;
ALTER TABLE sync_jobs ADD COLUMN available_at_epoch INTEGER NOT NULL DEFAULT 0
CHECK(available_at_epoch >= 0);
ALTER TABLE sync_jobs ADD COLUMN claimed_at TEXT;
ALTER TABLE sync_jobs ADD COLUMN owner_token TEXT;
ALTER TABLE sync_jobs ADD COLUMN lease_until_epoch INTEGER;
ALTER TABLE sync_jobs ADD COLUMN current_run_id INTEGER;
ALTER TABLE sync_jobs ADD COLUMN generation INTEGER NOT NULL DEFAULT 0
CHECK(generation >= 0);
ALTER TABLE sync_jobs ADD COLUMN last_error TEXT;

UPDATE sync_jobs
SET resource_key = 'legacy:' || id
WHERE resource_key = '';

CREATE UNIQUE INDEX idx_sync_jobs_one_active_resource
ON sync_jobs(resource_key)
WHERE status IN ('pending', 'running', 'paused', 'interrupted');
CREATE INDEX idx_sync_jobs_claim
ON sync_jobs(status, enabled, available_at_epoch, priority DESC, id ASC);

ALTER TABLE sync_runs ADD COLUMN attempt INTEGER NOT NULL DEFAULT 0;
ALTER TABLE sync_runs ADD COLUMN updated_at TEXT;
ALTER TABLE sync_runs ADD COLUMN owner_token TEXT;
ALTER TABLE sync_runs ADD COLUMN generation INTEGER NOT NULL DEFAULT 0;
ALTER TABLE sync_runs ADD COLUMN lease_until_epoch INTEGER;

-- P0-C allowed duplicate active history. Archive it before adding the invariant.
UPDATE sync_runs
SET status = 'interrupted',
    finished_at = COALESCE(finished_at, started_at),
    updated_at = COALESCE(updated_at, finished_at, started_at),
    error = COALESCE(error, 'archived during persistent queue migration')
WHERE status = 'running';

-- P0-C jobs had no trustworthy ownership linkage to those runs.
UPDATE sync_jobs
SET status = 'pending',
    owner_token = NULL,
    lease_until_epoch = NULL,
    current_run_id = NULL,
    generation = 0,
    claimed_at = NULL
WHERE owner_token IS NOT NULL
   OR lease_until_epoch IS NOT NULL
   OR current_run_id IS NOT NULL
   OR status = 'running';

CREATE INDEX idx_sync_runs_history
ON sync_runs(job_id, id DESC);
CREATE UNIQUE INDEX idx_sync_runs_one_running_per_job
ON sync_runs(job_id) WHERE status = 'running';

ALTER TABLE sync_checkpoints ADD COLUMN job_id INTEGER;
ALTER TABLE sync_checkpoints ADD COLUMN run_id INTEGER;
ALTER TABLE sync_checkpoints ADD COLUMN generation INTEGER;
ALTER TABLE sync_checkpoints ADD COLUMN owner_token TEXT;
CREATE INDEX idx_sync_checkpoints_job_id
ON sync_checkpoints(job_id);

CREATE TRIGGER validate_sync_runs_status_insert
BEFORE INSERT ON sync_runs
WHEN NEW.status NOT IN ('pending', 'running', 'paused', 'interrupted', 'completed', 'failed', 'cancelled')
BEGIN
    SELECT RAISE(ABORT, 'invalid sync run status');
END;

CREATE TRIGGER validate_sync_runs_status_update
BEFORE UPDATE OF status ON sync_runs
WHEN NEW.status NOT IN ('pending', 'running', 'paused', 'interrupted', 'completed', 'failed', 'cancelled')
BEGIN
    SELECT RAISE(ABORT, 'invalid sync run status');
END;
