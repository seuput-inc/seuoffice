import {
  assertHistoryCompensation,
  checkHistoryHeader,
  HistoryError,
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
import type { Limits, Principal } from './server.js'
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
  ): Promise<{ fileName: string; compensation?: Json[] }>
}

const MAX_FILE = 50 * 1024 * 1024
const MAX_HISTORY = 32 * 1024 * 1024
const MAX_FRAME = 256 * 1024
const encoder = new TextEncoder()
const encode = (value: unknown) => encoder.encode(JSON.stringify(value))

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
    }
    signal.throwIfAborted()
    return await use(stage, result.fileName, change)
  } finally {
    await stage.abort()
  }
}
