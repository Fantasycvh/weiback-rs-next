import { useEffect } from 'react'

export function useVisiblePolling(callback: () => void | Promise<void>, intervalMs: number): void {
  useEffect(() => {
    const run = () => {
      if (document.visibilityState === 'visible') void callback()
    }
    const timer = window.setInterval(run, intervalMs)
    document.addEventListener('visibilitychange', run)
    return () => {
      window.clearInterval(timer)
      document.removeEventListener('visibilitychange', run)
    }
  }, [callback, intervalMs])
}
