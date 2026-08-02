export type LegacySourceKind = 'rust_v1' | 'python_v2'

export interface LegacyDetection {
  kind: LegacySourceKind
  db_path: string
  schema_version: number
  media_root: string
  picture_dir: string | null
  video_dir: string | null
}

export type LegacyImportStatus = 'completed' | 'already_completed' | 'partial_recoverable'

export interface LegacyImportSummary {
  source: LegacyDetection
  status: LegacyImportStatus
  posts: number
  users: number
  media_copied: number
  media_pending: number
  rollback_backup: string
  diagnostic_code: string | null
}
