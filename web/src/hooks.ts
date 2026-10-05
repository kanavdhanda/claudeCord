import { useCallback, useEffect, useRef, useState } from 'react'

/** Loads something now and again every `every` ms while the tab is visible. Returns the data, the error, and a way to reload. */
export function useLoad<T>(load: () => Promise<T>, every = 0, key: unknown = null) {
  const [data, setData] = useState<T | null>(null)
  const [error, setError] = useState<Error | null>(null)
  const loadRef = useRef(load)
  loadRef.current = load
  const reload = useCallback(async () => {
    try {
      setData(await loadRef.current())
      setError(null)
    } catch (e) {
      setError(e as Error)
    }
  }, [])
  // `key` lets a caller reload at once when what it asks for changes (a different range, say).
  useEffect(() => {
    reload()
    if (!every) return
    const t = setInterval(() => document.visibilityState === 'visible' && reload(), every)
    return () => clearInterval(t)
  }, [reload, every, key])
  return { data, error, reload }
}

/** Runs an action and tracks whether it is running and what went wrong, for buttons. */
export function useAction() {
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const run = useCallback(async <T,>(f: () => Promise<T>): Promise<T | undefined> => {
    setBusy(true)
    setError(null)
    try {
      return await f()
    } catch (e) {
      setError((e as Error).message)
      return undefined
    } finally {
      setBusy(false)
    }
  }, [])
  return { busy, error, run, setError }
}

export function ago(ms: number | null | undefined): string {
  if (!ms) return 'never'
  const s = Math.max(0, Math.round((Date.now() - ms) / 1000))
  if (s < 10) return 'just now'
  if (s < 60) return `${s}s ago`
  if (s < 3600) return `${Math.round(s / 60)}m ago`
  if (s < 86400) return `${Math.round(s / 3600)}h ago`
  return `${Math.round(s / 86400)}d ago`
}
