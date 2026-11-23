/**
 * Open a document in a seuoffice iframe from its bytes and file name.
 *
 * The iframe page announces `ready`; this side answers with `open` carrying
 * the bytes, and the page reports `opened` or `error`. Messages are accepted
 * only from the iframe's own window at the seuoffice origin.
 *
 * In edit mode the host has taken the document's edit lock before calling
 * `openDocument`; the page sends `save` with new bytes and its `reason`, this side runs
 * `onSave` and answers `saved` or `save_failed`. `dirty` reports unsaved
 * changes, and `save()` asks the page to save (`request_save`).
 *
 * With `ai`, the `open` message says the host offers AI. The page sends
 * `ai_request` with a gateway-complete request body (decision 0009); this
 * side hands it to `ai.request`, which adds the host's own token and calls
 * the gateway, and streams the answer back as `ai_chunk` bytes, then
 * `ai_end`, or `ai_error` (with the HTTP status for a non-2xx answer).
 * `ai_cancel` aborts that request; `destroy()` aborts every request.
 *
 * With `collab`, the `open` message says the host relays co-editing. The
 * page's `collab_connect` makes this side call `collab.connect()` and pipe
 * that socket: `collab_open`, `collab_frame` both ways (unchanged), and
 * `collab_close`; `collab_disconnect` and `destroy()` close it. A collab
 * `save` carries `collabVersion`, the exact version its bytes hold. A page
 * whose `ready` says it cannot co-edit gets no `open`: `opened` rejects with
 * `collab_unavailable`.
 *
 * `thumbnail()` asks the page for its first page as a PNG (`thumbnail`); the
 * page answers `thumbnail_done` with the bytes or the code of why it has none.
 */

import {
  createCollabPipe,
  type CollabRelay,
  type HistoryJson,
  type HistoryRequest,
  type HistoryResult,
} from './collab.js'

export type {
  CollabRelay,
  CollabStatus,
  HistoryRequest,
  HistoryResult,
  WebSocketLike,
} from './collab.js'

export type SeuofficeApp = 'docs' | 'sheets' | 'slides' | 'pdf' | 'hwp'

const APP_BY_EXTENSION: ReadonlyMap<string, SeuofficeApp> = new Map([
  ['pdf', 'pdf'],
  ['docx', 'docs'],
  ['xlsx', 'sheets'],
  ['pptx', 'slides'],
  ['hwp', 'hwp'],
  ['hwpx', 'hwp'],
])

/** The suite app that opens this file, by extension; null when none does */
export function appForFileName(fileName: string): SeuofficeApp | null {
  const dot = fileName.lastIndexOf('.')
  if (dot < 0) return null
  return APP_BY_EXTENSION.get(fileName.slice(dot + 1).toLowerCase()) ?? null
}

export const MAX_EMBED_BYTES = 50 * 1024 * 1024

/**
 * `collab_unavailable`: the page cannot co-edit this file (too large to load
 * whole, an edit kind it cannot sync). The host opens it again without
 * `collab`, under its edit lock.
 *
 * The `thumbnail_*` codes are the reasons `thumbnail()` rejects with (see `ThumbnailErrorCode`).
 */
export type EmbedErrorCode =
  | 'unsupported_format'
  | 'too_large'
  | 'open_failed'
  | 'timeout'
  | 'save_failed'
  | 'collab_unavailable'
  | 'open_timeout'
  | 'invalid_sandbox'
  | 'origin_not_allowed'
  | 'invalid_history'
  | 'unsupported'
  | 'conflict'
  | ThumbnailErrorCode

/**
 * Why `thumbnail()` gave no image. `thumbnail_unsupported`: this format draws
 * none yet, no document is open, or the page could not draw it;
 * `thumbnail_timeout`: no image in time; `thumbnail_too_large`: the PNG is over
 * the page's size limit; `thumbnail_tainted`: the page holds a picture of
 * another origin and may not read its own pixels.
 */
export type ThumbnailErrorCode =
  'thumbnail_unsupported' | 'thumbnail_timeout' | 'thumbnail_too_large' | 'thumbnail_tainted'

