/**
 * The iframe side of the @seuoffice/embed protocol. The page announces
 * `ready` to its parent, takes the first `open` from an allowed origin and
 * answers `opened` or `error` to that origin. In edit mode the page sends
 * `save` with the document's bytes and waits for `saved` or `save_failed`;
 * it reports `dirty` changes, and answers the host's `request_save` with
 * `request_save_done` after running its own save.
 *
 * When `open` says the host offers AI, `requestAiFromHost` sends one
 * gateway-complete request body as `ai_request`; the host answers with raw
 * SSE bytes in `ai_chunk`, then `ai_end`, or `ai_error`, and the page sends
 * `ai_cancel` when it stops reading.
 *
 * `ready` says whether this page can co-edit (`registerCollabCapable`). When
 * `open` says the host relays co-editing, `collab_*` messages carry the
 * frames of the host's socket (see collab-link.ts).
 *
 * A host's `thumbnail` is answered with `thumbnail_done`: the first page as
 * a PNG, or the code of why there is none (see thumbnail.ts).
 */

import type { HistoryRequest } from '@seuoffice/collab/client'
import { makeThumbnail, ThumbnailRefusal, type ThumbnailSource } from './thumbnail'

export const EMBED_SOURCE = 'seuoffice'
export const EMBED_VERSION = 1

export type EmbedOpenErrorCode =
  'unsupported_format' | 'too_large' | 'open_failed' | 'collab_unavailable' | 'unsupported'

export interface EmbedOpen {
  fileName: string
  bytes: ArrayBuffer
  /** the host relays AI requests to its gateway (`ai_request`) */
  ai: boolean
  /** the host runs the app's autosave (`autoSave: true`); without it the page shows no autosave toggle */
  autoSave: boolean
  /** the host relays co-editing frames (`collab_connect`) */
  collab: boolean
}

/**
 * Why a save happened, carried by the save itself so overlapping saves never
 * share a flag: the app's autosave, the person's own Save, or the host's `doc.save()`.
 */
export type SaveReason = 'auto' | 'user' | 'host'

interface Peer {
  target: Window
  origin: string
}

let started: Promise<EmbedOpen | null> | undefined
let peer: Peer | undefined
let opened = false
/**
 * `held`: a co-edited page holds `opened` until its editor attached to the
 * session; `dropped`: the page left the session and reported why instead.
 */
let openedHold: 'none' | 'held' | 'dropped' = 'none'
let openedAsked = false
let lastDirty: boolean | undefined
let nextSaveId = 1
const pendingSaves = new Map<number, { resolve: () => void; reject: (error: Error) => void }>()
/** What a host's `request_save` runs: it gets this save's owner id (see `saveReason`) */
let saveRequestHandler: ((ownerId: string) => Promise<void>) | undefined
/** Owner ids of the host-asked saves still running: the saves they start are `host` saves */
const hostSaveOwners = new Set<string>()
let exportRequestHandler: (() => Promise<{ fileName: string; bytes: Uint8Array }>) | undefined
/** What draws the first page for a host's `thumbnail`; an app that draws none sets nothing */
let thumbnailSource: ThumbnailSource | undefined
let collabCapable = false
let historyCapable = false
let historyOpen: { requestId: string; request: HistoryRequest } | undefined
let collabMessageHandler: ((data: Record<string, unknown>) => void) | undefined
let collabOffered = false

/** One piece of a host's answer, read by the response body in arrival order */
type AiAnswerPart =
  { kind: 'bytes'; bytes: Uint8Array } | { kind: 'end' } | { kind: 'error'; error: Error }

/**
 * One AI request to the host. `settle` answers the fetch-shaped promise with
 * the first message and is gone after that; `parts` feed the response body.
 * The request stays in `aiRequests` until its body is read to the end, so an
 * abort still drops bytes not read yet; `wireDone` marks that the host sent
 * its last message, after which nothing more from the host counts.
 */
