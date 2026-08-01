export type RefreshTier = 'hot' | 'warm' | 'cold'

export interface SyncAccount {
  id: string
  provider: string
  uid: string
  display_name: string | null
  enabled: boolean
  has_session: boolean
  created_at: string
  updated_at: string | null
}

export interface SaveSyncAccountInput {
  id?: string
  provider: string
  uid: string
  display_name: string | null
  session_ref?: string
  enabled: boolean
}

export interface MonitoredUser {
  account_id: string
  uid: string
  screen_name: string | null
  refresh_strategy: RefreshTier
  enabled: boolean
  last_refreshed_at: string | null
  created_at: string
  updated_at: string | null
  tier: RefreshTier
  interval_secs: string
  jitter_secs: string
  next_refresh_epoch: string
  last_refresh_epoch: string | null
}

export type SyncJobSpec =
  | { kind: 'collect_user_posts'; account_id: string; uid: string; max_pages: number | null; priority: number }
  | { kind: 'collect_comments'; account_id: string; post_id: string; max_pages: number | null; priority: number }
  | { kind: 'collect_comment_replies'; account_id: string; post_id: string; root_comment_id: string; max_pages: number | null; priority: number }

export interface SyncJob {
  id: string
  resource_key: string
  name: string
  kind: string
  status: string
  priority: string
  schedule_config: string | null
  enabled: boolean
  recovery_count: string
  max_recovery_attempts: string
  available_at: string | null
  available_at_epoch: string
  claimed_at: string | null
  current_run_id: string | null
  created_at: string
  updated_at: string | null
  account_id: string
  endpoint_key: string
}

export interface SyncRun {
  id: string
  job_id: string
  status: string
  started_at: string
  finished_at: string | null
  stats_json: string | null
  attempt: string
  updated_at: string | null
}

export type WorkerStopResult =
  | { status: 'stopped'; detail: { pid: number } }
  | { status: 'worker_not_found' }
  | { status: 'worker_starting' }
  | { status: 'stop_timed_out'; detail: { pid: number } }
  | { status: 'stop_failed'; detail: string }

export interface SyncJobControlOutcome {
  job: SyncJob
  worker_stop: WorkerStopResult
}

export interface SaveMonitoredUserInput {
  account_id: string
  uid: string
  screen_name: string | null
  refresh_strategy: RefreshTier
  enabled: boolean
  tier: RefreshTier
  interval_secs: number
  jitter_secs: number
}