export class SeuofficeEmbedError extends Error {
  readonly code: EmbedErrorCode

  constructor(code: EmbedErrorCode, message: string) {
    super(message)
    this.name = 'SeuofficeEmbedError'
    this.code = code
  }
}

interface BaseOpenOptions {
  /** where the seuoffice web build is served, e.g. https://office.example/ */
  baseUrl: string
  bytes: ArrayBuffer | Uint8Array
  fileName: string
  lang?: string
  /** how long to wait for the page to announce itself; default 30000 */
  readyTimeoutMs?: number
  /**
   * the iframe's `sandbox` attribute; default `DEFAULT_SANDBOX`. It must keep
   * `allow-scripts` and `allow-same-origin`, else `opened` rejects with `invalid_sandbox`
   */
  sandbox?: string
  /** the iframe's `allow` (permissions policy) attribute; none by default */
  allow?: string
  /**
   * how long the page has to open the document once the host sent it (from
   * `ready` to `opened`); on expiry `opened` rejects with `open_timeout`. Default 60000
   */
  openTimeoutMs?: number
  /** offer AI to the page; without it the page shows AI as not connected */
  ai?: AiRelay
}

/**
 * The host's call to its AI gateway. `body` is the page's gateway-complete
 * request body (OpenAI chat completions); the host adds its own token, posts
 * it, and returns the gateway's response. Abort the request when `signal`
 * aborts. The page never sees the token.
 */
export interface AiRelay {
  request(body: unknown, signal: AbortSignal): Promise<Response>
}

/**
 * Why the page saved: `auto` its autosave, `user` the person's own Save (button or
 * shortcut), `host` a `doc.save()` the host asked for
 */
export type SaveReason = 'auto' | 'user' | 'host'

/** A document the page saved: same format and file name it was opened with */
export interface SavedFile {
  fileName: string
  bytes: Uint8Array
  reason: SaveReason
}

/** A co-edited document the page saved: `bytes` hold exactly version `version` */
export interface CollabSavedFile extends SavedFile {
  version: number
}

export interface ViewOptions extends BaseOpenOptions {
  mode?: 'view'
  /** relay co-editing frames between the page and the host's socket */
  collab?: CollabRelay
}

interface EditOptionsBase extends BaseOpenOptions {
  /** open for editing; without `collab` the host holds the edit lock for this document */
  mode: 'edit'
  /** called when the page gains or loses unsaved changes */
  onDirtyChange?: (dirty: boolean) => void
  /**
   * run the app's autosave (30 s and when the window loses focus, while it has unsaved
   * changes), each one reaching `onSave` with `reason: 'auto'`. Default false: the page then
   * shows no autosave toggle and never saves by itself. When true, autosave starts on and the
   * person can switch it off; the switch lasts until the page closes and is never stored.
   */
  autoSave?: boolean
}

export interface LockedEditOptions extends EditOptionsBase {
  collab?: undefined
  /** store the saved bytes; resolve when stored, reject with the reason it was not */
  onSave: (file: SavedFile) => Promise<void>
}

export interface CollabEditOptions extends EditOptionsBase {
  /** relay co-editing frames between the page and the host's socket */
  collab: CollabRelay
  /** store the saved bytes of `version`; resolve when stored, reject with the reason it was not */
  onSave: (file: CollabSavedFile) => Promise<void>
}

export type EditOptions = LockedEditOptions | CollabEditOptions

export type OpenOptions = ViewOptions | EditOptions

/** Options that ask for co-editing, in either mode */
export type CollabOpenOptions = (ViewOptions & { collab: CollabRelay }) | CollabEditOptions

/** A file of exactly what the page shows; it holds no version */
export interface LocalFile {
  fileName: string
  bytes: Uint8Array
}

export interface ThumbnailOptions {
  /** the image's width in pixels; default 480, and a width outside 64 to 1024 is moved to the nearer end */
  width?: number
}

/** The document's first page as a PNG of `width` by `height` pixels */
export interface Thumbnail {
  bytes: Uint8Array
  width: number
  height: number
}