interface AiRequest {
  settle: { resolve: (response: Response) => void; reject: (error: Error) => void } | undefined
  parts: AiAnswerPart[]
  wake: (() => void) | undefined
  body: ReadableStream<Uint8Array>
  wireDone: boolean
}
const aiRequests = new Map<string, AiRequest>()

/** The response headers the protocol code reads; the host sends no others */
const AI_HEADER_NAMES = ['content-type', 'retry-after']

function aiHeaders(value: unknown): Headers {
  const headers = new Headers()
  if (!value || typeof value !== 'object') return headers
  for (const name of AI_HEADER_NAMES) {
    const header: unknown = Reflect.get(value, name)
    if (typeof header === 'string') headers.set(name, header)
  }
  return headers
}

function pushAiPart(request: AiRequest, part: AiAnswerPart): void {
  request.parts.push(part)
  request.wake?.()
}

/** Where the serving side lists the extra origins, next to the apps' folders (dist/embed-origins.json) */
export const EMBED_ORIGINS_FILE = 'embed-origins.json'

/** A config fetch that takes longer fails closed: the page then accepts only its own origin */
const CONFIG_TIMEOUT_MS = 5000

/**
 * The exact origin a config entry names, or null. Only `scheme://host[:port]`
 * of http(s) counts: no wildcard, path, query, fragment, credentials or
 * `null`. The default port and the letter case normalize away.
 */
export function normalizeOrigin(value: unknown): string | null {
  if (typeof value !== 'string' || /[*\s]/.test(value)) return null
  let url: URL
  try {
    url = new URL(value)
  } catch {
    return null
  }
  if (url.protocol !== 'http:' && url.protocol !== 'https:') return null
  if (url.hostname.includes('*') || url.href !== `${url.origin}/`) return null
  return url.origin
}

/** The origins of a parsed `embed-origins.json` (`{ "origins": [...] }`), or null when any part is not one */
export function parseOriginsConfig(json: unknown): string[] | null {
  if (typeof json !== 'object' || json === null || !('origins' in json)) return null
  const { origins: entries } = json
  if (!Array.isArray(entries)) return null
  const origins: string[] = []
  for (const entry of entries) {
    const origin = normalizeOrigin(entry)
    if (origin === null) return null
    origins.push(origin)
  }
  return origins
}

/**
 * Origins allowed to send `open`: this page's own, plus the ones the server
 * lists in the same-origin file `../embed-origins.json`. Nothing the
 * embedding page controls is read. A missing file, a failed fetch, a status
 * other than 200, or a file that is not valid leaves only this page's origin.
 * Never rejects.
 */
export async function loadAllowedOrigins(): Promise<Set<string>> {
  const allowed = new Set([location.origin])
  let response: Response
  try {
    response = await fetch(new URL(`../${EMBED_ORIGINS_FILE}`, location.href), {
      cache: 'no-store',
      credentials: 'omit',
      redirect: 'error',
      signal: AbortSignal.timeout(CONFIG_TIMEOUT_MS),
    })
    if (response.status !== 200) return allowed
  } catch {
    return allowed
  }
  let origins: string[] | null = null
  try {
    origins = parseOriginsConfig(await response.json())
  } catch {
    // not JSON, or cut off
  }
  if (origins === null) {
    console.warn(
      `[seuoffice] ${EMBED_ORIGINS_FILE} is not {"origins": [exact origins]}; ignoring it`,
    )
    return allowed
  }
  for (const origin of origins) allowed.add(origin)
  return allowed
}

/** First accepted `open`, or null when the page is not framed */
export function receiveOpen(): Promise<EmbedOpen | null> {
  started ??= listen()
  return started
}

/** Whether the host that opened this page runs the app's autosave; false for a page nobody framed */
export async function hostEnablesAutoSave(): Promise<boolean> {
  return (await receiveOpen())?.autoSave === true
}

/**
 * Tell the sender of a refused `open` that it was refused, so its `opened`
 * settles instead of waiting. Only the code goes back, to the origin that
 * asked: no document data and no word about which origins are allowed.
 */
