import {
  assertHistoryCompensation,
  checkHistoryHeader,
  HistoryError,
  type HistoryOpId,
  type HistoryRequest,
  type HistoryResult,
} from '../history.js'
import type { Change, Format, Json } from '../model.js'
import type {
  HistoryAction,
  HistoryCommand,
  HistoryPoint,
  HistoryReceipt,
  RetainedHistoryStore,
} from './history.js'
import type { HwpCheckpointStore, HwpRetained } from './hwp-history.js'
import { HWP_REPLAY, isHwpHistory, type Limits, type Principal } from './server.js'
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

/** One retained point as a stream store returns it. */
export interface RetainedStream extends HistoryPoint {
  format: Format
  baseHash: string
  source: HistoryByteSource
  changes: AsyncIterable<Uint8Array>
}

export interface StreamHistoryStore
  extends
    CollabStore,
    HwpCheckpointStore,
    Pick<RetainedHistoryStore, 'authorizeHistory' | 'authorizeUndoTarget' | 'historyReceipt'> {
  /** Return an immutable retained source, trusted coordinates and opaque Change frames for exactly 1..version. */
  readHistoryStream(
    principal: Principal,
    point: HistoryPoint,
    action: HistoryAction,
  ): Promise<RetainedStream>
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
const decoder = new TextDecoder('utf-8', { fatal: true })
const encode = (value: unknown) => encoder.encode(JSON.stringify(value))

/** Render a small HWP history through the stream renderer, keeping the whole file in memory. */
export async function renderStreamBytes(
  options: StreamHistoryOptions,
  base: Uint8Array,
  request: HistoryRequest,
  signal: AbortSignal,
  author?: string,
): Promise<HistoryResult> {
  const { changes, ...header } = request
  const parts: Uint8Array[] = []
  let size = 0
  const result = await options.renderStream(
    {
      byteLength: base.byteLength,
      async *open() {
        yield base
      },
    },
    encode(header),
    (async function* () {
      for (const change of changes) yield encode(change)
    })(),
    {
      async write(bytes) {
        signal.throwIfAborted()
        if ((size += bytes.byteLength) > HWP_REPLAY.fileBytes)
          throw new HistoryError('unsupported', 'HWP output exceeds 2 MiB')
        parts.push(new Uint8Array(bytes))
      },
      async abort() {
        parts.length = 0
        size = 0
      },
    },
    signal,
    author,
  )
  const bytes = new Uint8Array(size)
  let offset = 0
  for (const part of parts) {
    bytes.set(part, offset)
    offset += part.byteLength
  }
  return {
    fileName: result.fileName,
    bytes,
    ...(result.compensation === undefined ? {} : { compensation: result.compensation }),
  }
}

/** Read a retained HWP point into memory: its base is at most 2 MiB, its log bounded like any stream. */
async function retainedHwp(
  retained: RetainedStream,
  point: HistoryPoint,
  signal: AbortSignal,
): Promise<HwpRetained> {
  const { source, format, baseHash } = retained
  if (source.byteLength > HWP_REPLAY.fileBytes)
    throw new HistoryError('unsupported', 'HWP source exceeds 2 MiB')
  const base = new Uint8Array(source.byteLength)
  let offset = 0
  for await (const bytes of source.open()) {
    signal.throwIfAborted()
    if (offset + bytes.byteLength > base.byteLength)
      throw new HistoryError('invalid_history', 'retained source is longer than declared')
    base.set(bytes, offset)
    offset += bytes.byteLength
  }
  if (offset !== base.byteLength)
    throw new HistoryError('invalid_history', 'retained source is shorter than declared')
  const changes: Change[] = []
  let total = 0
  for await (const bytes of retained.changes) {
    signal.throwIfAborted()
    if (
      changes.length >= point.version ||
      !bytes.byteLength ||
      bytes.byteLength > MAX_FRAME ||
      (total += bytes.byteLength) > MAX_HISTORY
    )
      throw new HistoryError('invalid_history', 'invalid retained stream length')
    changes.push(JSON.parse(decoder.decode(bytes)))
  }
  if (changes.length !== point.version)
    throw new HistoryError('invalid_history', 'incomplete retained stream')
  return { format, baseHash, base, changes }
}

export async function withStreamHistory<T>(
  options: StreamHistoryOptions,
  p: Principal,
  point: HistoryPoint,
  action: HistoryAction,
  target: HistoryOpId | undefined,
  signal: AbortSignal,
  hwp: (retained: HwpRetained) => Promise<{ file: HistoryResult; change?: Change }>,
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
    if (isHwpHistory(header.format)) {
      // HWP replays from its newest checkpoint, so the whole bounded window is read into memory.
      const { file, change } = await hwp(await retainedHwp(retained, point, signal))
      if (!change) await stage.write(file.bytes)
      signal.throwIfAborted()
      return await use(stage, file.fileName, change)
    }
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
    }
    signal.throwIfAborted()
    return await use(stage, result.fileName, change)
  } finally {
    await stage.abort()
  }
}