export interface EmbeddedDocument {
  readonly iframe: HTMLIFrameElement
  /** settles once the document is on screen, or rejects with SeuofficeEmbedError */
  readonly opened: Promise<void>
  /**
   * Whether the page can co-edit this document, from its `ready`. False when
   * the page cannot (or never answered); with `collab` given, `opened` then
   * rejects with `collab_unavailable`: destroy this document and open it again
   * without `collab`, under the edit lock.
   */
  readonly collabCapable: Promise<boolean>
  /**
   * Ask the page to save now; settles after `onSave` stored the bytes.
   * Rejects with `save_failed` in view mode, before the document opened, or
   * when the page or `onSave` failed.
   */
  save(): Promise<void>
  /**
   * The first page as a PNG, in view and in edit mode. Rejects with one of the
   * `thumbnail_*` codes; before the document opened that is `thumbnail_unsupported`.
   */
  thumbnail(options?: ThumbnailOptions): Promise<Thumbnail>
  destroy(): void
}

export interface CollabEmbeddedDocument extends EmbeddedDocument {
  /**
   * The file of what this person's page shows, for when the session ended
   * (`diverged`, `failed`) or they are offline. It has no version: offer it
   * as a download or a copy, never store it over the document. Never calls
   * `onSave`. Rejects with `save_failed` when the page refused or did not
   * answer in time, or the document is not open.
   */
  exportLocal(): Promise<LocalFile>
}

const SOURCE = 'seuoffice'
const VERSION = 1

/**
 * The iframe's `sandbox` unless `OpenOptions.sandbox` says otherwise. The page
 * needs scripts and its own origin (without `allow-same-origin` it cannot
 * announce itself); `allow-downloads` lets the page hand a file to the user.
 * Popups, modals, forms, top navigation and the like stay closed.
 */
export const DEFAULT_SANDBOX = 'allow-scripts allow-same-origin allow-downloads'

/** Without these two in `sandbox` the page cannot open: `openDocument` refuses at once */
const REQUIRED_SANDBOX = ['allow-scripts', 'allow-same-origin']

/** How long the page has, after the host sent the bytes, to answer `opened` or `error` */
export const DEFAULT_OPEN_TIMEOUT_MS = 60000

export interface RenderHistoryOptions {
  baseUrl: string
  fileName: string
  bytes: ArrayBuffer | Uint8Array
  history: HistoryRequest
  lang?: string
  /** Total time for the disposable editor to load, replay and produce the file. */
  timeoutMs?: number
}

let nextHistoryId = 0

