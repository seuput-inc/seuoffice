import {
  assertHistoryCompensation,
  checkHistoryHeader,
  HistoryError,
  type HistoryRequest,
  type HistoryOpId,
} from '../history.js'
import type { Change, Format, Json } from '../model.js'
import type {
  HistoryAction,
  HistoryCommand,
  HistoryPoint,
  HistoryReceipt,
  RetainedHistoryStore,
} from './history.js'
import {
  HWP_REPLAY,
  isHwpHistory,
  type HwpHistoryCandidate,
  type HwpHistoryRefusal,
  type Limits,
  type Principal,
} from './server.js'
import { sha256 } from '../sha256.js'
import type { CollabStore } from './store.js'

export interface HistoryByteSource {
  byteLength: number
  open(): AsyncIterable<Uint8Array>
}
export interface HistoryStage {
  write(bytes: Uint8Array): Promise<void>
  /** Discard private bytes, including pending writes. Idempotent after abort or ownership transfer. */
  abort(): Promise<void>
}
export interface StreamHistoryArtifact extends HistoryByteSource {
  fileName: string
  dispose(): Promise<void>
}
export type StreamHistoryWrite =
  | { command: Extract<HistoryCommand, { kind: 'restore' }>; stage: HistoryStage; fileName: string }
  | { command: Extract<HistoryCommand, { kind: 'undo' }>; change: Change }

export interface StreamHistoryStore
  extends
    CollabStore,
    Pick<RetainedHistoryStore, 'authorizeHistory' | 'authorizeUndoTarget' | 'historyReceipt'> {
  /** Return an immutable retained source, trusted coordinates and opaque Change frames for exactly 1..version. */
  readHistoryStream(
    principal: Principal,
    point: HistoryPoint,
    action: HistoryAction,
  ): Promise<
    HistoryPoint & {
      format: Format
      baseHash: string
      source: HistoryByteSource
      changes: AsyncIterable<Uint8Array>
    }
  >
  /** Allocate privately and capture the active epoch/head fence for exports (not the requested historical point). */
  createHistoryStage(principal: Principal, action: HistoryAction): Promise<HistoryStage>
  /** Atomically check signal, fence and read grant, then transfer stage ownership to a disposable reader. */
  publishHistoryExport(
    principal: Principal,
    stage: HistoryStage,
    fileName: string,
    signal: AbortSignal,
  ): Promise<StreamHistoryArtifact>
  /** One transaction: authorize including target author, deduplicate intent, compare live head, transfer or append and record receipt. Check signal before commit. */
  commitHistoryStream(
    principal: Principal,
    write: StreamHistoryWrite,
    signal: AbortSignal,
  ): Promise<HistoryReceipt>
}
export interface StreamHistoryOptions {
  store: StreamHistoryStore
  limits?: Partial<Limits>
  renderStream(
    source: HistoryByteSource,
    header: Uint8Array,
    changes: AsyncIterable<Uint8Array>,
    stage: HistoryStage,
    signal: AbortSignal,
    compensationAuthor?: string,
  ): Promise<{ fileName: string; compensation?: Json[] }>
}

const MAX_FILE = 50 * 1024 * 1024
const MAX_HISTORY = 32 * 1024 * 1024
const MAX_FRAME = 256 * 1024
const encoder = new TextEncoder()
const encode = (value: unknown) => encoder.encode(JSON.stringify(value))

/**
 * Render the unpublished candidate. Count/discard its output; never create an export or revision.
 * Without `action` it admits a socket submit: like byte mode it reads the epoch's own log, so an
 * editor needs no history grant. With `action` it reads the retained history under that grant.
 */
