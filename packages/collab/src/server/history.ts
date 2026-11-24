import {
  assertHistoryResult,
  checkPrefix,
  HistoryError,
  type HistoryOpId,
  type HistoryRequest,
  type HistoryResult,
} from '../history.js'
import type { Change, Json } from '../model.js'
import { sha256 } from '../sha256.js'
import {
  createServerCore,
  HWP_REPLAY,
  isHwpHistory,
  type CollabServer,
  type HwpHistoryCandidate,
  type HwpHistoryRefusal,
  type Limits,
  type Principal,
} from './server.js'
import type { CollabStore } from './store.js'
import { utf8Length } from '../wire.js'
import { MAX_CHANGE_BYTES } from '../limits.js'
import {
  admitHwpStream,
  withStreamHistory,
  type StreamHistoryArtifact,
  type StreamHistoryOptions,
  type StreamHistoryWrite,
} from './stream-history.js'

export interface HistoryPoint {
  epoch: string
  version: number
}

export interface RestoreRequest {
  requestId: string
  expected: HistoryPoint
  source: HistoryPoint
}

export interface UndoRequest {
  requestId: string
  expected: HistoryPoint
  target: HistoryOpId
}

export type HistoryAction = 'export' | 'restore' | 'undo'
/** The host decides from an authenticated principal and an author read from the retained log. */
export type UndoTargetPolicy = (
  principal: Readonly<Principal>,
  target: Readonly<HistoryOpId & { author: string }>,
) => boolean
export type HistoryCommand =
  ({ kind: 'restore' } & RestoreRequest) | ({ kind: 'undo' } & UndoRequest)
export type RestoreReceipt = { kind: 'restored'; revisionId: string }
export type UndoReceipt = { kind: 'undone'; version: number }
export type HistoryReceipt = RestoreReceipt | UndoReceipt
export type HistoryArtifact = Pick<HistoryResult, 'fileName' | 'bytes'>
export type HistoryWrite =
  | { command: { kind: 'restore' } & RestoreRequest; artifact: HistoryArtifact }
  | { command: { kind: 'undo' } & UndoRequest; compensation: Json[]; change?: Change }

/** The file at `version` of one HWP epoch, produced by the renderer and admitted like a base. */
export interface HwpCheckpoint {
  version: number
  baseHash: string
  base: Uint8Array
}

/** Where an HWP replay starts: the epoch's base (start 0) or a checkpoint, and changes already read. */
interface ReplaySource {
  base: Uint8Array
  baseHash: string
  start: number
  changes?: Change[]
}

function sameBytes(a: Uint8Array, b: Uint8Array): boolean {
  return a.length === b.length && a.every((byte, i) => byte === b[i])
}

/** Trusted persistence port. Never map these methods directly to an HTTP or socket handler. */
export interface RetainedHistoryStore extends CollabStore {
  /** Opt in only if commitHistory appends write.change verbatim when supplied, including its id and author. */
  readonly exactHistoryChange?: true
  /** Return only the named document/epoch and exactly versions 1..version, including after drop. */
  readHistory(
    principal: Principal,
    point: HistoryPoint,
    action: HistoryAction,
  ): Promise<{
    baseBytes: Uint8Array
    request: HistoryRequest
  }>
  authorizeHistory(principal: Principal, action: HistoryAction): Promise<void>
  /** Check the document grant and the exact operation in its active epoch. */
  authorizeUndoTarget(principal: Principal, target: HistoryOpId): Promise<void>
  /** Authorize the command, including its undo target, before returning a receipt. */
  historyReceipt(principal: Principal, command: HistoryCommand): Promise<HistoryReceipt | null>
  /** Atomically reauthorize the command and target, deduplicate, CAS live epoch/head, fork or undo a not-yet-undone target,
   * and record the receipt. A fork adds a revision; it never updates a current revision pointer. */
  commitHistory(principal: Principal, write: HistoryWrite): Promise<HistoryReceipt>
  readRevision(principal: Principal, revisionId: string): Promise<HistoryArtifact>
  /**
   * Optional, together with readHwpCheckpoint. Keep the file at `checkpoint.version` of the
   * named epoch for good. First write wins: a later write for the same version changes nothing.
   * Without the pair, HWP retained history refuses a change once its epoch passes the replay
   * limits, and the editor's session ends there.
   */
  writeHwpCheckpoint?(documentId: string, epochId: string, checkpoint: HwpCheckpoint): Promise<void>
  /** The checkpoint with the highest version at or below `atMost`, or null; also after drop. */
  readHwpCheckpoint?(
    documentId: string,
    epochId: string,
    atMost: number,
  ): Promise<HwpCheckpoint | null>
}