/** Replay a retained prefix in a new, read-only editor. Never opens a socket or calls onSave. */
export async function renderHistory(
  container: HTMLElement,
  options: RenderHistoryOptions,
): Promise<HistoryResult> {
  const app = appForFileName(options.fileName)
  if (!app) throw new SeuofficeEmbedError('unsupported', `no editor opens ${options.fileName}`)
  if (options.bytes.byteLength > MAX_EMBED_BYTES)
    throw new SeuofficeEmbedError('too_large', 'the base file exceeds the embed size limit')
  if (!options.history || typeof options.history !== 'object')
    throw new SeuofficeEmbedError('invalid_history', 'no history request')
  const url = new URL(`${app}/index.html`, options.baseUrl)
  url.search = new URLSearchParams({ mode: 'view', lang: options.lang ?? 'ko' }).toString()
  const frame = container.ownerDocument.createElement('iframe')
  frame.setAttribute('sandbox', REQUIRED_SANDBOX.join(' '))
  frame.setAttribute('aria-hidden', 'true')
  frame.inert = true
  frame.tabIndex = -1
  // Keep a layout viewport for editors that initialize from its dimensions.
  frame.style.cssText =
    'position:fixed;inset:0;width:100%;height:100%;opacity:0;pointer-events:none;border:0'
  frame.src = url.href
  const view = container.ownerDocument.defaultView ?? window
  const requestId = String(++nextHistoryId)
  let sent = false
  let undo = false
  let receive!: (event: MessageEvent) => void
  let timer: number | undefined
  try {
    return await new Promise<HistoryResult>((resolve, reject) => {
      receive = (event) => {
        if (event.source !== frame.contentWindow || event.origin !== url.origin) return
        const data = event.data as Record<string, unknown> | null
        if (!data || data.source !== SOURCE || data.version !== VERSION) return
        if (data.type === 'ready' && !sent) {
          sent = true
          if (data.history !== true) {
            reject(new SeuofficeEmbedError('unsupported', 'this editor does not support history'))
            return
          }
          try {
            const bytes = copyBytes(options.bytes)
            undo = options.history.undo !== undefined
            frame.contentWindow!.postMessage(
              {
                source: SOURCE,
                version: VERSION,
                type: 'open',
                requestId,
                fileName: options.fileName,
                bytes,
                history: options.history,
                collab: false,
                autoSave: false,
                ai: false,
              },
              url.origin,
              [bytes],
            )
          } catch (error) {
            reject(new SeuofficeEmbedError('invalid_history', String(error)))
          }
        } else if (
          sent &&
          (data.type === 'error' || (data.type === 'history_done' && data.requestId === requestId))
        ) {
          if (data.type === 'error' || data.ok !== true) {
            const code =
              data.code === 'unsupported_format' || data.code === 'collab_unavailable'
                ? 'unsupported'
                : data.code === 'invalid_history' ||
                    data.code === 'conflict' ||
                    data.code === 'unsupported' ||
                    isPageErrorCode(data.code)
                  ? data.code
                  : 'open_failed'
            reject(new SeuofficeEmbedError(code, String(data.message ?? code)))
            return
          }
          const { bytes, fileName, compensation } = data
          if (
            !(bytes instanceof ArrayBuffer) ||
            !bytes.byteLength ||
            typeof fileName !== 'string' ||
            !fileName
          ) {
            reject(new SeuofficeEmbedError('invalid_history', 'invalid history result'))
            return
          }
          try {
            const file = { fileName, bytes: new Uint8Array(bytes) }
            if (undo) {
              if (
                !Array.isArray(compensation) ||
                !compensation.length ||
                !isHistoryJson(compensation)
              )
                throw new Error('invalid compensation')
              resolve({ ...file, compensation })
            } else if (compensation === undefined) {
              resolve(file)
            } else {
              throw new Error('unexpected compensation')
            }
          } catch {
            reject(new SeuofficeEmbedError('invalid_history', 'invalid compensation'))
          }
        }
      }
      view.addEventListener('message', receive)
      timer = view.setTimeout(
        () =>
          reject(new SeuofficeEmbedError('timeout', 'the history editor did not answer in time')),
        options.timeoutMs ?? DEFAULT_OPEN_TIMEOUT_MS,
      )
      container.appendChild(frame)
    })
  } finally {
    view.clearTimeout(timer)
    view.removeEventListener('message', receive)
    frame.remove()
  }
}

function isHistoryJson(
  value: unknown,
  memo = new Map<object, boolean>(),
  depth = 0,
): value is HistoryJson {
  if (depth > 256) return false
  if (value === null || typeof value === 'boolean' || typeof value === 'string') return true
  if (typeof value !== 'object') return typeof value === 'number' && Number.isFinite(value)
  const known = memo.get(value)
  if (known !== undefined) return known
  // Reject cycles without repeatedly walking shared subtrees in a structured clone.
  memo.set(value, false)
  if (!Array.isArray(value)) {
    const proto: unknown = Object.getPrototypeOf(value)
    if (proto !== null && proto !== Object.prototype) return false
  }
  for (const child of Array.isArray(value) ? value : Object.values(value)) {
    if (!isHistoryJson(child, memo, depth + 1)) return false
  }
  memo.set(value, true)
  return true
}

// A view can come from another window or from Buffer. Transfer only an exact, owned copy.
function copyBytes(bytes: ArrayBuffer | Uint8Array): ArrayBuffer {
  return new Uint8Array(ArrayBuffer.isView(bytes) ? bytes : new Uint8Array(bytes)).buffer
}

