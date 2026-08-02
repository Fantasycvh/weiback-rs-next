import { User } from './user'

// --- From PostDisplay ---
export interface UrlStructItem {
  long_url: string | null
  short_url: string
  url_title: string
}

export interface TagStructItem {
  tag_name: string
  url_type_pic: string
  otype?: string
  tag_hidden?: number
  ori_url?: string
  desc?: string
}

export interface Post {
  id: string
  idstr: string
  text: string
  favorited: boolean
  created_at: string
  user: User | null
  retweeted_status?: Post | null
  url_struct: UrlStructItem[] | null
  tag_struct?: TagStructItem[] | null
}

export type AttachedImage =
  | { type: 'livephoto'; data: { id: string; video_url: string } }
  | { type: 'video_cover'; data: { id: string; video_url: string } }
  | { type: 'article_cover'; data: { id: string; title: string; url: string } }
  | { type: 'normal'; data: { id: string } }

export interface PostInfo {
  post: Post
  avatar_id: string | null
  emoji_map: Record<string, string>
  standalone_pics: AttachedImage[]
  inline_map: Record<string, string>
}

// --- From LocalExport ---
export interface PaginatedPostInfo {
  posts: PostInfo[]
  total_items: number
}

export type SearchTerm = { Fuzzy: string } | { Strict: string }

export interface PostFilter {
  startDate: Date | null
  endDate: Date | null
  isFavorited: boolean
  reverseOrder: boolean
  searchTerm: string
  searchMode: 'fuzzy' | 'strict'
  userInput: User | string | null
  contentType: string
  contentStatus: string
  source: string
}

export interface PostQuery {
  user_id?: string
  start_date?: number // Unix timestamp
  end_date?: number // Unix timestamp
  search_term?: SearchTerm
  is_favorited: boolean
  reverse_order: boolean
  page: number
  posts_per_page: number
  content_type?: string
  content_status?: string
  source?: string
}

export interface CommentItem {
  id: string
  post_id: string
  root_id: string | null
  parent_id: string | null
  user_id: string | null
  text: string
  created_at: string
  depth: number
  child_count: number
  like_count: number
  source: string | null
  content_status: string
  deleted: boolean
}

export interface PaginatedComments {
  items: CommentItem[]
  total_items: string
  offset: number
  limit: number
}

export interface OwnerMedia {
  id: string
  owner_type: string
  owner_id: string
  media_type: string
  remote_url: string | null
  local_available: boolean
  status: 'pending' | 'downloading' | 'downloaded' | 'failed'
  retry_count: string
  created_at: string
  updated_at: string | null
  definition: string | null
}

export interface MediaBlob {
  content_type: string
  bytes: number[] | Uint8Array
}

export interface ExportOutputConfig {
  task_name: string
  export_dir: string
}

export interface ExportJobOptions {
  query: PostQuery
  output: ExportOutputConfig
}
