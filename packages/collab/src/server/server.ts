/**
 * The authority for the order of every edit (design section 3). Per document
 * it handles one event at a time on a promise chain, so a decision sees the
 * effects of every earlier frame, close and store call of that document. The
 * decisions themselves are the pure functions in decide.ts; this file is the
 * shell around sockets, timers and the store.
 *
 * An epoch ends in exactly three ways: it stayed without sockets for
 * `idleEpochMs` with every version saved, the host called `reset`, or two
 * fingerprints differed. A socket leaving never ends it by itself, so a
 * client that lost its connection can always resume.
 */

import { systemClock, type Clock } from '../clock.js'
import { deferred } from '../deferred.js'
import { MAX_CHANGE_BYTES, MAX_PRESENCE_BYTES, PRESENCE_INTERVAL_MS } from '../limits.js'
import type { Change, Format, Peer } from '../model.js'
import { sha256 } from '../sha256.js'
import {
  encodePresence,
  parseClientFrame,
  withinBytes,
  type BytesFrame,
  type ClientFrame,
  type ErrorCode,
  type HelloFrame,
  type PresenceFrame,
  type ServerFrame,
  type SubmitFrame,
} from '../wire.js'
import { decideHello, decideSubmit, type HelloDecision } from './decide.js'
import type { AppendRequest, AppendResult, CollabStore, EpochHead, NewEpoch } from './store.js'
import type { HistoryReceipt } from './history.js'

/** Who is on the other end of a socket; the host authenticated the upgrade and decides it */
export interface Principal {
  documentId: string
  userId: string
  name: string
  canEdit: boolean
  /**
   * The sha-256 hex of the file the host holds as current. When given, a socket may seed a new
   * epoch only from exactly that file; others get `error forbidden`.
   */
  fileHash?: string | undefined
}

/** One accepted WebSocket, adapted by the host from its WebSocket library */
export interface ServerSocket {
  send(frame: string | Uint8Array): void
  close(code?: number, reason?: string): void
  onMessage(handler: (frame: string | Uint8Array) => void): void
  onClose(handler: () => void): void
}

export interface Limits {
  /** the largest text frame, which bounds one submit with its ops */
  maxChangeBytes: number
  /** the largest base file a seeding socket may send */
  maxBaseBytes: number
  /** changes read from the store at a time while a socket catches up */
  catchUpPage: number
  /** how long a document stays without sockets before its saved epoch ends; Infinity keeps it */
  idleEpochMs: number
  /** how long a socket asked for the base may take to send it before the next editor is asked */
  seedTimeoutMs: number
  /** frames of one socket waiting to be handled; past it the socket is closed */
  maxQueuedFrames: number
  /**
   * Size of the frames of one socket waiting to be handled; past it the socket is closed. The
   * socket's first binary frame (its base upload) is not counted; `maxBaseBytes` bounds it.
   * Keep it at least `maxChangeBytes`, or one large change can never be received.
   */
  maxQueuedBytes: number
}

const DEFAULT_LIMITS: Limits = {
  maxChangeBytes: MAX_CHANGE_BYTES,
  maxBaseBytes: 50 * 1024 * 1024,
  catchUpPage: 200,
  idleEpochMs: 10 * 60 * 1000,
  seedTimeoutMs: 30_000,
  maxQueuedFrames: 256,
  maxQueuedBytes: 32 * 1024 * 1024,
}

/** setTimeout fires at once for longer delays */
const MAX_TIMER_MS = 2 ** 31 - 1

export interface CollabServerOptions {
  store: CollabStore
  limits?: Partial<Limits> | undefined
}

export interface CollabServer {
  /** take a socket the host authenticated for `principal.documentId` */
  accept(socket: ServerSocket, principal: Principal): void
  /**
   * End the document's epoch now: every socket of it gets `error reset` and
   * closes. Call it after replacing the stored file outside co-editing; the
   * next opener starts a new epoch from that file.
   */
  reset(documentId: string): Promise<void>
  /** close every socket and wait until every document has settled; epochs stay in the store */
  close(): Promise<void>
}