export function openDocument(
  container: HTMLElement,
  options: CollabOpenOptions,
): CollabEmbeddedDocument
export function openDocument(container: HTMLElement, options: OpenOptions): EmbeddedDocument
export function openDocument(container: HTMLElement, options: OpenOptions): CollabEmbeddedDocument {
  const iframe = container.ownerDocument.createElement('iframe')
  const app = appForFileName(options.fileName)
  const sandbox = options.sandbox ?? DEFAULT_SANDBOX
  const sandboxTokens = sandbox.split(/\s+/)
  const refusal = !app
    ? new SeuofficeEmbedError('unsupported_format', `no seuoffice app opens ${options.fileName}`)
    : options.bytes.byteLength > MAX_EMBED_BYTES
      ? new SeuofficeEmbedError(
          'too_large',
          `${options.fileName} is ${options.bytes.byteLength} bytes; the limit is ${MAX_EMBED_BYTES}`,
        )
      : !REQUIRED_SANDBOX.every((token) => sandboxTokens.includes(token))
        ? new SeuofficeEmbedError(
            'invalid_sandbox',
            `sandbox "${sandbox}" must allow ${REQUIRED_SANDBOX.join(' and ')}: the page cannot open without them`,
          )
        : null
  if (refusal) {
    const opened = Promise.reject(refusal)
    // a rejection the host never awaits must not surface as unhandled
    opened.catch(() => {})
    return {
      iframe,
      opened,
      collabCapable: Promise.resolve(false),
      save: () => Promise.reject(notSaved('the document did not open')),
      thumbnail: () => Promise.reject(noThumbnail('the document did not open')),
      exportLocal: () => Promise.reject(notSaved('the document did not open')),
      destroy: () => iframe.remove(),
    }
  }

  // hosts may compile against ES2022, which has no Promise.withResolvers
  let resolve!: () => void
  let reject!: (error: SeuofficeEmbedError) => void
  const opened = new Promise<void>((onResolve, onReject) => {
    resolve = onResolve
    reject = onReject
  })
  opened.catch(() => {})
  const view = container.ownerDocument.defaultView ?? window
  const base = new URL(options.baseUrl)
  const mode = options.mode ?? 'view'
  const editing = options.mode === 'edit' ? options : undefined
  const query = new URLSearchParams({ mode, lang: options.lang ?? 'ko' })
  // the sandbox is set before the frame navigates, so the page never runs outside it
  iframe.setAttribute('sandbox', sandbox)
  if (options.allow !== undefined) iframe.setAttribute('allow', options.allow)
  iframe.src = new URL(`${app}/index.html?${query}`, base).href
  iframe.style.width = '100%'
  iframe.style.height = '100%'
  iframe.style.border = '0'

  let settled = false
  let sent = false
  let shown = false
  let nextRequestId = 1
  // one store at a time, in arrival order: a slow older save must never land after a newer one
  let storing = Promise.resolve()
  const saveRequests = new Map<
    number,
    { resolve: () => void; reject: (error: SeuofficeEmbedError) => void }
  >()
  // in-flight AI requests by the page's request id; aborting one ends its host fetch
  const aiRequests = new Map<string, AbortController>()
  let nextExportId = 1
  const exportRequests = new Map<
    string,
    {
      resolve: (file: LocalFile) => void
      reject: (error: SeuofficeEmbedError) => void
      timer: ReturnType<typeof setTimeout>
    }
  >()
  let nextThumbnailId = 1
  const thumbnailRequests = new Map<
    string,
    {
      resolve: (image: Thumbnail) => void
      reject: (error: SeuofficeEmbedError) => void
      timer: ReturnType<typeof setTimeout>
    }
  >()
  let collabAnswer!: (capable: boolean) => void
  const collabCapable = new Promise<boolean>((resolve) => (collabAnswer = resolve))
  const post = (message: Record<string, unknown>, transfer: Transferable[] = []): void => {
    iframe.contentWindow?.postMessage(
      { source: SOURCE, version: VERSION, ...message },
      { targetOrigin: base.origin, transfer },
    )
  }
  let openTimer: ReturnType<typeof setTimeout> | undefined
  const settle = (error?: SeuofficeEmbedError): void => {
    if (settled) return
    settled = true
    clearTimeout(timer)
    clearTimeout(openTimer)
    if (error) reject(error)
    else resolve()
  }
  // exists only when the host relays co-editing: without it no collab message is ever sent
  const collabPipe = options.collab ? createCollabPipe(options.collab, post) : undefined

  const onMessage = (event: MessageEvent): void => {
    if (event.source !== iframe.contentWindow || event.origin !== base.origin) return
    const data = event.data as Record<string, unknown> | null
    if (!data || data.source !== SOURCE || data.version !== VERSION) return
    if (data.type === 'ready' && !sent) {
      if (options.collab && data.collab !== true) {
        // this app has no co-editing: opening it would give this person a private copy
        collabAnswer(false)
        settle(
          new SeuofficeEmbedError('collab_unavailable', 'this page cannot co-edit the document'),
        )
        return
      }
      sent = true
      clearTimeout(timer)
      // from here the page has the bytes: it must answer `opened` or `error` in time
      openTimer = setTimeout(
        () => settle(new SeuofficeEmbedError('open_timeout', 'the document did not open in time')),
        options.openTimeoutMs ?? DEFAULT_OPEN_TIMEOUT_MS,
      )
      collabAnswer(data.collab === true)
      const bytes = copyBytes(options.bytes)
      iframe.contentWindow!.postMessage(
        {
          source: SOURCE,
          version: VERSION,
          type: 'open',
          fileName: options.fileName,
          bytes,
          ai: options.ai !== undefined,
          // only an edit session saves, so only it can ask for the app's autosave
          autoSave: editing?.autoSave === true,
          collab: options.collab !== undefined,
        },
        base.origin,
        [bytes],
      )
    } else if (data.type === 'opened') {
      shown = true
      settle()
    } else if (data.type === 'error') {
      const code = isPageErrorCode(data.code) ? data.code : 'open_failed'
      settle(new SeuofficeEmbedError(code, String(data.message ?? code)))
    } else if (data.type === 'save' && editing) {
      storing = storing.then(() => store(editing, data, post))
    } else if (data.type === 'dirty' && editing && typeof data.dirty === 'boolean') {
      editing.onDirtyChange?.(data.dirty)
    } else if (data.type === 'request_save_done' && typeof data.requestId === 'number') {
      const request = saveRequests.get(data.requestId)
      if (!request) return
      saveRequests.delete(data.requestId)
      if (data.ok === true) request.resolve()
      else request.reject(notSaved(String(data.message ?? 'the page did not save')))
    } else if (data.type === 'ai_request' && sent && typeof data.requestId === 'string') {
      const requestId = data.requestId
      if (aiRequests.has(requestId)) return
      if (!options.ai) {
        post({ type: 'ai_error', requestId, message: 'this host does not offer AI' })
        return
      }
      const controller = new AbortController()
      aiRequests.set(requestId, controller)
      void relayAi(options.ai, requestId, data.body, controller.signal, post).finally(() => {
        if (aiRequests.get(requestId) === controller) aiRequests.delete(requestId)
      })
    } else if (data.type === 'ai_cancel' && typeof data.requestId === 'string') {
      aiRequests.get(data.requestId)?.abort()
      aiRequests.delete(data.requestId)
    } else if (data.type === 'collab_export_done' && typeof data.requestId === 'string') {
      const request = exportRequests.get(data.requestId)
      if (!request) return
      exportRequests.delete(data.requestId)
      clearTimeout(request.timer)
      if (
        data.ok === true &&
        typeof data.fileName === 'string' &&
        data.bytes instanceof ArrayBuffer
      )
        request.resolve({ fileName: data.fileName, bytes: new Uint8Array(data.bytes) })
      else request.reject(notSaved(String(data.message ?? 'the page did not export the document')))
    } else if (data.type === 'thumbnail_done' && typeof data.requestId === 'string') {
      const request = thumbnailRequests.get(data.requestId)
      if (!request) return
      thumbnailRequests.delete(data.requestId)
      clearTimeout(request.timer)
      if (
        data.ok === true &&
        data.bytes instanceof ArrayBuffer &&
        typeof data.width === 'number' &&
        typeof data.height === 'number'
      )
        request.resolve({
          bytes: new Uint8Array(data.bytes),
          width: data.width,
          height: data.height,
        })
      else
        request.reject(
          new SeuofficeEmbedError(
            isThumbnailErrorCode(data.code) ? data.code : 'thumbnail_unsupported',
            String(data.message ?? 'the page gave no image'),
          ),
        )
    } else if (sent && typeof data.type === 'string' && data.type.startsWith('collab_')) {
      collabPipe?.onPageMessage(data)
    }
  }

  view.addEventListener('message', onMessage)
  const timer = setTimeout(() => {
    collabAnswer(false)
    settle(new SeuofficeEmbedError('timeout', 'the seuoffice page did not answer in time'))
  }, options.readyTimeoutMs ?? 30000)
  container.appendChild(iframe)
  return {
    iframe,
    opened,
    collabCapable,
    save: () => {
      if (mode !== 'edit') return Promise.reject(notSaved('the document was opened for viewing'))
      if (!shown) return Promise.reject(notSaved('the document is not open yet'))
      const requestId = nextRequestId++
      const done = new Promise<void>((resolve, reject) =>
        saveRequests.set(requestId, { resolve, reject }),
      )
      post({ type: 'request_save', requestId })
      return done
    },
    exportLocal: () => {
      if (!options.collab)
        return Promise.reject(notSaved('the document was opened without co-editing'))
      if (!shown) return Promise.reject(notSaved('the document is not open yet'))
      const requestId = String(nextExportId++)
      const done = new Promise<LocalFile>((resolve, reject) => {
        const timer = setTimeout(() => {
          exportRequests.delete(requestId)
          reject(notSaved('the page did not export the document in time'))
        }, options.collab?.exportTimeoutMs ?? 60000)
        exportRequests.set(requestId, { resolve, reject, timer })
      })
      post({ type: 'collab_export', requestId })
      return done
    },
    thumbnail: (asked = {}) => {
      if (!shown) return Promise.reject(noThumbnail('the document is not open yet'))
      const requestId = String(nextThumbnailId++)
      const done = new Promise<Thumbnail>((resolve, reject) => {
        const timer = setTimeout(() => {
          thumbnailRequests.delete(requestId)
          reject(new SeuofficeEmbedError('thumbnail_timeout', 'the page did not answer in time'))
        }, THUMBNAIL_ANSWER_TIMEOUT_MS)
        thumbnailRequests.set(requestId, { resolve, reject, timer })
      })
      post({
        type: 'thumbnail',
        requestId,
        ...(asked.width !== undefined ? { width: asked.width } : {}),
      })
      return done
    },
    destroy: () => {
      view.removeEventListener('message', onMessage)
      clearTimeout(timer)
      for (const request of saveRequests.values())
        request.reject(notSaved('the document was closed'))
      saveRequests.clear()
      for (const request of exportRequests.values()) {
        clearTimeout(request.timer)
        request.reject(notSaved('the document was closed'))
      }
      exportRequests.clear()
      for (const request of thumbnailRequests.values()) {
        clearTimeout(request.timer)
        request.reject(noThumbnail('the document was closed'))
      }
      thumbnailRequests.clear()
      for (const controller of aiRequests.values()) controller.abort()
      aiRequests.clear()
      collabPipe?.close()
      collabAnswer(false)
      iframe.remove()
    },
  }
}

