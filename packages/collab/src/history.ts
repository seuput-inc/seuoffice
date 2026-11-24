import type { CollabAdapter } from './client/api.js'
import { MAX_CHANGE_BYTES, SUBMIT_FRAME_BYTES } from './limits.js'
import type { Change, Format, Json } from './model.js'
import { sha256 } from './sha256.js'

export interface HistoryOpId {
  epoch: string
  v: number
  index: number
}

export interface HistoryRequest {
  epoch: string
  format: Format
  baseHash: string
  /** Exactly versions `start + 1` through `version`, from the named epoch. */
  changes: Change[]
  /**
   * The versions the base file already contains: 0 for the epoch's first file, or the version
   * of a retained HWP checkpoint. Absent means 0.
   */
  start?: number
  version: number
  undo?: HistoryOpId
}

export interface HistoryResult {
  fileName: string
  bytes: Uint8Array
  /** When undoing, the ops to append at the requested head. The file already includes them. */
  compensation?: Json[]
}

export class HistoryError extends Error {
  constructor(
    readonly code: 'invalid_history' | 'unsupported' | 'conflict',
    detail?: string,
  ) {
    super(detail ?? code)
    this.name = 'HistoryError'
  }
}

function invalid(detail: string): never {
  throw new HistoryError('invalid_history', detail)
}

const count = (n: unknown): n is number =>
  typeof n === 'number' && Number.isSafeInteger(n) && n >= 0
const id = (s: unknown): boolean => typeof s === 'string' && s.length > 0 && s.length <= 256

// A structured clone may hold cycles, sparse arrays, typed arrays or non-finite numbers; a log cannot.
function checkOps(ops: unknown): asserts ops is Json[] {
  if (!Array.isArray(ops) || !ops.length) invalid('empty or missing op list')
  let remaining = MAX_CHANGE_BYTES
  function visit(value: unknown, depth: number): void {
    if (--remaining < 0 || depth > 256) invalid('op exceeds history limits')
    if (typeof value === 'string') {
      remaining -= value.length
    } else if (Array.isArray(value)) {
      for (let i = 0; i < value.length; i++) visit(value[i], depth + 1)
    } else if (value !== null && typeof value === 'object') {
      const proto: unknown = Object.getPrototypeOf(value)
      if (proto !== null && proto !== Object.prototype) invalid('non-JSON object')
      for (const [key, child] of Object.entries(value)) {
        remaining -= key.length
        visit(child, depth + 1)
      }
    } else if (value !== null && typeof value !== 'boolean' && !Number.isFinite(value)) {
      invalid('non-JSON value')
    }
    if (remaining < 0) invalid('op exceeds history limits')
  }
  visit(ops, 0)
}

export function assertHistoryResult(value: unknown, undo: boolean): asserts value is HistoryResult {
  if (!value || typeof value !== 'object') invalid('no history file')
  const { fileName, bytes, compensation } = value as Partial<HistoryResult>
  if (
    typeof fileName !== 'string' ||
    !fileName ||
    !ArrayBuffer.isView(bytes) ||
    Object.prototype.toString.call(bytes) !== '[object Uint8Array]' ||
    !bytes.byteLength
  )
    invalid('empty history file')
  assertHistoryCompensation(compensation, undo)
}

export function assertHistoryCompensation(compensation: unknown, undo: boolean): void {
  if (undo) {
    checkOps(compensation)
    if (
      new TextEncoder().encode(JSON.stringify(compensation)).byteLength + SUBMIT_FRAME_BYTES >
      MAX_CHANGE_BYTES
    )
      invalid('compensation does not fit one change')
  } else if (compensation !== undefined) invalid('unexpected compensation')
}

