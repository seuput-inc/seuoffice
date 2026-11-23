import {
  assertHistoryResult,
  checkPrefix,
  type HistoryOpId,
  type HistoryRequest,
  type HistoryResult,
} from '../history.js'
import type { Json } from '../model.js'
import { sha256 } from '../sha256.js'
import { createServerCore, type CollabServer, type Limits, type Principal } from './server.js'
import type { CollabStore } from './store.js'
import { utf8Length } from '../wire.js'
import { MAX_CHANGE_BYTES } from '../limits.js'

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
  | { command: { kind: 'undo' } & UndoRequest; compensation: Json[] }

/** Trusted persistence port. Never map these methods directly to an HTTP or socket handler. */
export interface RetainedHistoryStore extends CollabStore {
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

export interface HistoryServer extends CollabServer {
  exportHistory(principal: Principal, source: HistoryPoint): Promise<HistoryArtifact>
  restoreHistory(principal: Principal, input: RestoreRequest): Promise<RestoreReceipt>
  undoHistory(principal: Principal, input: UndoRequest): Promise<UndoReceipt>
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

/** Route every socket and history command for a document to its one owning server. */
export function createHistoryServer(options: {
  store: RetainedHistoryStore
  limits?: Partial<Limits>
  /** Host-controlled isolated editor worker, never a result uploaded by the requesting user. */
  render: (baseBytes: Uint8Array, request: HistoryRequest) => Promise<HistoryResult>
}): HistoryServer {
  const { store, render } = options
  const server = createServerCore(options)
  let closed = false

  function principalFor(p: Principal, action: HistoryAction): Principal {
    if (closed || !p?.documentId || !p.userId || (action !== 'export' && p.canEdit !== true))
      throw new Error('history forbidden')
    return { ...p }
  }

  async function artifact(
    p: Principal,
    source: HistoryPoint,
    action: HistoryAction,
    undo?: UndoRequest['target'],
  ): Promise<HistoryResult> {
    const { baseBytes, request } = await store.readHistory(p, source, action)
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
    const result = await render(baseBytes, prefix)
    assertHistoryResult(result, !!undo)
    return result
  }

  async function mutate(p: Principal, command: HistoryCommand): Promise<HistoryReceipt> {
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
        return receipt
      })
    }
    const undo = command.kind === 'undo' ? command.target : undefined
    const source = command.kind === 'restore' ? command.source : command.expected
    const file = await artifact(p, source, command.kind, undo)
    let write: HistoryWrite
    if (command.kind === 'restore') write = { command, artifact: file }
    else {
      const compensation = file.compensation!
      const frame = { t: 'submit', seq: 1, base: command.expected.version, ops: compensation }
      if (utf8Length(JSON.stringify(frame)) > (options.limits?.maxChangeBytes ?? MAX_CHANGE_BYTES))
        throw new Error('compensation too large')
      write = { command, compensation }
    }
    return server.commitHistory(p.documentId, command.expected.epoch, () =>
      store.commitHistory(p, write),
    )
  }

  return {
    accept: server.accept,
    reset: server.reset,
    close() {
      closed = true
      return server.close()
    },
    async exportHistory(principal, source) {
      const p = principalFor(principal, 'export')
      const file = await artifact(p, point(source), 'export')
      await store.authorizeHistory(p, 'export')
      return file
    },
    async restoreHistory(principal, { source, expected, requestId }) {
      const receipt = await mutate(principalFor(principal, 'restore'), {
        kind: 'restore',
        source: point(source),
        expected: point(expected),
        requestId,
      })
      if (receipt.kind !== 'restored') throw new Error('invalid restore receipt')
      return receipt
    },
    async undoHistory(principal, { target: { epoch, v, index }, expected, requestId }) {
      if (
        epoch !== expected.epoch ||
        !Number.isSafeInteger(v) ||
        v < 1 ||
        v > expected.version ||
        !Number.isSafeInteger(index) ||
        index < 0
      )
        throw new Error('invalid undo target')
      const receipt = await mutate(principalFor(principal, 'undo'), {
        kind: 'undo',
        expected: point(expected),
        target: { epoch, v, index },
        requestId,
      })
      if (receipt.kind !== 'undone') throw new Error('invalid undo receipt')
      return receipt
    },
  }
}