function refuseOpen(sender: MessageEventSource | null, origin: string): void {
  // an opaque origin ("null") cannot be a target origin: there is nobody to answer
  if (!sender || origin === 'null') return
  // an `open` comes from a window; a port or a worker has no origin to answer
  const host = sender as Window
  try {
    host.postMessage(
      { source: EMBED_SOURCE, version: EMBED_VERSION, type: 'error', code: 'origin_not_allowed' },
      { targetOrigin: origin },
    )
  } catch {
    // a window that went away cannot hear it
  }
}

function listen(): Promise<EmbedOpen | null> {
  if (window.parent === window) return Promise.resolve(null)
  // the suite apps compile this file against ES2022, which has no Promise.withResolvers
  let resolve!: (open: EmbedOpen | null) => void
  const promise = new Promise<EmbedOpen | null>((settle) => (resolve = settle))
  // an `open` waits here until the file is read: it is neither dropped nor trusted early
  const allowed = loadAllowedOrigins()
  let accepted = false
  const onOpen = (event: MessageEvent): void => {
    const data = event.data as Record<string, unknown> | null
    if (!data || data.source !== EMBED_SOURCE || data.version !== EMBED_VERSION) return
    if (data.type !== 'open') return
    if ('history' in data && event.source !== window.parent) return
    const { source: sender, origin } = event
    void allowed.then((origins) => {
      if (accepted) return
      if (!origins.has(origin)) {
        refuseOpen(sender, origin)
        return
      }
      if (typeof data.fileName !== 'string' || !(data.bytes instanceof ArrayBuffer)) return
      accepted = true
      window.removeEventListener('message', onOpen)
      peer = { target: sender as Window, origin }
      if ('history' in data) {
        if (
          !historyCapable ||
          new URLSearchParams(location.search).get('mode') !== 'view' ||
          data.collab !== false ||
          data.ai !== false ||
          data.autoSave !== false
        ) {
          reply({ type: 'error', code: 'unsupported', message: 'this page cannot replay history' })
          resolve(null)
          return
        }
        if (
          !data.history ||
          typeof data.history !== 'object' ||
          typeof data.requestId !== 'string' ||
          !data.requestId
        ) {
          reply({ type: 'error', code: 'invalid_history', message: 'invalid history request' })
          resolve(null)
          return
        }
        // replayHistory validates the entire retained prefix before applying it.
        historyOpen = { requestId: data.requestId, request: data.history as HistoryRequest }
        resolve({
          fileName: data.fileName,
          bytes: data.bytes,
          ai: false,
          autoSave: false,
          collab: false,
        })
        return
      }
      window.addEventListener('message', onHostMessage)
      // a page that goes away stops reading: the host should stop paying for the answers
      window.addEventListener('pagehide', cancelAllAiRequests)
      collabOffered = data.collab === true
      resolve({
        fileName: data.fileName,
        bytes: data.bytes,
        ai: data.ai === true,
        autoSave: data.autoSave === true,
        collab: collabOffered,
      })
    })
  }
  window.addEventListener('message', onOpen)
  // Only capabilities go to an unknown parent; the base and history use the accepted origin.
  window.parent.postMessage(
    {
      source: EMBED_SOURCE,
      version: EMBED_VERSION,
      type: 'ready',
      collab: collabCapable,
      history: historyCapable,
    },
    '*',
  )
  return promise
}