/** A new epoch's base, the next change on its head, or the head of an epoch an editor joins. */
export type HwpHistoryCandidate = Pick<NewEpoch, 'id' | 'format' | 'baseHash'> &
  ({ base: Uint8Array } | { next: Change } | { head: number })
/**
 * Why HWP history refused a base, an edit or a join: `detail` is a fixed phrase the editor shows,
 * `cause` the host's own error text, which only the host log receives. `unavailable`: the
 * renderer could not run, so the editor reconnects and tries again.
 */
export interface HwpHistoryRefusal {
  refused: 'schema_mismatch' | 'too_large' | 'unavailable'
  detail: string
  cause?: string
}

/** What the history renderer accepts in one HWP request (history-renderer request.ts). */
export const HWP_REPLAY = {
  fileBytes: 2 * 1024 * 1024,
  changes: 128,
  ops: 256,
  changeBytes: 256 * 1024,
  nodes: 16_384,
  depth: 32,
  /** UTF-8 bytes of every string and object key */
  textBytes: 256 * 1024,
  /** The whole request as JSON; the stream renderer counts its header and change frames */
  requestBytes: 1024 * 1024,
}

const hwpRefusalReasons: Record<'schema_mismatch' | 'too_large', string> = {
  too_large: 'HWP retained history exceeds its replay limits',
  schema_mismatch: 'HWP source or edit cannot be replayed within the retained history profile',
}

/** Renderer failures in a row for one document before its editor is refused instead of retried */
const HWP_RENDER_ATTEMPTS = 3

export function isHwpHistory(format: Format): boolean {
  return format.name === 'hwp' || format.name === 'hwpx'
}

export interface ServerCoreOptions extends CollabServerOptions {
  clock?: Clock | undefined
  admitHwpHistory?: (
    principal: Principal,
    candidate: HwpHistoryCandidate,
  ) => Promise<HwpHistoryRefusal | undefined>
}

/** The server plus what the conformance kit needs to step it deterministically */
export interface ServerCore extends CollabServer {
  /** resolves once no document has queued work */
  idle(): Promise<void>
  /** Publish an authorized staged export after earlier socket and history operations. */
  inDocumentQueue<T>(documentId: string, run: () => Promise<T>): Promise<T>
  /** Commit in the same document queue as idle/drop and socket submits, then deliver the result. */
  commitHistory(
    documentId: string,
    epoch: string,
    commit: () => Promise<HistoryReceipt>,
  ): Promise<HistoryReceipt>
}

/** `new` until its hello; `closed` sockets are forgotten and their frames ignored */
type Stage = 'new' | 'seeding' | 'waiting' | 'live' | 'closed'

interface Conn {
  readonly socket: ServerSocket
  readonly principal: Principal
  stage: Stage
  hello: HelloFrame | null
  client: string | null
  epoch: string | null
  /** the highest version this socket received; a change goes out only as `sent + 1` (invariant 4) */
  sent: number
  presence: (PresenceFrame & { client: string }) | null
  presenceScheduled: boolean
  presenceTimer: unknown
  nextPresenceAt: number
  /** frames received and not yet handled */
  inbox: number
  /** size of those frames, in string length or bytes */
  inboxBytes: number
  /** the socket sent its one binary frame, the base upload */
  sentBinary: boolean
  /** the socket is being closed for a frame it should not have sent; its later frames are ignored */
  refused: boolean
  /** while seeding: the timer that hands seeding on */
  seedTimer: unknown
}

/** What this process remembers about an epoch beyond what the store holds */
interface EpochState {
  id: string
  saved: number
  /** per version, the fingerprint each editing client last reported */
  fingerprints: Map<number, Map<string, string>>
  /** client id to the user it was issued to and the highest connection attempt welcomed */
  clients: Map<string, { user: string; attempt: number }>
  /** HWP history replayed this epoch's head in this process, so its editors may join */
  replayable: boolean
}

interface Doc {
  readonly id: string
  readonly conns: Set<Conn>
  /** the socket asked for the base; the others wait in `waiting` */
  seeder: Conn | null
  waiting: Conn[]
  tail: Promise<void>
  queued: number
  /** the timer that ends a saved epoch nobody came back to */
  idle: unknown
  epoch: EpochState | null
}

type Welcome = Extract<HelloDecision, { kind: 'welcome' }>