async function store(
  options: EditOptions,
  data: Record<string, unknown>,
  post: (message: Record<string, unknown>) => void,
): Promise<void> {
  const saveId = data.saveId
  const named = data.reason
  if (typeof saveId !== 'number' || !(data.bytes instanceof ArrayBuffer)) return
  try {
    // the page saves under the name it opened; a different name is not this document
    const file: SavedFile = {
      fileName: options.fileName,
      bytes: new Uint8Array(data.bytes),
      // a page that names no reason (an older build) saved for the person
      reason: named === 'auto' || named === 'host' ? named : 'user',
    }
    if (!options.collab) await options.onSave(file)
    else if (typeof data.collabVersion === 'number')
      await options.onSave({ ...file, version: data.collabVersion })
    else throw new Error('the page saved a co-edited document without its version')
    post({ type: 'saved', saveId })
  } catch (error) {
    post({
      type: 'save_failed',
      saveId,
      message: error instanceof Error ? error.message : String(error),
    })
  }
}

/** an error answer's body is cut to this many characters before it goes to the page */
const MAX_AI_ERROR_CHARS = 64 * 1024

/**
 * The response headers the page's protocol code reads. Nothing else crosses:
 * cookies and whatever else the gateway sent stay with the host.
 */
const RELAYED_AI_HEADERS = ['content-type', 'retry-after'] as const