/** Messages after `open`: only from the window and origin that sent it */
function onHostMessage(event: MessageEvent): void {
  if (!peer || event.source !== peer.target || event.origin !== peer.origin) return
  const data = event.data as Record<string, unknown> | null
  if (!data || data.source !== EMBED_SOURCE || data.version !== EMBED_VERSION) return
  if (data.type === 'saved' || data.type === 'save_failed') {
    const id = data.saveId
    const pending = typeof id === 'number' ? pendingSaves.get(id) : undefined
    if (!pending) return
    pendingSaves.delete(id as number)
    if (data.type === 'saved') pending.resolve()
    else pending.reject(new Error(String(data.message ?? 'the host did not save the document')))
  } else if (data.type === 'request_save' && typeof data.requestId === 'number') {
    void answerSaveRequest(data.requestId)
  } else if (
    (data.type === 'ai_chunk' || data.type === 'ai_end' || data.type === 'ai_error') &&
    typeof data.requestId === 'string'
  ) {
    onAiAnswer(data.requestId, data)
  } else if (data.type === 'collab_export' && typeof data.requestId === 'string') {
    void answerExportRequest(data.requestId)
  } else if (data.type === 'thumbnail' && typeof data.requestId === 'string') {
    void answerThumbnailRequest(data.requestId, data.width)
  } else if (typeof data.type === 'string' && data.type.startsWith('collab_')) {
    collabMessageHandler?.(data)
  }
}

async function answerSaveRequest(requestId: number): Promise<void> {
  const ownerId = crypto.randomUUID()
  hostSaveOwners.add(ownerId)
  try {
    if (!saveRequestHandler) throw new Error('this document cannot be saved here')
    await saveRequestHandler(ownerId)
    reply({ type: 'request_save_done', requestId, ok: true })
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error)
    reply({ type: 'request_save_done', requestId, ok: false, message })
  } finally {
    hostSaveOwners.delete(ownerId)
  }
}

/** The file of what the page shows, for a host rescuing edits a session could not keep */
async function answerExportRequest(requestId: string): Promise<void> {
  try {
    if (!exportRequestHandler) throw new Error('this page does not co-edit')
    const { fileName, bytes } = await exportRequestHandler()
    const copy = new Uint8Array(bytes.byteLength)
    copy.set(bytes)
    reply({ type: 'collab_export_done', requestId, ok: true, fileName, bytes: copy.buffer }, [
      copy.buffer,
    ])
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error)
    reply({ type: 'collab_export_done', requestId, ok: false, message })
  }
}

/** The first page as a PNG, or why the page has none to give */
async function answerThumbnailRequest(requestId: string, requestedWidth: unknown): Promise<void> {
  try {
    // before `opened` nothing is on screen: there is no first page yet
    const source = opened ? thumbnailSource : undefined
    const { bytes, width, height } = await makeThumbnail(source, requestedWidth)
    reply({ type: 'thumbnail_done', requestId, ok: true, bytes, width, height }, [bytes])
  } catch (error) {
    // a drawing the app could not finish has no code of its own: this page cannot draw it
    const code = error instanceof ThumbnailRefusal ? error.code : 'thumbnail_unsupported'
    const message = error instanceof Error ? error.message : String(error)
    reply({ type: 'thumbnail_done', requestId, ok: false, code, message })
  }
}

/** The host's answer to one AI request, in arrival order */
function onAiAnswer(requestId: string, data: Record<string, unknown>): void {
  const request = aiRequests.get(requestId)
  if (!request || request.wireDone) return
  const settle = request.settle
  request.settle = undefined
  if (data.type === 'ai_error') {
    request.wireDone = true
    const message = String(data.message ?? 'the host could not reach the AI gateway')
    if (!settle) {
      // after the bytes already sent: the protocol reads those first, then fails
      pushAiPart(request, { kind: 'error', error: new Error(message) })
    } else {
      // the body is never read: the request ends here
      aiRequests.delete(requestId)
      if (typeof data.status === 'number' && data.status >= 400 && data.status <= 599) {
        // the gateway's refusal reaches the protocol as that HTTP answer, as on the desktop
        settle.resolve(
          new Response(message, { status: data.status, headers: aiHeaders(data.headers) }),
        )
      } else {
        // the host's request failed before an answer: what a failed fetch looks like
        settle.reject(new TypeError(message))
      }
    }
    return
  }
  if (data.type !== 'ai_chunk' && data.type !== 'ai_end') return
  settle?.resolve(new Response(request.body, { status: 200, headers: aiHeaders(data.headers) }))
  if (data.type === 'ai_chunk' && ArrayBuffer.isView(data.data)) {
    // isView, not instanceof: the bytes may come from another realm's Uint8Array
    const { buffer, byteOffset, byteLength } = data.data
    pushAiPart(request, { kind: 'bytes', bytes: new Uint8Array(buffer, byteOffset, byteLength) })
  } else if (data.type === 'ai_end') {
    request.wireDone = true
    pushAiPart(request, { kind: 'end' })
  }
}