export async function admitHwpStream(
  options: StreamHistoryOptions,
  p: Principal,
  candidate: HwpHistoryCandidate,
  header: Omit<HistoryRequest, 'changes'>,
  signal: AbortSignal,
  action?: HistoryAction,
): Promise<HwpHistoryRefusal | undefined> {
  const seed = 'base' in candidate ? candidate.base : undefined
  const next = 'next' in candidate ? candidate.next : undefined
  if (header.version > HWP_REPLAY.changes || (next && next.ops.length > HWP_REPLAY.ops))
    return 'too_large'
  let source: HistoryByteSource
  let prefix: AsyncIterable<Uint8Array> | undefined
  if (seed) {
    source = {
      byteLength: seed.byteLength,
      async *open() {
        yield seed
      },
    }
  } else if (!action) {
    const { store } = options
    const base = await store.readBase(p.documentId, header.epoch)
    if (base.byteLength > HWP_REPLAY.fileBytes) return 'too_large'
    if ((await sha256(base)) !== header.baseHash) return 'schema_mismatch'
    source = {
      byteLength: base.byteLength,
      async *open() {
        yield base
      },
    }
    prefix = (async function* () {
      for (let v = 1; v < header.version; v++) {
        const [change] = await store.read(p.documentId, header.epoch, v - 1, 1)
        if (change?.v !== v)
          throw new HistoryError('invalid_history', 'invalid HWP retained prefix')
        yield encode(change)
      }
    })()
  } else {
    const point = { epoch: header.epoch, version: header.version - 1 }
    const retained = await options.store
      .readHistoryStream(p, point, action)
      .catch(async (error) => {
        try {
          await options.store.authorizeHistory(p, action)
        } catch {
          return null
        }
        throw error
      })
    if (!retained) return 'forbidden'
    if (
      retained.epoch !== point.epoch ||
      retained.version !== point.version ||
      retained.baseHash !== header.baseHash ||
      retained.format.name !== header.format.name ||
      retained.format.schema !== header.format.schema
    )
      return 'schema_mismatch'
    source = retained.source
    prefix = retained.changes
  }
  if (source.byteLength > HWP_REPLAY.fileBytes) return 'too_large'
  if (!Number.isSafeInteger(source.byteLength) || source.byteLength < 1) return 'schema_mismatch'
  const encoded = encode(header)
  let total = encoded.byteLength + ',"changes":[]'.length
  let written = 0
  let count = 0
  let complete = false
  let refusal: HwpHistoryRefusal | undefined
  function overLimit(): never {
    refusal = 'too_large'
    throw new HistoryError('unsupported', 'HWP retained history exceeds replay limits')
  }
  async function* input() {
    if (prefix) yield* prefix
    if (next) yield encode(next)
  }
  async function* frames() {
    for await (const bytes of input()) {
      signal.throwIfAborted()
      total += bytes.byteLength + (count ? 1 : 0)
      if (bytes.byteLength > MAX_FRAME || total > HWP_REPLAY.historyBytes) overLimit()
      if (!bytes.byteLength || ++count > header.version)
        throw new HistoryError('invalid_history', 'invalid HWP retained prefix')
      yield bytes
    }
    complete = count === header.version
  }
  try {
    const result = await options.renderStream(
      source,
      encoded,
      frames(),
      {
        async write(bytes) {
          signal.throwIfAborted()
          if (written + bytes.byteLength > HWP_REPLAY.fileBytes) overLimit()
          if (seed && !bytes.every((value, i) => value === seed[written + i]))
            throw new HistoryError('unsupported', 'HWP renderer changed the base file')
          written += bytes.byteLength
        },
        async abort() {
          written = 0
        },
      },
      signal,
    )
    signal.throwIfAborted()
    if (
      !refusal &&
      complete &&
      written &&
      (!seed || written === seed.byteLength) &&
      result.fileName.toLowerCase().endsWith(`.${header.format.name}`) &&
      result.compensation === undefined
    )
      return undefined
  } catch {
    signal.throwIfAborted()
  }
  return refusal ?? 'schema_mismatch'
}

export async function withStreamHistory<T>(
  options: StreamHistoryOptions,
  p: Principal,
  point: HistoryPoint,
  action: HistoryAction,
  target: HistoryOpId | undefined,
  signal: AbortSignal,
  use: (stage: HistoryStage, fileName: string, change: Change | undefined) => Promise<T>,
): Promise<T> {
  const { store } = options
  signal.throwIfAborted()
  await store.authorizeHistory(p, action)
  if (target) await store.authorizeUndoTarget(p, target)
  signal.throwIfAborted()
  const stage = await store.createHistoryStage(p, action)
  try {
    signal.throwIfAborted()
    const retained = await store.readHistoryStream(p, point, action)
    if (retained.epoch !== point.epoch || retained.version !== point.version)
      throw new HistoryError('invalid_history', 'wrong retained point')
    const { name, schema } = retained.format
    const header = { ...point, format: { name, schema }, baseHash: retained.baseHash }
    const request = target ? { ...header, undo: target } : header
    checkHistoryHeader(request, header.format)
    const encoded = encode(request)
    const fixedBytes = ',"changes":[]'.length
    let logBytes = 0,
      count = 0,
      written = 0,
      complete = false
    async function* frames() {
      for await (const bytes of retained.changes) {
        signal.throwIfAborted()
        logBytes += (count ? 1 : 0) + bytes.byteLength
        if (
          ++count > point.version ||
          !bytes.byteLength ||
          bytes.byteLength > MAX_FRAME ||
          encoded.byteLength + fixedBytes + logBytes > MAX_HISTORY
        )
          throw new HistoryError('invalid_history', 'invalid retained stream length')
        yield bytes
      }
      if (count !== point.version)
        throw new HistoryError('invalid_history', 'incomplete retained stream')
      complete = true
    }
    const result = await options.renderStream(
      retained.source,
      encoded,
      frames(),
      {
        async write(bytes) {
          signal.throwIfAborted()
          written += bytes.byteLength
          if (written > MAX_FILE)
            throw new HistoryError('unsupported', 'stream output exceeds limit')
          await stage.write(bytes)
        },
        abort: () => stage.abort(),
      },
      signal,
      target ? p.userId : undefined,
    )
    signal.throwIfAborted()
    if (!complete || !written || typeof result?.fileName !== 'string' || !result.fileName)
      throw new HistoryError('invalid_history', 'incomplete staged result')
    assertHistoryCompensation(result.compensation, !!target)
    let change: Change | undefined
    if (result.compensation) {
      change = {
        v: point.version + 1,
        client: `history:${crypto.randomUUID()}`,
        seq: 1,
        author: p.userId,
        ops: result.compensation,
      }
      const frame = encode(change)
      const nextHeader = encode({ ...header, version: change.v })
      if (
        frame.byteLength > MAX_FRAME ||
        nextHeader.byteLength + fixedBytes + logBytes + 1 + frame.byteLength > MAX_HISTORY
      )
        throw new HistoryError('unsupported', 'appended change exceeds replay limits')
      await stage.abort()
      if (isHwpHistory(header.format)) {
        const refused = await admitHwpStream(
          options,
          p,
          { id: point.epoch, format: header.format, baseHash: retained.baseHash, next: change },
          { ...header, version: change.v },
          signal,
          action,
        )
        if (refused)
          throw new HistoryError('unsupported', `HWP compensation cannot be replayed (${refused})`)
      }
    }
    signal.throwIfAborted()
    return await use(stage, result.fileName, change)
  } finally {
    await stage.abort()
  }
}