function relayedHeaders(response: Response): Record<string, string> {
  const headers: Record<string, string> = {}
  for (const name of RELAYED_AI_HEADERS) {
    const value = response.headers.get(name)
    if (value !== null) headers[name] = value
  }
  return headers
}

/**
 * One AI request: the host's response streamed to the page as raw bytes.
 * Nothing is posted after an abort: the page cancelled, or the frame is gone.
 */
async function relayAi(
  ai: AiRelay,
  requestId: string,
  body: unknown,
  signal: AbortSignal,
  post: (message: Record<string, unknown>, transfer?: Transferable[]) => void,
): Promise<void> {
  try {
    const response = await ai.request(body, signal)
    if (signal.aborted) {
      await response.body?.cancel().catch(() => {})
      return
    }
    const headers = relayedHeaders(response)
    if (!response.ok) {
      const text = await response.text().catch(() => '')
      if (!signal.aborted) {
        post({
          type: 'ai_error',
          requestId,
          status: response.status,
          headers,
          message: text.slice(0, MAX_AI_ERROR_CHARS),
        })
      }
      return
    }
    // the first message of an answer carries its headers, whichever message it is
    let first: { headers: Record<string, string> } | Record<string, never> = { headers }
    if (response.body) {
      const reader = response.body.getReader()
      // a cancel stops the read even when the host's fetch ignored the signal
      const stop = (): void => void reader.cancel().catch(() => {})
      signal.addEventListener('abort', stop, { once: true })
      try {
        for (;;) {
          const { done, value } = await reader.read()
          if (done || signal.aborted) break
          // a fresh copy the page owns: the reader may reuse or share its buffer
          const data = new Uint8Array(value)
          post({ type: 'ai_chunk', requestId, data, ...first }, [data.buffer])
          first = {}
        }
      } finally {
        signal.removeEventListener('abort', stop)
      }
    }
    if (!signal.aborted) post({ type: 'ai_end', requestId, ...first })
  } catch (error) {
    if (signal.aborted) return
    post({
      type: 'ai_error',
      requestId,
      message: error instanceof Error ? error.message : String(error),
    })
  }
}

function notSaved(message: string): SeuofficeEmbedError {
  return new SeuofficeEmbedError('save_failed', message)
}

/**
 * How long `thumbnail()` waits for the page's answer. The page gives up
 * drawing after 10 s and says so; this is for a page that never answers (an
 * older build, a frame that went away).
 */
const THUMBNAIL_ANSWER_TIMEOUT_MS = 15000

function noThumbnail(message: string): SeuofficeEmbedError {
  return new SeuofficeEmbedError('thumbnail_unsupported', message)
}

function isThumbnailErrorCode(code: unknown): code is ThumbnailErrorCode {
  return (
    code === 'thumbnail_unsupported' ||
    code === 'thumbnail_timeout' ||
    code === 'thumbnail_too_large' ||
    code === 'thumbnail_tainted'
  )
}

function isPageErrorCode(code: unknown): code is EmbedErrorCode {
  return (
    code === 'unsupported_format' ||
    code === 'too_large' ||
    code === 'open_failed' ||
    code === 'collab_unavailable' ||
    code === 'origin_not_allowed'
  )
}