function abortError(): Error {
  return new DOMException('The AI request was cancelled', 'AbortError')
}

/** Stop one request here and tell the host: nothing it sends for it counts any more */
function cancelAiRequest(requestId: string): void {
  const request = aiRequests.get(requestId)
  if (!request) return
  aiRequests.delete(requestId)
  // a host that already finished has nothing left to stop
  if (!request.wireDone) reply({ type: 'ai_cancel', requestId })
  request.settle?.reject(abortError())
  request.settle = undefined
  // bytes not read yet are dropped: the reader sees the cancel next
  request.parts.length = 0
  pushAiPart(request, { kind: 'error', error: abortError() })
}

function cancelAllAiRequests(): void {
  for (const requestId of [...aiRequests.keys()]) cancelAiRequest(requestId)
}

/**
 * Send one gateway-complete request body to the host and read its answer as
 * a fetch Response: the SSE bytes stream into the body as they arrive, with
 * the content type and Retry-After the gateway sent. A non-2xx gateway answer
 * resolves with that status and the gateway's text; a host that could not
 * call the gateway rejects like a failed fetch. Aborting `signal`, cancelling
 * the body or leaving the page tells the host to stop (`ai_cancel`), and
 * drops bytes not read yet even when the host already finished sending.
 */
export function requestAiFromHost(body: unknown, signal?: AbortSignal | null): Promise<Response> {
  if (!peer) return Promise.reject(new TypeError('no host to ask: the page is not embedded'))
  if (signal?.aborted) return Promise.reject(abortError())
  const requestId = crypto.randomUUID()
  const request: AiRequest = {
    settle: undefined,
    parts: [],
    wake: undefined,
    wireDone: false,
    // pulled one part per read, so an error after some bytes lets the reader take those first
    body: new ReadableStream<Uint8Array>(
      {
        pull: async (controller) => {
          while (request.parts.length === 0) {
            await new Promise<void>((resolve) => (request.wake = resolve))
          }
          request.wake = undefined
          const part = request.parts.shift()!
          if (part.kind === 'bytes') {
            controller.enqueue(part.bytes)
            return
          }
          // the last part is read: nothing can cancel this request any more
          aiRequests.delete(requestId)
          if (part.kind === 'end') controller.close()
          else controller.error(part.error)
        },
        cancel: () => cancelAiRequest(requestId),
      },
      { highWaterMark: 0 },
    ),
  }
  const settled = new Promise<Response>((resolve, reject) => {
    request.settle = { resolve, reject }
  })
  aiRequests.set(requestId, request)
  signal?.addEventListener('abort', () => cancelAiRequest(requestId), { once: true })
  reply({ type: 'ai_request', requestId, body })
  return settled
}

export function reply(message: Record<string, unknown>, transfer: Transferable[] = []): void {
  if (!peer) return
  peer.target.postMessage(
    { source: EMBED_SOURCE, version: EMBED_VERSION, ...message },
    { targetOrigin: peer.origin, transfer },
  )
}

/** Tell the host the document is on screen; later calls do nothing. Waits while `holdOpened` holds it. */
export function reportOpened(): void {
  if (opened) return
  if (openedHold !== 'none') {
    openedAsked = true
    return
  }
  opened = true
  reply({ type: 'opened' })
}

/**
 * Keep `opened` back until the returned function runs: `true` sends it (if
 * the page reported it meanwhile), `false` never sends it, for a page that
 * reported an error instead. A co-edited page holds it until its editor
 * attached: before that, an error still reaches the host, which stops
 * listening for one once the document is open.
 */
