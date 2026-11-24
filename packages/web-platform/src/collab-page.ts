import type { CollabAdapter, CollabSession, CollabStatus, Json } from '@seuoffice/collab/client'
import { collabLink, reportCollabStatus } from './collab-link'
import { editMode } from './document-store'
import {
  holdOpened,
  hostHistory,
  onHostExportRequest,
  onHostSaveRequest,
  reply,
  reportDirty,
  reportError,
  saveToHost,
  type SaveReason,
} from './embed-channel'
import { presenceMarks, type PresenceMark } from './presence'

export interface PageCollab<Handles> {
  /** the bytes the editor must open: the epoch base, which may be a peer's file rather than the host's */
  baseBytes: Uint8Array
  /** the renderer calls this once the editor shows `baseBytes`; later calls do nothing */
  attach(handles: Handles): Promise<void>
  /**
   * What a host save request does: the file of one exact version, stored by
   * the host. `skipWhenSaved`: nothing is stored when the host holds the head already.
   */
  save(how: { reason: SaveReason; skipWhenSaved?: boolean }): Promise<void>
  /** this page as the session names it (the host's display name for this user); null before the attach */
  self(): { client: string; name: string } | null
}

declare global {
  interface Window {
    /** set while the page co-edits: the renderer's one way into the session */
    seuCollab?: {
      attach(handles: unknown): Promise<void>
      /**
       * Whether the document has edits no stored file holds, for the editor's
       * unsaved mark: the app's own dirty flags stay set after a co-edited
       * save, since its next save is built against the session's base file.
       */
      unsaved?(): boolean
      /**
       * The renderer will never attach (its document did not load in full):
       * the page leaves the session, takes input again and tells the host
       * `collab_unavailable` with `reason`. Nothing once the editor attached.
       */
      fail?(reason: string): void
    }
  }
}

/**
 * Join the host's co-editing session before the editor opens anything.
 * Null when the host did not ask for co-editing; rejects with the engine's
 * `CollabError` when the session cannot be joined.
 *
 * From the join on, a host save request is answered by the session: the
 * engine freezes input, confirms every local edit, and produces the file of
 * one exact version, which goes to the host with that version and is then
 * reported to the server as stored. The app's own save handler is replaced:
 * a co-edited document must never be saved from a state the log does not hold.
 * The host's unsaved flag follows the session (edits in the log that no stored
 * file holds), so the app must not report its own. The host hears `opened`
 * only once the editor attached: when the adapter cannot be built, or the
 * renderer gives up (`fail`), the page leaves the session and reports
 * `collab_unavailable` instead.
 *
 * A host's `collab_export` is answered with the file of exactly what the page
 * shows (`exportLocal`), in any phase: the way to keep edits a failed or
 * offline session cannot confirm.
 */
