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

  async function admitHwpHistory(
    p: Principal,
    candidate: HwpHistoryCandidate,
    retained?: { base: Uint8Array; changes: Change[] },
  ): Promise<HwpHistoryRefusal | undefined> {
    shutdown.signal.throwIfAborted()
    const next = 'next' in candidate ? candidate.next : undefined
    const { id: epoch, format, baseHash } = candidate
    const version = next?.v ?? 0
    const header = { epoch, version, format, baseHash }
    if ('renderStream' in options)
      return admitHwpStream(options, p, candidate, header, shutdown.signal)
    if (options.store.exactHistoryChange !== true) return 'exact_write_required'
    if (version > HWP_REPLAY.changes || (next && next.ops.length > HWP_REPLAY.ops))
      return 'too_large'

    const seed = 'base' in candidate ? candidate.base : undefined
    const base = seed ?? retained?.base ?? (await store.readBase(p.documentId, epoch))
    if (base.byteLength > HWP_REPLAY.fileBytes) return 'too_large'
    if (!seed && !retained && (await sha256(base)) !== baseHash) return 'schema_mismatch'
    const request: HistoryRequest = { ...header, changes: [] }
    let bytes = utf8Length(JSON.stringify(request))
    let ops = 0
    for (let v = 1; v <= version; v++) {
      // Older epochs need not meet today's byte budget. Read only one stored change at a time.
      shutdown.signal.throwIfAborted()
      const frame =
        v === version
          ? next
          : retained
            ? retained.changes[v - 1]
            : (await store.read(p.documentId, epoch, v - 1, 1))[0]
      if (frame?.v !== v || !Array.isArray(frame.ops)) return 'schema_mismatch'
      if ((ops += frame.ops.length) > HWP_REPLAY.ops) return 'too_large'
      try {
        bytes += utf8Length(JSON.stringify(frame)) + (v > 1 ? 1 : 0)
      } catch {
        return 'schema_mismatch'
      }
      if (bytes > HWP_REPLAY.historyBytes) return 'too_large'
      request.changes.push(frame)
    }
    try {
      checkPrefix(request, format)
      shutdown.signal.throwIfAborted()
      const result = await options.render(base, request)
      assertHistoryResult(result, false)
      if (result.bytes.byteLength > HWP_REPLAY.fileBytes) return 'too_large'
      if (!result.fileName.toLowerCase().endsWith(`.${format.name}`)) return 'schema_mismatch'
      if (
        seed &&
        (result.bytes.length !== seed.length || !result.bytes.every((b, i) => b === seed[i]))
      )
        return 'schema_mismatch'
    } catch {
      shutdown.signal.throwIfAborted()
      return 'schema_mismatch'
    }
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
    const prefix = undo ? { ...request, undo } : request
    checkPrefix(prefix, request.format)
    if (undo) await store.authorizeUndoTarget(p, undo)
    const hwpUndo = undo && isHwpHistory(request.format)
    if (hwpUndo && bytes.store.exactHistoryChange !== true)
      throw new HistoryError('unsupported', 'HWP undo requires exact history writes')
    const result = await bytes.render(baseBytes, prefix)
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
        { base: baseBytes, changes: request.changes },
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