const FINGERPRINTS_KEPT = 64

export function createCollabServer(options: CollabServerOptions): CollabServer {
  const { accept, reset, close } = createServerCore(options)
  return { accept, reset, close }
}

export function createServerCore({
  store,
  limits: overrides,
  clock = systemClock,
  admitHwpHistory,
}: ServerCoreOptions): ServerCore {
  const limits: Limits = { ...DEFAULT_LIMITS, ...overrides }
  const docs = new Map<string, Doc>()
  /** per document, the HWP renders in a row that could not run */
  const renderFailures = new Map<string, number>()
  let closing = false
  let busy = 0
  let idleWaiters: (() => void)[] = []

  function docFor(id: string): Doc {
    let doc = docs.get(id)
    if (!doc) {
      doc = {
        id,
        conns: new Set(),
        seeder: null,
        waiting: [],
        tail: Promise.resolve(),
        queued: 0,
        idle: null,
        epoch: null,
      }
      docs.set(id, doc)
    }
    return doc
  }

  /** run `task` after everything queued for `doc`; resolves when it is done */
  function enqueue(doc: Doc, conn: Conn | null, task: () => Promise<void>): Promise<void> {
    doc.queued += 1
    busy += 1
    doc.tail = doc.tail
      .then(task)
      .catch(async () => {
        // The store failed: drop the socket so its client reconnects and resumes
        if (conn) await abort(doc, conn)
      })
      .catch(() => {})
      .then(() => {
        doc.queued -= 1
        busy -= 1
        // A document whose epoch this process knows stays, so client ids stay bound to users
        const forgettable = doc.conns.size === 0 && doc.idle === null && doc.epoch === null
        if (doc.queued === 0 && forgettable && docs.get(doc.id) === doc) docs.delete(doc.id)
        if (busy === 0) {
          const waiters = idleWaiters
          idleWaiters = []
          for (const resolve of waiters) resolve()
        }
      })
    return doc.tail
  }

  function send(conn: Conn, frame: ServerFrame | string | Uint8Array): void {
    if (conn.stage === 'closed') return
    try {
      conn.socket.send(
        typeof frame === 'string' || frame instanceof Uint8Array ? frame : JSON.stringify(frame),
      )
    } catch {
      // A socket that cannot take a frame is closing; its onClose removes it
    }
  }

  function live(doc: Doc): Conn[] {
    return [...doc.conns].filter((conn) => conn.stage === 'live')
  }

  function peersOf(doc: Doc): Peer[] {
    return live(doc).map((conn) => ({ client: conn.client!, name: conn.principal.name }))
  }

  function broadcast(doc: Doc, frame: ServerFrame, except?: Conn): void {
    const text = JSON.stringify(frame)
    for (const conn of live(doc)) {
      if (conn !== except && (frame.t !== 'presence' || conn.hello?.presence)) send(conn, text)
    }
  }

  function receivePresence(doc: Doc, conn: Conn, frame: PresenceFrame): void {
    if (conn.stage !== 'live' || !conn.hello?.presence || frame.v !== conn.sent) return
    if (encodePresence(frame) === undefined) return
    conn.presence = { ...frame, client: conn.client! }
    if (conn.presenceScheduled) return
    conn.presenceScheduled = true
    const flush = () => {
      void enqueue(doc, conn, async () => {
        conn.presenceScheduled = false
        const latest = conn.presence
        if (conn.stage !== 'live' || conn.refused || !latest || latest.v !== conn.sent) return
        conn.nextPresenceAt = clock.now() + PRESENCE_INTERVAL_MS
        broadcast(doc, latest, conn)
      })
    }
    const delay = conn.nextPresenceAt - clock.now()
    if (delay > 0) conn.presenceTimer = clock.setTimeout(flush, delay)
    else flush()
  }

  function changeFrame({ v, client, seq, author, ops }: Change): string {
    return JSON.stringify({ t: 'change', v, client, seq, author, ops })
  }

  function epochState(doc: Doc, epoch: EpochHead): EpochState {
    if (doc.epoch?.id !== epoch.id) {
      doc.epoch = {
        id: epoch.id,
        saved: 0,
        fingerprints: new Map(),
        clients: new Map(),
        replayable: false,
      }
    }
    doc.epoch.saved = Math.max(doc.epoch.saved, epoch.saved)
    return doc.epoch
  }

  async function dropEpoch(doc: Doc, epochId: string): Promise<void> {
    await store.drop(doc.id, epochId)
    if (doc.epoch?.id === epochId) doc.epoch = null
  }

  function cancelIdle(doc: Doc): void {
    if (doc.idle === null) return
    clock.clearTimeout(doc.idle)
    doc.idle = null
  }

  /** started when the last socket leaves; any accepted socket cancels it */
  function armIdle(doc: Doc): void {
    if (closing || doc.idle !== null || !Number.isFinite(limits.idleEpochMs)) return
    doc.idle = clock.setTimeout(
      () => {
        doc.idle = null
        void enqueue(doc, null, async () => {
          if (doc.conns.size > 0) return
          const epoch = await store.load(doc.id)
          if (epoch && epoch.saved === epoch.head) await dropEpoch(doc, epoch.id)
        })
      },
      Math.min(limits.idleEpochMs, MAX_TIMER_MS),
    )
  }

  /** close `conns` after one last frame, without the leave bookkeeping of `remove` */
  function evict(doc: Doc, conns: Conn[], frame: ServerFrame, reason: string): void {
    const text = JSON.stringify(frame)
    for (const conn of conns) {
      send(conn, text)
      conn.socket.close(4000, reason)
      clock.clearTimeout(conn.presenceTimer)
      conn.stage = 'closed'
      doc.conns.delete(conn)
    }
  }

  async function reject(
    doc: Doc,
    conn: Conn,
    code: ErrorCode,
    reason: string = code,
    /** a sentence for the person whose editor stops, sent in the error frame */
    detail?: string,
  ): Promise<void> {
    send(conn, { t: 'error', code, ...(detail ? { reason: detail } : {}) })
    conn.socket.close(4000, reason)
    await remove(doc, conn)
  }

  async function abort(doc: Doc, conn: Conn): Promise<void> {
    if (conn.stage === 'closed') return
    conn.socket.close(1011, 'internal error')
    await remove(doc, conn)
  }

  /**
   * Work for one socket inside a loop over several: when the store fails, only that socket is
   * closed (its client reconnects and resumes) and the loop goes on with the others.
   */
  async function forSocket(doc: Doc, conn: Conn, work: () => Promise<void>): Promise<void> {
    try {
      await work()
    } catch {
      await abort(doc, conn)
    }
  }

  async function remove(doc: Doc, conn: Conn): Promise<void> {
    if (conn.stage === 'closed') return
    const wasLive = conn.stage === 'live'
    conn.stage = 'closed'
    clock.clearTimeout(conn.presenceTimer)
    doc.conns.delete(conn)
    doc.waiting = doc.waiting.filter((other) => other !== conn)
    if (wasLive) {
      if (conn.hello?.presence)
        broadcast(doc, { t: 'presence', client: conn.client!, v: conn.sent, at: null })
      broadcast(doc, { t: 'peers', peers: peersOf(doc) })
    }
    if (doc.seeder === conn) {
      endSeeding(doc)
      await answerWaiting(doc)
    }
    if (doc.conns.size === 0) armIdle(doc)
  }

  function endSeeding(doc: Doc): void {
    clock.clearTimeout(doc.seeder?.seedTimer)
    doc.seeder = null
  }

  /** answer every waiting hello again, in order: the epoch now exists, or the next editor seeds */
  async function answerWaiting(doc: Doc): Promise<void> {
    const waiting = doc.waiting
    doc.waiting = []
    for (const other of waiting) {
      if (other.stage === 'waiting') await forSocket(doc, other, () => answerHello(doc, other))
    }
  }

  async function answerHello(doc: Doc, conn: Conn): Promise<void> {
    const hello = conn.hello!
    if (hello.resume?.client.startsWith('history:')) return reject(doc, conn, 'forbidden')
    const epoch = await store.load(doc.id)
    const known = epoch ? epochState(doc, epoch) : null
    const issued = hello.resume ? known?.clients.get(hello.resume.client) : undefined
    const decision = decideHello(epoch, hello, {
      seeding: doc.seeder !== null,
      owner: issued?.user,
      user: conn.principal.userId,
      canEdit: conn.principal.canEdit,
      currentHash: conn.principal.fileHash,
      welcomed: issued?.attempt,
    })
    switch (decision.kind) {
      case 'need_base':
        conn.stage = 'seeding'
        doc.seeder = conn
        conn.seedTimer = clock.setTimeout(
          () =>
            void enqueue(doc, null, async () => {
              if (doc.seeder !== conn) return
              conn.socket.close(4000, 'no base')
              await remove(doc, conn)
            }),
          limits.seedTimeoutMs,
        )
        send(conn, { t: 'need_base' })
        return
      case 'wait':
        conn.stage = 'waiting'
        doc.waiting.push(conn)
        return
      case 'reject':
        return reject(doc, conn, decision.code)
      case 'welcome':
        return welcome(doc, conn, decision)
    }
  }

  async function welcome(
    doc: Doc,
    conn: Conn,
    { epoch, client: resumed, attempt, start, sendBase }: Welcome,
  ): Promise<void> {
    const client = resumed ?? crypto.randomUUID()
    const state = epochState(doc, epoch)
    const admit = conn.principal.canEdit && !state.replayable ? hwpAdmission(doc, conn) : undefined
    if (admit) {
      // The host, a server without history, or this server before a restart started this epoch:
      // an editor learns at join that it cannot be replayed, never on its first edit.
      const refusal = await admit(conn.principal, epoch)
      if (refusal)
        return rejectHistory(doc, conn, {
          ...refusal,
          detail: `the existing session cannot be replayed: ${refusal.detail}`,
        })
      state.replayable = true
    }
    // The fence in decideHello let only a newer attempt through, so any other socket of this
    // client belongs to a connection the client gave up
    for (const other of live(doc)) {
      if (other.client === client) await reject(doc, other, 'superseded')
    }
    state.clients.set(client, { user: conn.principal.userId, attempt })
    if (sendBase) {
      const base = await store.readBase(doc.id, epoch.id)
      send(conn, { t: 'base', bytes: base.byteLength })
      send(conn, base)
    }
    conn.stage = 'live'
    conn.client = client
    conn.epoch = epoch.id
    conn.sent = start
    send(conn, {
      t: 'welcome',
      epoch: epoch.id,
      client,
      name: conn.principal.name,
      start,
      head: epoch.head,
      saved: state.saved,
      canEdit: conn.principal.canEdit,
      peers: peersOf(doc),
      ...(conn.hello?.presence ? { presence: true } : {}),
    })
    await catchUp(doc, conn, epoch.head)
    if (conn.hello?.presence) {
      for (const other of live(doc)) {
        const latest = other.presence
        if (other !== conn && latest && latest.at !== null && latest.v === conn.sent)
          send(conn, latest)
      }
    }
    broadcast(doc, { t: 'peers', peers: peersOf(doc) }, conn)
  }

  /** send `conn` every change after the ones it has, up to `to`, in pages */
  async function catchUp(doc: Doc, conn: Conn, to: number): Promise<void> {
    while (conn.stage === 'live' && conn.sent < to) {
      const page = await store.read(
        doc.id,
        conn.epoch!,
        conn.sent,
        Math.min(limits.catchUpPage, to - conn.sent),
      )
      if (page.length === 0) throw new Error('the log is shorter than its head')
      for (const change of page) {
        if (change.v !== conn.sent + 1) throw new Error('the log has a gap')
        send(conn, changeFrame(change))
        conn.sent = change.v
      }
    }
  }

  function hwpAdmission(doc: Doc, conn: Conn) {
    const admit = admitHwpHistory
    if (!admit || !conn.hello || !isHwpHistory(conn.hello.format)) return undefined
    return async (...args: Parameters<typeof admit>) => {
      const refusal = await admit(...args)
      if (!refusal) renderFailures.delete(doc.id)
      return refusal
    }
  }

  async function rejectHistory(doc: Doc, conn: Conn, refusal: HwpHistoryRefusal) {
    const { detail, cause } = refusal
    const log = (reason: string) =>
      console.warn(`collab: document ${doc.id}: ${reason}${cause ? ` (${cause})` : ''}`)
    let refused = refusal.refused
    let shown = detail
    if (refused === 'unavailable') {
      const failures = (renderFailures.get(doc.id) ?? 0) + 1
      if (failures < HWP_RENDER_ATTEMPTS) {
        renderFailures.set(doc.id, failures)
        log(`HWP history renderer could not run (${failures} in a row); the editor reconnects`)
        // Closed without an error frame: the client reconnects as after a dropped connection.
        return abort(doc, conn)
      }
      renderFailures.delete(doc.id)
      refused = 'schema_mismatch'
      shown = `${detail} ${failures} times in a row`
    }
    const reason = `${hwpRefusalReasons[refused]}: ${shown}`
    log(`${refused}: ${reason}`)
    // The close reason must fit a WebSocket close frame; the error frame carries the detail.
    return reject(doc, conn, refused, hwpRefusalReasons[refused], reason)
  }

  async function onBase(doc: Doc, conn: Conn, bytes: Uint8Array): Promise<void> {
    if (conn.stage !== 'seeding') return reject(doc, conn, 'bad_frame')
    const hello = conn.hello!
    // Hash and render the same snapshot; the socket library may reuse the buffer.
    const admit = hwpAdmission(doc, conn)
    const base =
      admit && bytes.byteLength > HWP_REPLAY.fileBytes ? undefined : new Uint8Array(bytes)
    if (!admit || !(await store.load(doc.id))) {
      if (!base)
        return rejectHistory(doc, conn, {
          refused: 'too_large',
          detail: `the file exceeds ${HWP_REPLAY.fileBytes} bytes`,
        })
      if ((await sha256(base)) !== hello.fileHash) return reject(doc, conn, 'bad_frame')
      const epoch = {
        id: crypto.randomUUID(),
        format: hello.format,
        baseHash: hello.fileHash,
        base,
      }
      const refused = admit && (await admit(conn.principal, epoch))
      if (refused) {
        if (!(await store.load(doc.id))) return rejectHistory(doc, conn, refused)
      } else if ((await store.create(doc.id, epoch)) === 'created' && admit) {
        // The base was just replayed: its editors join without replaying it again.
        epochState(doc, { ...epoch, head: 0, saved: 0 }).replayable = true
      }
    }
    endSeeding(doc)
    await forSocket(doc, conn, () => answerHello(doc, conn))
    await answerWaiting(doc)
  }

  async function append(
    doc: Doc,
    conn: Conn,
    change: AppendRequest,
  ): Promise<AppendResult | HwpHistoryRefusal> {
    const id = conn.epoch!
    const admit = hwpAdmission(doc, conn)
    if (admit) {
      const epoch = await store.load(doc.id)
      if (epoch?.id !== id) return { kind: 'gone' }
      const { base, client, seq, author, ops } = change
      if (epoch.head > 0) {
        // A strictly older base can never append. The store still returns the original
        // duplicate receipt before comparing heads, even for a retry at today's head.
        const prior = await store.append(doc.id, id, { ...change, base: epoch.head - 1 })
        if (prior.kind !== 'stale' || prior.head !== base) return prior
      } else if (base !== 0) return { kind: 'stale', head: 0 }
      const refused = await admit(conn.principal, {
        ...epoch,
        next: { v: base + 1, client, seq, author, ops },
      })
      if (refused) {
        const current = await store.load(doc.id)
        if (current?.id !== id) return { kind: 'gone' }
        if (current.head === base) return refused
        // A competing writer won during render. Let the store answer stale or duplicate.
      }
    }
    return store.append(doc.id, id, change)
  }

  async function onSubmit(doc: Doc, conn: Conn, submit: SubmitFrame): Promise<void> {
    const sender = {
      client: conn.stage === 'live' ? conn.client : null,
      canEdit: conn.principal.canEdit,
      author: conn.principal.userId,
    }
    const decision = decideSubmit(sender, submit)
    if (decision.kind === 'reject') return reject(doc, conn, decision.code)
    let result: AppendResult | HwpHistoryRefusal
    try {
      result = await append(doc, conn, decision.change)
    } catch {
      // Appended or not, the client finds out when it resumes: a duplicate submit is acked
      return abort(doc, conn)
    }
    if ('refused' in result) return rejectHistory(doc, conn, result)
    switch (result.kind) {
      case 'appended': {
        const { client, seq, author, ops } = decision.change
        const change: Change = { v: result.v, client, seq, author, ops }
        await forSocket(doc, conn, async () => {
          if (conn.sent < change.v - 1) await catchUp(doc, conn, change.v - 1)
          send(conn, { t: 'ack', v: change.v, seq })
          conn.sent = change.v
        })
        const frame = changeFrame(change)
        for (const other of live(doc)) {
          if (other === conn) continue
          if (other.sent === change.v - 1) {
            send(other, frame)
            other.sent = change.v
          } else await forSocket(doc, other, () => catchUp(doc, other, change.v))
        }
        return
      }
      case 'duplicate':
        send(conn, { t: 'ack', v: result.v, seq: submit.seq })
        return
      case 'stale':
        await catchUp(doc, conn, result.head)
        send(conn, { t: 'stale', seq: submit.seq, base: submit.base, head: result.head })
        return
      case 'gone':
        return reject(doc, conn, 'reset')
    }
  }

  async function onFingerprint(doc: Doc, conn: Conn, v: number, fp: string): Promise<void> {
    if (conn.stage !== 'live' || v > conn.sent || !doc.epoch) return reject(doc, conn, 'bad_frame')
    // A view-only editor cannot be fixed by its user, and must not end everyone's epoch
    if (!conn.principal.canEdit) return
    const fingerprints = doc.epoch.fingerprints
    let atV = fingerprints.get(v)
    if (!atV) {
      atV = new Map()
      fingerprints.set(v, atV)
      if (fingerprints.size > FINGERPRINTS_KEPT)
        fingerprints.delete(fingerprints.keys().next().value!)
    }
    // A client's later report at one version replaces its earlier one
    atV.set(conn.client!, fp)
    if ([...atV.values()].every((other) => other === fp)) return
    // No file of this epoch can be trusted any more: its sockets go now, and with the last
    // of them the epoch, saved or not; the next opener starts from the host's stored file
    evict(doc, live(doc), { t: 'diverged', v }, 'diverged')
    await dropEpoch(doc, doc.epoch.id)
  }

  async function onSaved(doc: Doc, conn: Conn, v: number): Promise<void> {
    if (conn.stage !== 'live' || v > conn.sent) return reject(doc, conn, 'bad_frame')
    // Only an editor's host may declare the log stored: it lets the epoch end
    if (!conn.principal.canEdit) return reject(doc, conn, 'forbidden')
    const saved = await store.markSaved(doc.id, conn.epoch!, v)
    if (doc.epoch && saved > doc.epoch.saved) {
      doc.epoch.saved = saved
      broadcast(doc, { t: 'saved', v: saved })
    }
  }

  function tooLarge(raw: string | Uint8Array): boolean {
    if (raw instanceof Uint8Array) return raw.byteLength > limits.maxBaseBytes
    return !withinBytes(raw, limits.maxChangeBytes)
  }

  async function onFrame(doc: Doc, conn: Conn, raw: string | Uint8Array): Promise<void> {
    if (conn.stage === 'closed' || conn.refused) return
    let frame: ClientFrame | BytesFrame
    try {
      frame = parseClientFrame(raw)
    } catch {
      return reject(doc, conn, 'bad_frame')
    }
    switch (frame.t) {
      case 'presence':
        // In-budget presence bypasses the inbox, so this frame exceeded its wire limit.
        return reject(doc, conn, 'bad_frame')
      case 'hello':
        if (conn.stage !== 'new') return reject(doc, conn, 'bad_frame')
        conn.hello = frame
        return answerHello(doc, conn)
      case 'bytes':
        return onBase(doc, conn, frame.bytes)
      case 'submit':
        return onSubmit(doc, conn, frame)
      case 'fingerprint':
        return onFingerprint(doc, conn, frame.v, frame.fp)
      case 'saved':
        return onSaved(doc, conn, frame.v)
    }
  }

  function inDocumentQueue<T>(documentId: string, run: () => Promise<T>): Promise<T> {
    if (closing) return Promise.reject(new Error('server closed'))
    const outcome = deferred<T>()
    void enqueue(docFor(documentId), null, async () => {
      try {
        outcome.resolve(await run())
      } catch (error) {
        outcome.reject(error)
      }
    })
    return outcome.promise
  }

  return {
    accept(socket, principal) {
      if (closing) {
        socket.close(1001, 'server closing')
        return
      }
      const doc = docFor(principal.documentId)
      cancelIdle(doc)
      const conn: Conn = {
        socket,
        principal,
        stage: 'new',
        hello: null,
        client: null,
        epoch: null,
        sent: 0,
        presence: null,
        presenceScheduled: false,
        presenceTimer: null,
        nextPresenceAt: -Infinity,
        inbox: 0,
        inboxBytes: 0,
        sentBinary: false,
        refused: false,
        seedTimer: null,
      }
      doc.conns.add(conn)
      // Checked before queueing, so a socket cannot make the server hold what it will refuse
      socket.onMessage((frame) => {
        if (conn.stage === 'closed' || conn.refused) return
        if (tooLarge(frame)) {
          conn.refused = true
          void enqueue(doc, conn, () => reject(doc, conn, 'too_large'))
        } else {
          // Only a bounded selection may bypass the inbox. Keep queued edits in their wire form.
          if (
            typeof frame === 'string' &&
            withinBytes(frame, MAX_PRESENCE_BYTES) &&
            (frame.includes('presence') || frame.includes('\\u'))
          ) {
            try {
              const candidate = parseClientFrame(frame)
              if (candidate.t === 'presence') return receivePresence(doc, conn, candidate)
            } catch {
              // The queue handles invalid frames in arrival order.
            }
          }
          // A string's length bounds its memory to within a factor of two; no need to encode it
          const counted =
            typeof frame === 'string' ? frame.length : conn.sentBinary ? frame.byteLength : 0
          if (typeof frame !== 'string') conn.sentBinary = true
          if (
            conn.inbox >= limits.maxQueuedFrames ||
            conn.inboxBytes + counted > limits.maxQueuedBytes
          ) {
            conn.refused = true
            conn.socket.close(1008, 'too many frames')
            void enqueue(doc, conn, () => remove(doc, conn))
            return
          }
          conn.inbox += 1
          conn.inboxBytes += counted
          void enqueue(doc, conn, () => {
            conn.inbox -= 1
            conn.inboxBytes -= counted
            return onFrame(doc, conn, frame)
          })
        }
      })
      socket.onClose(() => void enqueue(doc, conn, () => remove(doc, conn)))
    },
    async reset(documentId) {
      const doc = docFor(documentId)
      cancelIdle(doc)
      const outcome: { failed: boolean; error?: unknown } = { failed: false }
      await enqueue(doc, null, async () => {
        try {
          evict(doc, [...doc.conns], { t: 'error', code: 'reset' }, 'reset')
          endSeeding(doc)
          doc.waiting = []
          const epoch = await store.load(doc.id)
          if (epoch) await dropEpoch(doc, epoch.id)
          doc.epoch = null
        } catch (error) {
          outcome.failed = true
          outcome.error = error
        }
      })
      if (outcome.failed) throw outcome.error
    },
    inDocumentQueue,
    commitHistory(documentId, epoch, commit) {
      return inDocumentQueue(documentId, async () => {
        const receipt = await commit()
        if (receipt.kind === 'undone') {
          const doc = docFor(documentId)
          for (const conn of live(doc)) {
            if (conn.epoch === epoch && conn.sent < receipt.version)
              await forSocket(doc, conn, () => catchUp(doc, conn, receipt.version))
          }
        }
        return receipt
      })
    },
    async close() {
      closing = true
      const tails = [...docs.values()].map((doc) => {
        cancelIdle(doc)
        return enqueue(doc, null, async () => {
          for (const conn of [...doc.conns]) {
            conn.socket.close(1001, 'server closing')
            await remove(doc, conn)
          }
        })
      })
      await Promise.all(tails)
    },
    idle() {
      if (busy === 0) return Promise.resolve()
      const { promise, resolve } = deferred<void>()
      idleWaiters.push(resolve)
      return promise
    },
  }
}
