/**
 * The renderer half of crash-recovery copies (same file in the Markdown and
 * HTML editors). About every 30 s, while the document has unsaved changes,
 * the text a save would write is handed to the main process, which keeps it
 * as the document's recovery copy (and skips a text it already has). This
 * runs whether or not AutoSave is on. A failed write skips the next few
 * ticks, then tries again; copies are never switched off for good.
 */

/** time between copy attempts */
export const RECOVERY_COPY_INTERVAL_MS = 30_000
/** ticks skipped after a failed write (as slides' autosave backoff) */
export const RECOVERY_COPY_BACKOFF_TICKS = 10

export interface RecoveryCopyLoopOptions {
  /**
   * The exact text a save would write now, or null when there is nothing to
   * keep (still loading, no unsaved changes, a save in flight).
   */
  read: () => string | null
  /** hand the text to the main process; rejects when the copy could not be written */
  write: (text: string | null, token?: string, error?: string) => Promise<unknown>
  /** the main process asks for a copy now (the OS is ending the session) */
  onFlushRequest?: (handler: (token?: string) => void) => () => void
  intervalMs?: number
  backoffTicks?: number
}

/** start the copy loop; the returned function stops it */
export function startRecoveryCopies(options: RecoveryCopyLoopOptions): () => void {
  const backoffTicks = options.backoffTicks ?? RECOVERY_COPY_BACKOFF_TICKS
  let skip = 0
  let stopped = false
  let chain = Promise.resolve()
  let periodicQueued = false

  const run = (flush: boolean, token?: string): void => {
    if (stopped) return
    if (!flush && periodicQueued) return
    if (!flush && skip > 0) {
      skip -= 1
      return
    }
    if (!flush) periodicQueued = true
    chain = chain.then(async () => {
      if (stopped) return
      try {
        const text = options.read()
        if (text === null && !flush) return
        await options.write(text, token)
        skip = 0
      } catch (error) {
        console.warn('[recovery] copy not written, retrying later:', error)
        skip = backoffTicks
        if (flush && token !== undefined) {
          try {
            await options.write(null, token, String(error))
          } catch {
            // The host may already have settled the matching failed write.
          }
        }
      }
    }).finally(() => {
      if (!flush) periodicQueued = false
    })
  }

  const timer = setInterval(() => run(false), options.intervalMs ?? RECOVERY_COPY_INTERVAL_MS)
  const offFlush = options.onFlushRequest?.((token) => run(true, token))
  return () => {
    stopped = true
    clearInterval(timer)
    offFlush?.()
  }
}
