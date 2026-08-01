import { invoke } from '@tauri-apps/api/core'
import {
  User,
  Task,
  TaskError,
  PaginatedPostInfo,
  PostQuery,
  ExportJobOptions,
  BackupType,
  ResolutionPolicy,
  CleanupInvalidPostsOptions,
  DeletePostOptions,
  MonitoredUser,
  SyncAccount,
  SaveMonitoredUserInput,
  SaveSyncAccountInput,
  SyncJob,
  SyncJobControlOutcome,
  SyncJobSpec,
  SyncRun,
} from '../types'
import { Config } from '../types/config'
import { LegacyDetection } from '../types/legacy'

// Backend
export type BackendStatus =
  | { status: 'Uninitialized' }
  | { status: 'Running'; warning?: string }
  | { status: 'Error'; message: string }

export const getBackendStatus = () => invoke<BackendStatus>('get_backend_status')
export const initBackend = () => invoke<BackendStatus>('init_backend')

// Auth
export const loginState = () => invoke<User | null>('login_state')
export const getSmsCode = (phoneNumber: string) => invoke('get_sms_code', { phoneNumber })
export const login = (smsCode: string) => invoke<User>('login', { smsCode })

// Tasks
export const getCurrentTaskStatus = () => invoke<Task | null>('get_current_task_status')
export const getAndClearTaskErrors = () => invoke<TaskError[]>('get_and_clear_task_errors')

// Persistent sync
export const getSyncAccounts = () => invoke<SyncAccount[]>('get_sync_accounts')
export const saveSyncAccount = (input: SaveSyncAccountInput) =>
  invoke<string>('save_sync_account', { input })
export const deleteSyncAccount = (id: string) => invoke<boolean>('delete_sync_account', { id })
export const getMonitoredUsers = () => invoke<MonitoredUser[]>('get_monitored_users')
export const saveMonitoredUser = (input: SaveMonitoredUserInput) =>
  invoke<void>('save_monitored_user', { input })
export const deleteMonitoredUser = (accountId: string, uid: string) =>
  invoke<boolean>('delete_monitored_user', { accountId, uid })
export const enqueueSyncJob = (spec: SyncJobSpec) => invoke<string>('enqueue_sync_job', { spec })
export const getSyncJobs = () => invoke<SyncJob[]>('get_sync_jobs')
export const getSyncRunHistory = (jobId: string, limit = 100) =>
  invoke<SyncRun[]>('get_sync_run_history', { jobId, limit })
export const pauseSyncJob = (jobId: string) =>
  invoke<SyncJobControlOutcome>('pause_sync_job', { jobId })
export const resumeSyncJob = (jobId: string) => invoke<SyncJob>('resume_sync_job', { jobId })
export const cancelSyncJob = (jobId: string) =>
  invoke<SyncJobControlOutcome>('cancel_sync_job', { jobId })
export const retrySyncJob = (jobId: string) => invoke<SyncJob>('retry_sync_job', { jobId })

// Backup
export const backupUser = (uid: string, numPages: number, backupType: BackupType) =>
  invoke('backup_user', { uid, numPages, backupType })
export const backupFavorites = (numPages: number) => invoke('backup_favorites', { numPages })
export const unfavoritePosts = () => invoke('unfavorite_posts')
export const rebackupPosts = (query: PostQuery) => invoke('rebackup_posts', { query })
export const rebackupMissingImages = (query: PostQuery) =>
  invoke('rebackup_missing_images', { query })

// Posts
export const queryLocalPosts = (query: PostQuery) =>
  invoke<PaginatedPostInfo>('query_local_posts', { query })
export const deletePost = (options: DeletePostOptions) => invoke('delete_post', { options })
export const rebackupPost = (id: string) => invoke('rebackup_post', { id })

// Users
export const getUsernameById = (uid: string) => invoke<string | null>('get_username_by_id', { uid })
export const searchIdByUsernamePrefix = (prefix: string) =>
  invoke<User[]>('search_id_by_username_prefix', { prefix })

// Export
export const exportPosts = (options: ExportJobOptions) => invoke('export_posts', { options })

// Pictures
export const getPictureBlob = (id: string) => invoke<ArrayBuffer>('get_picture_blob', { id })
export const getVideoBlob = (url: string) => invoke<ArrayBuffer>('get_video_blob', { url })
export const cleanupPictures = (policy: ResolutionPolicy) =>
  invoke('cleanup_pictures', { options: { policy } })
export const cleanupOutdatedAvatars = () => invoke('cleanup_outdated_avatars')
export const cleanupInvalidPosts = (options: CleanupInvalidPostsOptions) =>
  invoke('cleanup_invalid_posts', { options })
export const cleanupInvalidPictures = () => invoke('cleanup_invalid_pictures')

// Config
export const getConfig = () => invoke<Config>('get_config_command')
export const setConfig = (config: Config) => invoke('set_config_command', { config })

// Legacy
export const detectLegacySources = () => invoke<LegacyDetection[]>('detect_legacy_sources')