export function holdOpened(): (send: boolean) => void {
  openedHold = 'held'
  return (send) => {
    if (openedHold !== 'held') return
    openedHold = send ? 'none' : 'dropped'
    if (send && openedAsked) reportOpened()
  }
}

/** Tell the host the document could not be shown */
export function reportError(code: EmbedOpenErrorCode, message: string): void {
  reply({ type: 'error', code, message })
}

/**
 * The reason of one save. `ownerId` is the id the app's save flow was started
 * with by a host request (`onHostSaveRequest` hands it to the handler, the
 * app carries it through its own save calls); `auto` is the app's own mark of
 * an autosave pass. A save no host request started is `auto` or `user`.
 */
export function saveReason(ownerId: string | undefined, auto: boolean): SaveReason {
  if (ownerId !== undefined && hostSaveOwners.has(ownerId)) return 'host'
  return auto ? 'auto' : 'user'
}

/**
 * Hand the document's bytes to the host, which stores them. Settles when the
 * host answers: resolves on `saved`, rejects with the host's message. A
 * co-edited save names the exact version the bytes hold.
 */
export function saveToHost(
  fileName: string,
  bytes: Uint8Array,
  reason: SaveReason,
  collab?: { version: number },
): Promise<void> {
  if (historyMode()) return Promise.reject(new Error('a history render cannot save'))
  if (!peer) return Promise.reject(new Error('no host to save to: the page is not embedded'))
  const saveId = nextSaveId++
  // an exact fresh copy the page can transfer without losing its own
  const copy = new Uint8Array(bytes.byteLength)
  copy.set(bytes)
  const settled = new Promise<void>((resolve, reject) =>
    pendingSaves.set(saveId, { resolve, reject }),
  )
  const message: Record<string, unknown> = {
    type: 'save',
    saveId,
    reason,
    fileName,
    bytes: copy.buffer,
  }
  if (collab) message.collabVersion = collab.version
  reply(message, [copy.buffer])
  return settled
}

/** Tell the host whether the document has unsaved changes; repeats are dropped */
export function reportDirty(dirty: boolean): void {
  if (dirty === lastDirty) return
  lastDirty = dirty
  reply({ type: 'dirty', dirty })
}

/** The app's own save, run when the host asks for one (`request_save`); `ownerId` marks the saves it starts */
export function onHostSaveRequest(handler: (ownerId: string) => Promise<void>): void {
  saveRequestHandler = handler
}

/** What the page answers a host's `collab_export`: the file of exactly what it shows */
export function onHostExportRequest(
  handler: () => Promise<{ fileName: string; bytes: Uint8Array }>,
): void {
  exportRequestHandler = handler
}

/**
 * What draws the first page when the host asks for its picture (`thumbnail`).
 * The returned function takes it back: the app's document went away.
 */
export function onHostThumbnailRequest(source: ThumbnailSource): () => void {
  thumbnailSource = source
  return () => {
    if (thumbnailSource === source) thumbnailSource = undefined
  }
}

/**
 * Declare that this app co-edits. Call it before anything asks for the
 * document: `ready` carries the answer, and the host decides from it.
 */
export function registerCollabCapable(options?: { history?: boolean }): void {
  if (started) throw new Error('registerCollabCapable must run before the page announces ready')
  collabCapable = true
  historyCapable = options?.history === true
}

/** True only for a dedicated view opened by the authenticated host with a retained prefix. */
export function historyMode(): boolean {
  return historyOpen !== undefined
}

export function hostHistory(): typeof historyOpen {
  return historyOpen
}

/** The host's `collab_*` messages, already checked to come from the host */
export function onCollabMessage(handler: (data: Record<string, unknown>) => void): void {
  collabMessageHandler = handler
}

/** Whether the host's `open` asked for co-editing; false before `open` */
export function hostRelaysCollab(): boolean {
  return collabOffered
}