export function checkHistoryHeader(request: Omit<HistoryRequest, 'changes'>, format: Format): void {
  if (
    !request ||
    !id(request.epoch) ||
    !count(request.version) ||
    (request.start !== undefined && (!count(request.start) || request.start > request.version)) ||
    !request.format ||
    !id(request.format.name) ||
    !count(request.format.schema) ||
    typeof request.baseHash !== 'string' ||
    !/^[0-9a-f]{64}$/.test(request.baseHash)
  )
    invalid('incomplete history prefix')
  if (request.format.name !== format.name || request.format.schema !== format.schema)
    throw new HistoryError('unsupported', 'history format or schema differs from the editor')
  const target = request.undo
  if (
    target !== undefined &&
    (!target ||
      target.epoch !== request.epoch ||
      !count(target.v) ||
      target.v <= (request.start ?? 0) ||
      target.v > request.version ||
      !count(target.index))
  )
    invalid('op not in this prefix')
}

export function checkHistoryChange(
  change: Change,
  version: number,
  seen: Map<string, Set<number>>,
  target?: HistoryOpId,
): void {
  if (
    !change ||
    change.v !== version ||
    !id(change.client) ||
    !count(change.seq) ||
    typeof change.author !== 'string'
  )
    invalid('invalid change in prefix')
  let seqs = seen.get(change.client)
  if (!seqs) seen.set(change.client, (seqs = new Set()))
  if (seqs.has(change.seq)) invalid('duplicate change in prefix')
  seqs.add(change.seq)
  checkOps(change.ops)
  if (target?.v === version && target.index >= change.ops.length) invalid('op not in this prefix')
}

export function checkPrefix(request: HistoryRequest, format: Format): void {
  const start = request?.start ?? 0
  if (!Array.isArray(request?.changes) || request.changes.length !== request.version - start)
    invalid('incomplete history prefix')
  checkHistoryHeader(request, format)
  const seen = new Map<string, Set<number>>()
  for (let i = 0; i < request.changes.length; i++)
    checkHistoryChange(request.changes[i]!, start + i + 1, seen, request.undo)
}

/** The caller owns a fresh editor displaying these base bytes and disposes it even on failure. */
export async function replayHistory<Op>(
  adapter: CollabAdapter<Op>,
  format: Format,
  baseBytes: Uint8Array,
  request: HistoryRequest,
): Promise<HistoryResult> {
  const hash = await sha256(baseBytes)
  checkPrefix(request, format)
  if (hash !== request.baseHash) invalid('base file does not match its epoch')
  const target = request.undo
  if (target && !adapter.captureUndo) throw new HistoryError('unsupported')
  let local = false
  const unlisten = adapter.listen(() => {
    local = true
  })
  const apply = (ops: Op[]) => {
    if (ops.length && adapter.integrate(ops, []).length) invalid('history editor has pending edits')
    if (local) invalid('history editor emitted a local edit')
  }
  try {
    adapter.setEditable(false)
    let undo: ((later: readonly Op[]) => readonly Op[] | null) | undefined
    const later: Op[] = []
    for (const change of request.changes) {
      const ops = change.ops.map((op) => adapter.decode(op))
      if (target && change.v === target.v) {
        apply(ops.slice(0, target.index))
        undo = adapter.captureUndo!(ops[target.index]!)
        if (typeof undo !== 'function') throw new HistoryError('unsupported')
        for (let i = target.index + 1; i < ops.length; i++) later.push(ops[i]!)
        apply(ops.slice(target.index))
      } else {
        if (undo) for (const op of ops) later.push(op)
        apply(ops)
      }
    }
    let compensation: Json[] | undefined
    if (undo) {
      const planned = undo(later)
      if (!planned?.length) throw new HistoryError('conflict')
      compensation = planned.map((op) => adapter.encode(op))
      checkOps(compensation)
      // Produce the same file a peer gets when it receives the encoded compensation.
      apply(compensation.map((op) => adapter.decode(op)))
    }
    const file = await adapter.produceFile()
    if (local) invalid('history editor emitted a local edit')
    const result = compensation ? { ...file, compensation } : file
    assertHistoryResult(result, !!target)
    return result
  } catch (error) {
    if (error instanceof HistoryError) throw error
    throw new HistoryError('invalid_history', 'the editor could not replay this history')
  } finally {
    unlisten()
  }
}
