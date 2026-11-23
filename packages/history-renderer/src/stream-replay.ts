import { createHash } from 'node:crypto'
import { HistoryError, type HistoryRequest, type HistoryResult } from '@seuoffice/collab/client'
import { checkHistoryChange, checkHistoryHeader } from '../../collab/src/history'
import { capturePdfInverse } from '../../../apps/pdf/src/web/collab-adapter'
import {
  emptySnapshot,
  foldPeer,
  type PdfOp,
  type Put,
} from '../../../apps/pdf/src/web/collab-model'
import { decodeOp } from '../../../apps/pdf/src/web/collab-wire'
import { checkJsonBudget } from './request'
import { openStreamPdf } from './stream-pdf'

function invalid(detail: string): never {
  throw new HistoryError('invalid_history', detail)
}
function unsupported(detail: string): never {
  throw new HistoryError('unsupported', detail)
}

type SmallRender = (
  bytes: Uint8Array,
  history: HistoryRequest,
  author: string,
) => Promise<HistoryResult>

export async function streamPlan(input: AsyncIterable<Uint8Array>, smallRender?: SmallRender) {
  const reader = input[Symbol.asyncIterator]()
  let pending: Uint8Array = new Uint8Array()
  async function fill() {
    while (!pending.length) {
      const next = await reader.next()
      if (next.done) return false
      pending = next.value
    }
    return true
  }
  async function* take(length: number) {
    while (length) {
      if (!(await fill())) invalid('truncated stream')
      const n = Math.min(length, pending.length)
      const bytes = pending.subarray(0, n)
      pending = pending.subarray(n)
      length -= n
      yield bytes
    }
  }
  async function bytes(length: number) {
    const out = Buffer.allocUnsafe(length)
    let n = 0
    for await (const b of take(length)) {
      out.set(b, n)
      n += b.length
    }
    return out
  }
  const uint = async () => (await bytes(4)).readUInt32LE(0)
  const decoder = new TextDecoder('utf-8', { fatal: true })
  let author = ''
  if (smallRender) {
    const length = await uint()
    if (length > 256 * 1024) invalid('compensation author byte limit')
    author = new TextDecoder('utf-8', { fatal: true, ignoreBOM: true }).decode(await bytes(length))
  }
  let historyLimit = 32 * 1024 * 1024
  let historyBytes = ',"changes":[]'.length
  async function json(max: number) {
    const length = await uint()
    if (!length || length > max || (historyBytes += length) > historyLimit)
      invalid('streamed history byte limit')
    const value = JSON.parse(decoder.decode(await bytes(length)))
    checkJsonBudget(value)
    return value
  }
  const header: Omit<HistoryRequest, 'changes'> = await json(8192)
  const headerBytes = historyBytes - ',"changes":[]'.length
  if (
    !header ||
    typeof header !== 'object' ||
    Object.keys(header).some((k) => !['epoch', 'version', 'format', 'baseHash', 'undo'].includes(k))
  )
    invalid('invalid history header')
  checkHistoryHeader(header, smallRender ? header.format : { name: 'pdf', schema: 2 })
  if (smallRender && !!header.undo !== !!author) invalid('incorrect compensation author frame')
  const name = header.format.name
  if (smallRender && !['docx', 'xlsx', 'pptx', 'pdf'].includes(name)) unsupported('document format')
  if (name === 'pdf' && header.format.schema !== 2) unsupported('PDF schema')
  if (header.version > 65536) unsupported('streamed history version limit')
  const length = await uint()
  if (!length || length > 50 * 1024 * 1024) invalid('source byte limit')
  if (name !== 'pdf' && length > 2 * 1024 * 1024) unsupported('office source exceeds 2 MiB')
  const hash = createHash('sha256')
  async function* base() {
    for await (const b of take(length)) {
      hash.update(b)
      yield b
    }
  }
  function authenticatedHash() {
    const digest = hash.digest('hex')
    if (digest !== header.baseHash) invalid('incorrect source hash')
    return digest
  }
  function checkAppend(ops: readonly unknown[]) {
    const v = header.version + 1
    const next = { v, client: 'history:' + '0'.repeat(36), seq: 1, author, ops }
    const frame = Buffer.byteLength(JSON.stringify(next))
    const prefix = { ...header, version: v }
    delete prefix.undo
    const headerDelta = smallRender
      ? Buffer.byteLength(JSON.stringify(prefix)) - headerBytes
      : String(v).length - String(header.version).length
    if (frame > 256 * 1024 || historyBytes + headerDelta + 1 + frame > historyLimit)
      unsupported('compensated prefix exceeds replay limits')
    checkJsonBudget(next)
  }
  let document
  let baseHash: string
  if (smallRender && (name !== 'pdf' || length <= 1024 * 1024)) {
    const source = await bytes(length)
    hash.update(source)
    baseHash = authenticatedHash()
    if (name === 'pdf') {
      async function* buffered() {
        yield source
      }
      try {
        document = await openStreamPdf(buffered(), length)
      } catch (error) {
        if (!(error instanceof HistoryError && error.code === 'unsupported')) throw error
      }
    }
    if (!document) {
      historyLimit = 1024 * 1024
      if (header.version > 128) invalid('small history version limit')
      const changes: HistoryRequest['changes'] = []
      for (let v = 1; v <= header.version; v++) {
        if (v !== 1) historyBytes++
        changes.push(await json(256 * 1024))
      }
      if (await fill()) invalid('extra bytes after requested prefix')
      const file = await smallRender(source, { ...header, changes }, author)
      if (file.compensation) checkAppend(file.compensation)
      return {
        fileName: `history.${name}`,
        baseHash,
        parts:
          file.bytes === source
            ? [{ offset: 0, length }]
            : [
                {
                  data: Buffer.from(
                    file.bytes.buffer,
                    file.bytes.byteOffset,
                    file.bytes.byteLength,
                  ).toString('base64'),
                },
              ],
        compensation: file.compensation,
      }
    }
  } else {
    const chunks = base()
    try {
      document = await openStreamPdf(chunks, length)
    } catch (error) {
      if (smallRender && error instanceof HistoryError && error.code === 'unsupported') {
        // The parser may reject the syntax before it has requested the whole source.
        while (!(await chunks.next()).done) continue
        authenticatedHash()
      }
      throw error
    }
    baseHash = authenticatedHash()
  }
  let state = emptySnapshot()
  const seen = new Map<string, Set<number>>()
  let captured: ((later: readonly PdfOp[]) => Put[] | null) | undefined
  let conflict = false
  let opCount = 0
  for (let v = 1; v <= header.version; v++) {
    if (v !== 1) historyBytes++
    const change: HistoryRequest['changes'][number] = await json(256 * 1024)
    checkHistoryChange(change, v, seen, header.undo)
    if (seen.size > 256 || change.ops.length > 256 || (opCount += change.ops.length) > 131072)
      unsupported('streamed history work limit')
    for (const [index, wire] of change.ops.entries()) {
      const op = decodeOp(wire)
      for (const p of op.puts) {
        if (p.b !== 'rotations' && p.b !== 'metadata')
          unsupported('streamed PDF supports metadata and rotations')
        if (p.b === 'rotations' && p.k >= document.pageCount)
          invalid('rotation outside the retained PDF')
        if (
          p.b === 'metadata' &&
          p.v &&
          Object.values(p.v).some(
            (s) => typeof s !== 'string' || s.length > 1023 || /[\uD800-\uDFFF]/u.test(s),
          )
        )
          unsupported('PDF metadata text')
      }
      if (v === header.undo?.v && index === header.undo.index) {
        captured = capturePdfInverse(state, op, document.pageCount)
      } else if (captured && !conflict) {
        // PDF inverse conflicts are a predicate on each later put, not a position mapping.
        conflict = !captured([op])
      }
      state = foldPeer(state, op.puts, document.pageCount)
    }
  }
  if (await fill()) invalid('extra bytes after requested prefix')
  if (conflict) throw new HistoryError('conflict')
  const inverse = captured?.([])
  const compensation = inverse ? [{ id: crypto.randomUUID(), puts: inverse }] : undefined
  if (inverse && compensation) {
    if (header.version >= 65536 || seen.size >= 256 || opCount + compensation.length > 131072)
      unsupported('compensated prefix exceeds replay limits')
    checkAppend(compensation)
    state = foldPeer(state, inverse, document.pageCount)
  }
  return { fileName: 'history.pdf', baseHash, parts: await document.produce(state), compensation }
}
