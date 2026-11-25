/**
 * HWP retained history replays the whole epoch in an isolated renderer, which accepts a bounded
 * request. Once the next change would not fit, the server renders the head, admits that file as
 * it admits an epoch base, and keeps it as a checkpoint: later requests start from the newest
 * checkpoint, with changes renumbered from 1, so the renderer sees an ordinary short history.
 */

import {
  assertHistoryResult,
  checkPrefix,
  HistoryError,
  type HistoryOpId,
  type HistoryRequest,
  type HistoryResult,
} from '../history.js'
import type { Change, Format } from '../model.js'
import { sha256 } from '../sha256.js'
import { utf8Length } from '../wire.js'
import {
  HWP_REPLAY,
  type HwpHistoryCandidate,
  type HwpHistoryRefusal,
  type Principal,
} from './server.js'
import type { CollabStore } from './store.js'

/** The file at `version` of one HWP epoch, produced by the renderer and admitted like a base. */
export interface HwpCheckpoint {
  version: number
  baseHash: string
  base: Uint8Array
}

/** The optional checkpoint pair of a retained history store: both methods, or neither. */
export interface HwpCheckpointStore {
  /**
   * Keep the file at `checkpoint.version` of the named epoch for good. First write wins: a later
   * write for the same version changes nothing. Without the pair, HWP retained history refuses a
   * change once its epoch passes the replay limits, and the editor's session ends there.
   */
  writeHwpCheckpoint?(documentId: string, epochId: string, checkpoint: HwpCheckpoint): Promise<void>
  /** The checkpoint with the highest version at or below `atMost`, or null; also after drop. */
  readHwpCheckpoint?(
    documentId: string,
    epochId: string,
    atMost: number,
  ): Promise<HwpCheckpoint | null>
}

/** A retained point read under the caller's grant: the epoch base and versions 1..version. */
export interface HwpRetained {
  format: Format
  baseHash: string
  base: Uint8Array
  changes: Change[]
}

/** The renderer as a function of whole bytes; `author` is the compensation author of an undo. */
export type HwpRender = (
  base: Uint8Array,
  request: HistoryRequest,
  author?: string,
) => Promise<HistoryResult>

/** A history file, the compensation of an undo, and the checkpoint an undo needs once committed. */
export interface HwpArtifact {
  file: HistoryResult
  change?: Change
  keep?: () => Promise<void>
}
/** Where a replay starts: the epoch base (start 0) or a checkpoint, with changes already read. */
interface Window {
  base: Uint8Array
  baseHash: string
  start: number
  changes?: Change[]
}

/** The epoch a replay belongs to. */
type EpochRef = Pick<HwpHistoryCandidate, 'id' | 'format' | 'baseHash'>

const OLDER_THAN_CHECKPOINT =
  'This edit is older than the saved history point and can no longer be undone on its own'

function sameBytes(a: Uint8Array, b: Uint8Array): boolean {
  return a.length === b.length && a.every((byte, i) => byte === b[i])
}

/** Whether the renderer accepts `request`: the same counts, node, depth and text budget it checks. */
export function fitsHwpReplay(request: HistoryRequest): boolean {
  if (request.changes.length > HWP_REPLAY.changes) return false
  if (utf8Length(JSON.stringify(request)) > HWP_REPLAY.requestBytes) return false
  let ops = 0
  for (const change of request.changes) {
    if ((ops += change.ops.length) > HWP_REPLAY.ops) return false
    if (utf8Length(JSON.stringify(change)) > HWP_REPLAY.changeBytes) return false
  }
  let nodes = HWP_REPLAY.nodes
  let text = HWP_REPLAY.textBytes
  const visit = (value: unknown, depth: number): boolean => {
    if (--nodes < 0 || depth > HWP_REPLAY.depth) return false
    if (typeof value === 'string') return (text -= utf8Length(value)) >= 0
    if (value === null || typeof value !== 'object') return true
    if (Array.isArray(value)) return value.every((field) => visit(field, depth + 1))
    return Object.entries(value).every(
      ([key, field]) => (text -= utf8Length(key)) >= 0 && visit(field, depth + 1),
    )
  }
  return visit(request, 0)
}

