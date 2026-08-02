const SECRET_KEY = /(access[_-]?token|authorization|cookie|gsid|passport|password|secret|session)/i

export function redactDiagnostic(value: unknown, key = ''): unknown {
  if (SECRET_KEY.test(key)) return '[已脱敏]'
  if (Array.isArray(value)) return value.map(item => redactDiagnostic(item))
  if (value && typeof value === 'object') {
    return Object.fromEntries(
      Object.entries(value).map(([childKey, childValue]) => [
        childKey,
        redactDiagnostic(childValue, childKey),
      ])
    )
  }
  if (typeof value === 'string') {
    return value
      .replace(/(bearer\s+)[\w.+/=-]+/gi, '$1[已脱敏]')
      .replace(/([?&](?:token|session|cookie|auth)=[^&\s]+)/gi, '[已脱敏]')
  }
  return value
}

export function parseRunStats(statsJson: string | null): Record<string, unknown> | null {
  if (!statsJson) return null
  try {
    const value: unknown = JSON.parse(statsJson)
    return value && typeof value === 'object' && !Array.isArray(value)
      ? (value as Record<string, unknown>)
      : null
  } catch {
    return null
  }
}

export interface RunCounters {
  fetchedCount: number | null
  pages: number | null
}

function nonNegativeCount(value: unknown): number | null {
  const count = typeof value === 'number' ? value : Number(value)
  return Number.isFinite(count) && count >= 0 ? count : null
}

export function getRunCounters(stats: Record<string, unknown> | null): RunCounters {
  return {
    fetchedCount: nonNegativeCount(stats?.fetched_count),
    pages: nonNegativeCount(stats?.pages),
  }
}

export function isTerminalJobStatus(status: string): boolean {
  return ['completed', 'failed', 'cancelled'].includes(status.toLowerCase())
}

export function formatEpoch(epoch: string | null | undefined): string {
  if (!epoch) return '未安排'
  const value = Number(epoch)
  return Number.isFinite(value) ? new Date(value * 1000).toLocaleString() : epoch
}