export async function joinPageCollab<Handles, Op>(options: {
  format: { name: string; schema: number }
  fileName: string
  bytes: Uint8Array
  /** may settle later, for an editor that has to finish showing the document first */
  adapter(handles: Handles): CollabAdapter<Op> | Promise<CollabAdapter<Op>>
  /** hears the session's status once the editor attached and on every change, for an editor that shows it */
  status?(status: CollabStatus): void
  /** Attach selection listeners only to a live editor, never to a history replay. */
  presence?(
    handles: Handles,
    publish: () => void,
  ): {
    read(): Json
    draw(peers: readonly PresenceMark[]): void
  }
  /**
   * How a host save request runs; `save` is the session's save. An editor passes its own flow
   * here so that what a person still has open (a text box, a comment being typed) is committed
   * first. The flow must end in `save` or in `PageCollab.save`. `ownerId` marks the saves the
   * flow starts as the host's: an editor that runs its own save flow hands it to that flow, which
   * reaches `PageCollab.save` under `saveReason(ownerId, …)`.
   */
  hostSave?(save: () => Promise<void>, ownerId: string): Promise<void>
  /**
   * Commit what a person still has open, before a `collab_export` while the session is live.
   * Not `hostSave`, whose flow ends in a real save. A failed session's editor is frozen for good.
   */
  commitDrafts?(): void | Promise<void>
  /**
   * Input the page takes before the editor attached, by its target: what an editor must ask
   * before it can show the document at all (a password). Nothing of the document is editable there.
   */
  inputBeforeAttach?(target: EventTarget | null): boolean
}): Promise<PageCollab<Handles> | null> {
  const history = hostHistory()
  if (history) {
    // Ordinary viewers must not load the collaboration engine.
    const { replayHistory, HistoryError } = await import('@seuoffice/collab/client')
    const answer = { type: 'history_done', requestId: history.requestId }
    let finished = false
    let attaching: Promise<void> | undefined
    const fail = (message: string, code = 'unsupported'): void => {
      if (finished) return
      finished = true
      releaseInput()
      reply({ ...answer, ok: false, code, message })
    }
    const attach = (handles: Handles): Promise<void> =>
      (attaching ??= (async () => {
        if (finished) return
        try {
          const adapter = await options.adapter(handles)
          if (finished) return
          const result = await replayHistory(
            adapter,
            options.format,
            options.bytes,
            history.request,
          )
          if (finished) return
          const { bytes, fileName, compensation } = result
          const buffer =
            bytes.buffer instanceof ArrayBuffer && bytes.byteLength === bytes.buffer.byteLength
              ? bytes.buffer
              : new Uint8Array(bytes).buffer
          reply({ ...answer, ok: true, fileName, bytes: buffer, compensation }, [buffer])
          finished = true
          releaseInput()
        } catch (error) {
          fail(
            error instanceof Error ? error.message : String(error),
            error instanceof HistoryError ? error.code : 'unsupported',
          )
        }
      })())
    window.seuCollab = {
      attach: (handles) => attach(handles as Handles),
      fail,
      unsaved: () => false,
    }
    const releaseInput = holdInput()
    return {
      baseBytes: options.bytes,
      attach,
      save: async () => {
        throw new Error('a history render cannot save')
      },
      self: () => null,
    }
  }
  const link = collabLink()
  if (!link) return null
  // pages that never co-edit never load the engine
  const { joinCollab } = await import('@seuoffice/collab/client')
  const join = await joinCollab(link, { format: options.format, bytes: options.bytes })
  let session: CollabSession | null = null
  let attaching: Promise<void> | null = null
  /** `left`: the adapter could not be built, or the renderer gave up; nothing attaches after it */
  let stage: 'waiting' | 'attached' | 'left' = 'waiting'
  const releaseOpened = holdOpened()
  // Until the adapter attached, what the editor shows must stay the epoch base: an edit made
  // now would be counted as base by this page alone, and every peer's step would land shifted
  window.seuCollab = {
    attach: (handles) => attach(handles as Handles),
    // a viewer stores nothing, so peers' edits are not its unsaved changes
    unsaved: () => {
      const status = session?.status()
      return !!status?.canEdit && (status.version > status.saved || status.unconfirmed > 0)
    },
    fail: (reason) => leave(reason),
  }
  const releaseInput = holdInput(options.inputBeforeAttach)

  /** never edit a document the log cannot follow: leave the session and let the host reopen */
  const leave = (reason: string): void => {
    if (stage !== 'waiting') return
    stage = 'left'
    releaseInput()
    link.close()
    reportError('collab_unavailable', reason)
    releaseOpened(false)
  }

  const save = async (how: { reason: SaveReason; skipWhenSaved?: boolean }): Promise<void> => {
    if (!session) throw new Error('the document is not open yet')
    const status = session.status()
    // the app's own repeated saves (its autosave) store nothing new
    if (how.skipWhenSaved && status.version === status.saved && status.unconfirmed === 0) return
    const saved = await session.save()
    await saveToHost(options.fileName, saved.bytes, how.reason, { version: saved.version })
    session.reportSaved(saved.version)
  }
  // a viewer's host never stores a save, so a save request finds no handler
  if (editMode()) {
    const { hostSave } = options
    onHostSaveRequest((ownerId) =>
      hostSave ? hostSave(() => save({ reason: 'host' }), ownerId) : save({ reason: 'host' }),
    )
  }
  onHostExportRequest(async () => {
    if (!session) throw new Error('the document is not open yet')
    if (session.status().phase !== 'failed') await options.commitDrafts?.()
    return session.exportLocal()
  })

  const attach = (handles: Handles): Promise<void> => {
    attaching ??= (async () => {
      if (stage !== 'waiting') return
      let adapter: CollabAdapter<Op>
      try {
        adapter = await options.adapter(handles)
      } catch (error) {
        leave(error instanceof Error ? error.message : String(error))
        return
      }
      // the renderer gave up while the adapter was being built
      if (stage !== 'waiting') return
      stage = 'attached'
      const joined = join.attach(adapter)
      session = joined
      releaseInput()
      let displayedVersion: number | null = null
      let queued = false
      // Selection events can fire inside integrate(), before the engine confirms the edit.
      const publish = (): void => {
        if (queued) return
        queued = true
        queueMicrotask(() => {
          queued = false
          const status = joined.status()
          if (presence && status.phase === 'live' && !status.unconfirmed)
            joined.setPresence(presence.read())
        })
      }
      const presence = options.presence?.(handles, publish)
      const report = (status: CollabStatus): void => {
        reportCollabStatus(status)
        // unsaved means the log holds edits no stored file has
        reportDirty(status.version > status.saved)
        options.status?.(status)
        if (presence) {
          const version = status.phase === 'live' && !status.unconfirmed ? status.version : null
          if (version !== null && version !== displayedVersion) publish()
          displayedVersion = version
          presence.draw(presenceMarks(status))
        }
      }
      joined.subscribe(report)
      // opened first: a host that sets its title on opened must not overwrite the dirty state
      // a late joiner reports straight away
      releaseOpened(true)
      report(joined.status())
    })()
    return attaching
  }
  return {
    baseBytes: join.baseBytes,
    attach,
    save,
    self: () => session?.status().self ?? null,
  }
}

const INPUT_EVENTS = [
  'beforeinput',
  'keydown',
  'keypress',
  'paste',
  'cut',
  'drop',
  'compositionstart',
  'pointerdown',
  'mousedown',
  'click',
]

/**
 * Refuse every input event the page gets until the returned function runs, while this page's
 * session is the window's. Events whose target `allowed` accepts go through.
 */
function holdInput(allowed?: (target: EventTarget | null) => boolean): () => void {
  const owner = window.seuCollab
  const refuse = (event: Event): void => {
    if (window.seuCollab !== owner || allowed?.(event.target)) return
    event.preventDefault()
    event.stopImmediatePropagation()
  }
  for (const type of INPUT_EVENTS) window.addEventListener(type, refuse, true)
  return () => {
    for (const type of INPUT_EVENTS) window.removeEventListener(type, refuse, true)
  }
}
