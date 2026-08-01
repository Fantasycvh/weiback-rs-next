import { fetch } from '@tauri-apps/plugin-http'
import { openUrl } from '@tauri-apps/plugin-opener'

const GITHUB_REPO = import.meta.env.VITE_GITHUB_REPO?.trim()
export const UPDATE_SOURCE_CONFIGURED = Boolean(GITHUB_REPO)
const PROJECT_URL = GITHUB_REPO ? `https://github.com/${GITHUB_REPO}` : null
const RELEASE_URL = PROJECT_URL ? `${PROJECT_URL}/releases/latest` : null

export interface ReleaseInfo {
  tag_name: string
  html_url: string
  body: string
  published_at: string
}

export const checkLatestRelease = async (): Promise<ReleaseInfo | null> => {
  if (!GITHUB_REPO) return null

  try {
    const res = await fetch(`https://api.github.com/repos/${GITHUB_REPO}/releases/latest`, {
      method: 'GET',
      headers: { Accept: 'application/vnd.github+json' },
    })
    if (!res.ok) return null
    const data = await res.json()
    return data as ReleaseInfo
  } catch {
    return null
  }
}

export const openReleasePage = () => (RELEASE_URL ? openUrl(RELEASE_URL) : Promise.resolve())
export const openProjectPage = () => (PROJECT_URL ? openUrl(PROJECT_URL) : Promise.resolve())
