export type LegacySourceKind = 'rust_v1' | 'python_v2'

export interface LegacyDetection {
  kind: LegacySourceKind
  db_path: string
  schema_version: number
  media_root: string
  picture_dir: string | null
  video_dir: string | null
}