export function createHwpHistory({
  store,
  render,
  signal,
}: {
  store: CollabStore & HwpCheckpointStore
  render: HwpRender
  signal: AbortSignal
}) {
  const checkpoints =
    typeof store.writeHwpCheckpoint === 'function' && typeof store.readHwpCheckpoint === 'function'
      ? (store as Required<HwpCheckpointStore>)
      : null

  async function latest(documentId: string, epoch: string, atMost: number): Promise<Window | null> {
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

  /** The renderer request for versions `from.start + 1` through `version`, renumbered from 1. */
  async function prefix(
    documentId: string,
    { id: epoch, format }: EpochRef,
    version: number,
    from: Window,
    next?: Change,
    undo?: HistoryOpId,
  ): Promise<{ base: Uint8Array; request: HistoryRequest } | HwpHistoryRefusal> {
    const { base, baseHash, start } = from
    if (version - start > HWP_REPLAY.changes || base.byteLength > HWP_REPLAY.fileBytes)
      return 'too_large'
    if ((await sha256(base)) !== baseHash) return 'schema_mismatch'
    const changes: Change[] = []
    for (let v = start + 1; v <= version; v++) {
      signal.throwIfAborted()
      const frame =
        v === version && next
          ? next
          : from.changes
            ? from.changes[v - start - 1]
            : (await store.read(documentId, epoch, v - 1, 1))[0]
      if (frame?.v !== v || !Array.isArray(frame.ops)) return 'schema_mismatch'
      changes.push({ ...frame, v: v - start })
    }
    const request: HistoryRequest = {
      epoch,
      version: version - start,
      format,
      baseHash,
      changes,
      ...(undo ? { undo: { ...undo, v: undo.v - start } } : {}),
    }
    try {
      if (!fitsHwpReplay(request)) return 'too_large'
    } catch {
      return 'schema_mismatch'
    }
    return { base, request }
  }

  async function renderChecked(
    base: Uint8Array,
    request: HistoryRequest,
  ): Promise<HistoryResult | HwpHistoryRefusal> {
    try {
      checkPrefix(request, request.format)
      signal.throwIfAborted()
      const result = await render(base, request)
      assertHistoryResult(result, false)
      if (result.bytes.byteLength > HWP_REPLAY.fileBytes) return 'too_large'
      if (!result.fileName.toLowerCase().endsWith(`.${request.format.name}`))
        return 'schema_mismatch'
      return result
    } catch {
      signal.throwIfAborted()
      return 'schema_mismatch'
    }
  }

  /**
   * The file at `head`, rendered from `from` and reopened unchanged: the admission a new epoch's
   * base passes. `keep` stores it; until then nothing outlives the call.
   */
  async function checkpointAt(
    documentId: string,
    candidate: EpochRef,
    head: number,
    from: Window,
  ): Promise<{ window: Window; keep(): Promise<Window | HwpHistoryRefusal> } | HwpHistoryRefusal> {
    if (!checkpoints || head <= from.start) return 'too_large'
    const built = await prefix(documentId, candidate, head, from)
    if (typeof built === 'string') return built
    const file = await renderChecked(built.base, built.request)
    if (typeof file === 'string') return file
    const baseHash = await sha256(file.bytes)
    const reopened = await renderChecked(file.bytes, {
      epoch: candidate.id,
      version: 0,
      format: candidate.format,
      baseHash,
      changes: [],
    })
    if (typeof reopened === 'string') return reopened
    if (!sameBytes(reopened.bytes, file.bytes)) return 'schema_mismatch'
    const window = { base: file.bytes, baseHash, start: head }
    return {
      window,
      async keep() {
        signal.throwIfAborted()
        // A failed write ends this editor's session with a reason instead of a reconnect loop.
        try {
          await checkpoints.writeHwpCheckpoint(documentId, candidate.id, {
            version: head,
            baseHash,
            base: file.bytes,
          })
          // First write wins: replay from whatever the store kept for this version.
          const kept = await latest(documentId, candidate.id, head)
          return kept?.start === head ? kept : 'schema_mismatch'
        } catch {
          signal.throwIfAborted()
          return 'schema_mismatch'
        }
      },
    }
  }

  /** Admit a new epoch base, the next change on top of the retained head, or the head itself. */
  async function admit(
    p: Principal,
    candidate: HwpHistoryCandidate,
    retained?: Window,
  ): Promise<HwpHistoryRefusal | undefined> {
    signal.throwIfAborted()
    const next = 'next' in candidate ? candidate.next : undefined
    const version = next ? next.v : 'head' in candidate ? candidate.head : 0
    const replayed = next ? version - 1 : version
    if (next && next.ops.length > HWP_REPLAY.ops) return 'too_large'
    const seed = 'base' in candidate ? candidate.base : undefined
    const from: Window = seed
      ? { base: seed, baseHash: candidate.baseHash, start: 0, changes: [] }
      : (retained ??
        (await latest(p.documentId, candidate.id, replayed)) ?? {
          base: await store.readBase(p.documentId, candidate.id),
          baseHash: candidate.baseHash,
          start: 0,
        })
    let built = await prefix(p.documentId, candidate, version, from, next)
    if (built === 'too_large' && next && !seed) {
      // The replay window is full: continue from a retained file of the current head.
      const checkpoint = await checkpointAt(p.documentId, candidate, version - 1, from)
      if (typeof checkpoint === 'string') return checkpoint
      const kept = await checkpoint.keep()
      if (typeof kept === 'string') return kept
      built = await prefix(p.documentId, candidate, version, kept, next)
    }
    if (typeof built === 'string') return built
    const result = await renderChecked(built.base, built.request)
    if (typeof result === 'string') return result
    if (seed && !sameBytes(result.bytes, seed)) return 'schema_mismatch'
    signal.throwIfAborted()
    return undefined
  }

  async function renderWindow(
    p: Principal,
    candidate: EpochRef,
    version: number,
    window: Window,
    undo: HistoryOpId | undefined,
  ): Promise<HistoryResult> {
    const built = await prefix(p.documentId, candidate, version, window, undefined, undo)
    if (built === 'too_large')
      throw new HistoryError('unsupported', 'HWP history exceeds its replay limits')
    if (typeof built === 'string')
      throw new HistoryError('invalid_history', 'invalid retained HWP history')
    const result = await render(built.base, built.request, undo ? p.userId : undefined)
    assertHistoryResult(result, !!undo)
    return result
  }

  /**
   * Export, restore or undo at `point` from the newest checkpoint at or below it. An undo whose
   * window is too full to hold its compensation replays from a file just before its target.
   * `keep` stores that file; call it inside the document queue before the undo commits and do
   * not commit when it throws, so the committed head always replays from a stored checkpoint.
   */
  async function artifact(
    p: Principal,
    point: { epoch: string; version: number },
    retained: HwpRetained,
    undo?: HistoryOpId,
  ): Promise<HwpArtifact> {
    const { format } = retained
    const sliced = (from: Window) => ({ ...from, changes: retained.changes.slice(from.start) })
    const checkpoint = await latest(p.documentId, point.epoch, point.version)
    let window = sliced(
      checkpoint ?? { base: retained.base, baseHash: retained.baseHash, start: 0 },
    )
    if (undo && undo.v <= window.start) throw new HistoryError('unsupported', OLDER_THAN_CHECKPOINT)
    const candidate = { id: point.epoch, format, baseHash: retained.baseHash }
    let file: HistoryResult
    let narrowed: (() => Promise<Window | HwpHistoryRefusal>) | undefined
    try {
      file = await renderWindow(p, candidate, point.version, window, undo)
    } catch (error) {
      if (!undo || !(error instanceof HistoryError) || error.code !== 'unsupported') throw error
      if (undo.v - 1 <= window.start) throw error
      const narrow = await checkpointAt(p.documentId, candidate, undo.v - 1, window)
      if (typeof narrow === 'string') throw error
      window = sliced(narrow.window)
      file = await renderWindow(p, candidate, point.version, window, undo)
      narrowed = narrow.keep
    }
    if (!undo) return { file }
    const change: Change = {
      v: point.version + 1,
      client: `history:${crypto.randomUUID()}`,
      seq: 1,
      author: p.userId,
      ops: file.compensation!,
    }
    const refused = await admit(p, { ...candidate, next: change }, window)
    if (refused)
      throw new HistoryError('unsupported', `HWP compensation cannot be replayed (${refused})`)
    if (!narrowed) return { file, change }
    const keep = narrowed
    return {
      file,
      change,
      async keep() {
        for (let attempt = 0; attempt < 2; attempt++) if (typeof (await keep()) !== 'string') return
        throw new HistoryError(
          'unsupported',
          'This edit could not be undone because the saved history point it needs could not be stored; nothing was changed',
        )
      },
    }
  }

  return { admit, artifact }
}
