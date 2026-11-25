import {
  assertHistoryResult,
  checkPrefix,
  HistoryError,
  type HistoryOpId,
  type HistoryRequest,
  type HistoryResult,
} from '../history.js'
import { deferred } from '../deferred.js'
import type { Change, Json } from '../model.js'
import { sha256 } from '../sha256.js'
import { createHwpHistory, type HwpArtifact, type HwpCheckpointStore } from './hwp-history.js'
import {
  createServerCore,
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
  renderStreamBytes,
  withStreamHistory,
  type StreamHistoryArtifact,
  type StreamHistoryOptions,
  type StreamHistoryWrite,
} from './stream-history.js'

export type { HwpCheckpoint, HwpCheckpointStore } from './hwp-history.js'

/** HWP admissions this server renders at once; the others wait their turn */
const HWP_ADMISSIONS = 4

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
export interface RetainedHistoryStore extends CollabStore, HwpCheckpointStore {
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

  const hwp = createHwpHistory({
    store: options.store,
    signal: shutdown.signal,
    render:
      'renderStream' in options
        ? (base, request, author) =>
            renderStreamBytes(options, base, request, shutdown.signal, author)
        : (base, request) => options.render(base, request),
  })

  // Admissions of different documents run in their own queues; after a restart every reconnecting
  // editor's join replays its epoch, so a few sandboxes at a time keep each within its time limit.
  let admitting = 0
  const admissionsWaiting: (() => void)[] = []
  async function admitHwpHistory(
    p: Principal,
    candidate: HwpHistoryCandidate,
  ): Promise<HwpHistoryRefusal | undefined> {
    shutdown.signal.throwIfAborted()
    while (admitting >= HWP_ADMISSIONS) {
      const { promise, resolve } = deferred<void>()
      admissionsWaiting.push(resolve)
      await promise
    }
    admitting++
    try {
      return await hwp.admit(p, candidate)
    } finally {
      admitting--
      admissionsWaiting.shift()?.()
    }
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
  ): Promise<HwpArtifact> {
    const { baseBytes, request } = await bytes.store.readHistory(p, source, action)
    if (
      request.epoch !== source.epoch ||
      request.version !== source.version ||
      request.undo !== undefined ||
      request.baseHash !== (await sha256(baseBytes))
    )
      throw new Error('invalid retained history')
    checkPrefix(undo ? { ...request, undo } : request, request.format)
    if (undo) await store.authorizeUndoTarget(p, undo)
    if (isHwpHistory(request.format)) {
      if (undo && bytes.store.exactHistoryChange !== true)
        throw new HistoryError('unsupported', 'HWP undo requires exact history writes')
      const { format, baseHash, changes } = request
      return hwp.artifact(p, source, { format, baseHash, base: baseBytes, changes }, undo)
    }
    const result = await bytes.render(baseBytes, undo ? { ...request, undo } : request)
    assertHistoryResult(result, !!undo)
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
    // An undo that replayed from an unstored checkpoint stores it first, inside the document queue:
    // the committed head must always be replayable from a stored checkpoint. When it cannot be
    // stored the undo is refused and nothing changes. The checkpoint is a confirmed version, so
    // it stays a correct file even if the commit is then refused.
    let keep: (() => Promise<void>) | undefined
    const committed = (commit: () => Promise<HistoryReceipt>) =>
      server.commitHistory(p.documentId, command.expected.epoch, async () => {
        await keep?.()
        return commit()
      })
    if ('renderStream' in options) {
      return track(
        withStreamHistory(
          options,
          p,
          source,
          command.kind,
          undo,
          signal,
          async (retained) => {
            const result = await hwp.artifact(p, source, retained, undo)
            keep = result.keep
            return result
          },
          (stage, fileName, change) => {
            let write: StreamHistoryWrite
            if (command.kind === 'restore') write = { command, stage, fileName }
            else {
              if (!change) throw new Error('missing compensation')
              checkSubmit(command.expected.version, change.ops)
              write = { command, change }
            }
            return committed(() => {
              signal.throwIfAborted()
              return options.store.commitHistoryStream(p, write, signal)
            })
          },
        ),
      )
    }
    const result = await artifact(options, p, source, command.kind, undo)
    const { file, change } = result
    keep = result.keep
    let write: HistoryWrite
    if (command.kind === 'restore') write = { command, artifact: file }
    else {
      const compensation = file.compensation!
      checkSubmit(command.expected.version, compensation)
      write = change ? { command, compensation, change } : { command, compensation }
    }
    return committed(() => options.store.commitHistory(p, write))
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
          withStreamHistory(
            options,
            p,
            point(source),
            'export',
            undefined,
            active,
            (retained) => hwp.artifact(p, point(source), retained),
            (stage, name) =>
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