export interface HistoryServer<Artifact = HistoryArtifact> extends CollabServer {
  exportHistory(principal: Principal, source: HistoryPoint): Promise<Artifact>
  restoreHistory(principal: Principal, input: RestoreRequest): Promise<RestoreReceipt>
  undoHistory(principal: Principal, input: UndoRequest): Promise<UndoReceipt>
}
export interface StreamHistoryServer extends HistoryServer<StreamHistoryArtifact> {
  exportHistory(
    principal: Principal,
    source: HistoryPoint,
    signal?: AbortSignal,
  ): Promise<StreamHistoryArtifact>
  restoreHistory(
    principal: Principal,
    input: RestoreRequest,
    signal?: AbortSignal,
  ): Promise<RestoreReceipt>
  undoHistory(principal: Principal, input: UndoRequest, signal?: AbortSignal): Promise<UndoReceipt>
}

function point({ epoch, version }: HistoryPoint): HistoryPoint {
  if (
    typeof epoch !== 'string' ||
    !epoch ||
    epoch.length > 256 ||
    !Number.isSafeInteger(version) ||
    version < 0
  )
    throw new Error('invalid history point')
  return { epoch, version }
}

interface ByteHistoryOptions {
  store: RetainedHistoryStore
  limits?: Partial<Limits>
  /** Host-controlled isolated editor worker, never a result uploaded by the requesting user. */
  render: (baseBytes: Uint8Array, request: HistoryRequest) => Promise<HistoryResult>
}

/** Route sockets and history commands for a document to one owning server. */
export function createHistoryServer(options: StreamHistoryOptions): StreamHistoryServer
export function createHistoryServer(options: ByteHistoryOptions): HistoryServer
export function createHistoryServer(
  options: ByteHistoryOptions | StreamHistoryOptions,
): HistoryServer<HistoryArtifact | StreamHistoryArtifact> {
  const { store } = options
  const shutdown = new AbortController()
  const signalFor = (signal?: AbortSignal) =>
    signal ? AbortSignal.any([shutdown.signal, signal]) : shutdown.signal
  const server = createServerCore({ ...options, admitHwpHistory })
  const staging = new Set<Promise<unknown>>()
  function track<T>(work: Promise<T>): Promise<T> {
    staging.add(work)
    return work.finally(() => staging.delete(work))
  }

  const byteMode = 'renderStream' in options ? null : options

  /** Both checkpoint methods, or none: without them HWP history refuses at the replay limits. */
  function checkpointStore() {
    const s = byteMode?.store
    return typeof s?.writeHwpCheckpoint === 'function' && typeof s.readHwpCheckpoint === 'function'
      ? (s as Required<Pick<RetainedHistoryStore, 'writeHwpCheckpoint' | 'readHwpCheckpoint'>>)
      : null
  }

  /** The newest retained file at or below `atMost`, or null when the epoch's own base is newest. */
  async function latestCheckpoint(
    documentId: string,
    epoch: string,
    atMost: number,
  ): Promise<ReplaySource | null> {
    const checkpoints = checkpointStore()
    if (!checkpoints || atMost < 1) return null
    const found = await checkpoints.readHwpCheckpoint(documentId, epoch, atMost)
    if (!found) return null
    const { version, baseHash, base } = found
    if (
      !Number.isSafeInteger(version) ||
      version < 1 ||
      version > atMost ||
      typeof baseHash !== 'string' ||
      !(base instanceof Uint8Array)
    )
      throw new Error('invalid HWP checkpoint')
    return { base, baseHash, start: version }
  }

  /** The replay request for versions `source.start + 1` through `version`, within the limits. */
  async function replayPrefix(
    documentId: string,
    { id: epoch, format }: HwpHistoryCandidate,
    version: number,
    source: ReplaySource,
    next?: Change,
  ): Promise<{ base: Uint8Array; request: HistoryRequest } | HwpHistoryRefusal> {
    const { base, baseHash, start } = source
    if (version - start > HWP_REPLAY.changes || base.byteLength > HWP_REPLAY.fileBytes)
      return 'too_large'
    if ((await sha256(base)) !== baseHash) return 'schema_mismatch'
    const request: HistoryRequest = { epoch, version, format, baseHash, changes: [] }
    if (start) request.start = start
    let bytes = utf8Length(JSON.stringify(request))
    let ops = 0
    for (let v = start + 1; v <= version; v++) {
      // Older epochs need not meet today's byte budget. Read only one stored change at a time.
      shutdown.signal.throwIfAborted()
      const frame =
        v === version && next
          ? next
          : source.changes
            ? source.changes[v - start - 1]
            : (await store.read(documentId, epoch, v - 1, 1))[0]
      if (frame?.v !== v || !Array.isArray(frame.ops)) return 'schema_mismatch'
      if ((ops += frame.ops.length) > HWP_REPLAY.ops) return 'too_large'
      try {
        bytes += utf8Length(JSON.stringify(frame)) + (v > start + 1 ? 1 : 0)
      } catch {
        return 'schema_mismatch'
      }
      if (bytes > HWP_REPLAY.byteRequestBytes) return 'too_large'
      request.changes.push(frame)
    }
    return { base, request }
  }

  async function renderHwp(
    base: Uint8Array,
    request: HistoryRequest,
  ): Promise<HistoryResult | HwpHistoryRefusal> {
    try {
      checkPrefix(request, request.format)
      shutdown.signal.throwIfAborted()
      const result = await byteMode!.render(base, request)
      assertHistoryResult(result, false)
      if (result.bytes.byteLength > HWP_REPLAY.fileBytes) return 'too_large'
      if (!result.fileName.toLowerCase().endsWith(`.${request.format.name}`))
        return 'schema_mismatch'
      return result
    } catch {
      shutdown.signal.throwIfAborted()
      return 'schema_mismatch'
    }
  }

  /**
   * Retain the file at `head` so later changes replay from it. The renderer must reopen the file
   * it produced unchanged, the same admission a new epoch's base passes.
   */
  async function writeCheckpoint(
    documentId: string,
    candidate: HwpHistoryCandidate,
    head: number,
    from: ReplaySource,
  ): Promise<ReplaySource | HwpHistoryRefusal> {
    const checkpoints = checkpointStore()
    if (!checkpoints || head <= from.start) return 'too_large'
    const prefix = await replayPrefix(documentId, candidate, head, from)
    if (typeof prefix === 'string') return prefix
    const file = await renderHwp(prefix.base, prefix.request)
    if (typeof file === 'string') return file
    const baseHash = await sha256(file.bytes)
    const reopened = await renderHwp(file.bytes, {
      ...prefix.request,
      baseHash,
      start: head,
      changes: [],
    })
    if (typeof reopened === 'string') return reopened
    if (!sameBytes(reopened.bytes, file.bytes)) return 'schema_mismatch'
    shutdown.signal.throwIfAborted()
    // A failed write ends this editor's session with a reason instead of a reconnect loop.
    let kept: ReplaySource | null
    try {
      await checkpoints.writeHwpCheckpoint(documentId, candidate.id, {
        version: head,
        baseHash,
        base: file.bytes,
      })
      // First write wins: replay from whatever the store kept for this version.
      kept = await latestCheckpoint(documentId, candidate.id, head)
    } catch {
      shutdown.signal.throwIfAborted()
      return 'schema_mismatch'
    }
    return kept?.start === head ? kept : 'schema_mismatch'
  }

  async function admitHwpHistory(
    p: Principal,
    candidate: HwpHistoryCandidate,
    retained?: ReplaySource,
  ): Promise<HwpHistoryRefusal | undefined> {
    shutdown.signal.throwIfAborted()
    const next = 'next' in candidate ? candidate.next : undefined
    const { id: epoch, format, baseHash } = candidate
    const version = next?.v ?? 0
    if ('renderStream' in options)
      return admitHwpStream(
        options,
        p,
        candidate,
        { epoch, version, format, baseHash },
        shutdown.signal,
      )
    if (options.store.exactHistoryChange !== true) return 'exact_write_required'
    if (next && next.ops.length > HWP_REPLAY.ops) return 'too_large'

    const seed = 'base' in candidate ? candidate.base : undefined
    const from: ReplaySource = seed
      ? { base: seed, baseHash, start: 0, changes: [] }
      : (retained ??
        (await latestCheckpoint(p.documentId, epoch, version - 1)) ?? {
          base: await store.readBase(p.documentId, epoch),
          baseHash,
          start: 0,
        })
    let prefix = await replayPrefix(p.documentId, candidate, version, from, next)
    if (prefix === 'too_large' && next && !seed) {
      // The replay window is full: continue from a retained file of the current head.
      const checkpoint = await writeCheckpoint(p.documentId, candidate, version - 1, from)
      if (typeof checkpoint === 'string') return checkpoint
      prefix = await replayPrefix(p.documentId, candidate, version, checkpoint, next)
    }
    if (typeof prefix === 'string') return prefix
    const result = await renderHwp(prefix.base, prefix.request)
    if (typeof result === 'string') return result
    if (seed && !sameBytes(result.bytes, seed)) return 'schema_mismatch'
    shutdown.signal.throwIfAborted()
    return undefined
  }

  function principalFor(p: Principal, action: HistoryAction): Principal {
    if (
      shutdown.signal.aborted ||
      !p?.documentId ||
      !p.userId ||
      (action !== 'export' && p.canEdit !== true)
    )
      throw new Error('history forbidden')
    return { ...p }
  }

  async function artifact(
    bytes: ByteHistoryOptions,
    p: Principal,
    source: HistoryPoint,
    action: HistoryAction,
    undo?: UndoRequest['target'],
  ): Promise<{ file: HistoryResult; change?: Change }> {
    const { baseBytes, request } = await bytes.store.readHistory(p, source, action)
    if (
      request.epoch !== source.epoch ||
      request.version !== source.version ||
      request.undo !== undefined ||
      request.baseHash !== (await sha256(baseBytes))
    )
      throw new Error('invalid retained history')
    const checkpoint = isHwpHistory(request.format)
      ? await latestCheckpoint(p.documentId, source.epoch, source.version)
      : null
    if (checkpoint && undo && undo.v <= checkpoint.start)
      throw new HistoryError(
        'unsupported',
        'This edit is older than the saved history point and can no longer be undone on its own',
      )
    // Replay from the newest checkpoint at or below the point; the file is the same.
    const replay: ReplaySource = checkpoint
      ? { ...checkpoint, changes: request.changes.slice(checkpoint.start) }
      : { base: baseBytes, baseHash: request.baseHash, start: 0, changes: request.changes }
    const prefix: HistoryRequest = {
      ...request,
      baseHash: replay.baseHash,
      changes: replay.changes!,
      ...(replay.start ? { start: replay.start } : {}),
      ...(undo ? { undo } : {}),
    }
    checkPrefix(prefix, request.format)
    if (undo) await store.authorizeUndoTarget(p, undo)
    const hwpUndo = undo && isHwpHistory(request.format)
    if (hwpUndo && bytes.store.exactHistoryChange !== true)
      throw new HistoryError('unsupported', 'HWP undo requires exact history writes')
    if (checkpoint && (await sha256(checkpoint.base)) !== checkpoint.baseHash)
      throw new Error('invalid retained history')
    const result = await bytes.render(replay.base, prefix)
    assertHistoryResult(result, !!undo)
    if (hwpUndo) {
      const change: Change = {
        v: source.version + 1,
        client: `history:${crypto.randomUUID()}`,
        seq: 1,
        author: p.userId,
        ops: result.compensation!,
      }
      const refused = await admitHwpHistory(
        p,
        { id: source.epoch, format: request.format, baseHash: request.baseHash, next: change },
        replay,
      )
      if (refused)
        throw new HistoryError('unsupported', `HWP compensation cannot be replayed (${refused})`)
      return { file: result, change }
    }
    return { file: result }
  }

  function checkSubmit(base: number, ops: Json[]) {
    if (
      utf8Length(JSON.stringify({ t: 'submit', seq: 1, base, ops })) >
      (options.limits?.maxChangeBytes ?? MAX_CHANGE_BYTES)
    )
      throw new Error('compensation too large')
  }

  async function mutate(
    p: Principal,
    command: HistoryCommand,
    signal: AbortSignal,
  ): Promise<HistoryReceipt> {
    signal.throwIfAborted()
    if (
      typeof command.requestId !== 'string' ||
      !command.requestId ||
      command.requestId.length > 256
    )
      throw new Error('invalid history request id')
    const receipt = await store.historyReceipt(p, command)
    if (receipt) {
      return server.commitHistory(p.documentId, command.expected.epoch, async () => {
        if (command.kind === 'undo') await store.authorizeUndoTarget(p, command.target)
        else await store.authorizeHistory(p, command.kind)
        signal.throwIfAborted()
        return receipt
      })
    }
    const undo = command.kind === 'undo' ? command.target : undefined
    const source = command.kind === 'restore' ? command.source : command.expected
    if ('renderStream' in options) {
      return track(
        withStreamHistory(
          options,
          p,
          source,
          command.kind,
          undo,
          signal,
          (stage, fileName, change) => {
            let write: StreamHistoryWrite
            if (command.kind === 'restore') write = { command, stage, fileName }
            else {
              if (!change) throw new Error('missing compensation')
              checkSubmit(command.expected.version, change.ops)
              write = { command, change }
            }
            return server.commitHistory(p.documentId, command.expected.epoch, () => {
              signal.throwIfAborted()
              return options.store.commitHistoryStream(p, write, signal)
            })
          },
        ),
      )
    }
    const { file, change } = await artifact(options, p, source, command.kind, undo)
    let write: HistoryWrite
    if (command.kind === 'restore') write = { command, artifact: file }
    else {
      const compensation = file.compensation!
      checkSubmit(command.expected.version, compensation)
      write = change ? { command, compensation, change } : { command, compensation }
    }
    return server.commitHistory(p.documentId, command.expected.epoch, () =>
      options.store.commitHistory(p, write),
    )
  }

  return {
    accept: server.accept,
    reset: server.reset,
    async close() {
      shutdown.abort(new Error('server closed'))
      await Promise.all([server.close(), Promise.allSettled(staging)])
    },
    async exportHistory(principal, source, signal?: AbortSignal) {
      const p = principalFor(principal, 'export')
      if ('renderStream' in options) {
        const active = signalFor(signal)
        return track(
          withStreamHistory(options, p, point(source), 'export', undefined, active, (stage, name) =>
            server.inDocumentQueue(p.documentId, async () => {
              await store.authorizeHistory(p, 'export')
              active.throwIfAborted()
              return options.store.publishHistoryExport(p, stage, name, active)
            }),
          ),
        )
      }
      const { file } = await artifact(options, p, point(source), 'export')
      await store.authorizeHistory(p, 'export')
      return file
    },
    async restoreHistory(principal, { source, expected, requestId }, signal?: AbortSignal) {
      const receipt = await mutate(
        principalFor(principal, 'restore'),
        {
          kind: 'restore',
          source: point(source),
          expected: point(expected),
          requestId,
        },
        signalFor(signal),
      )
      if (receipt.kind !== 'restored') throw new Error('invalid restore receipt')
      return receipt
    },
    async undoHistory(
      principal,
      { target: { epoch, v, index }, expected, requestId },
      signal?: AbortSignal,
    ) {
      if (
        epoch !== expected.epoch ||
        !Number.isSafeInteger(v) ||
        v < 1 ||
        v > expected.version ||
        !Number.isSafeInteger(index) ||
        index < 0
      )
        throw new Error('invalid undo target')
      const receipt = await mutate(
        principalFor(principal, 'undo'),
        {
          kind: 'undo',
          expected: point(expected),
          target: { epoch, v, index },
          requestId,
        },
        signalFor(signal),
      )
      if (receipt.kind !== 'undone') throw new Error('invalid undo receipt')
      return receipt
    },
  }
}
