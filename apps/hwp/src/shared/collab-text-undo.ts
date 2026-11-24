import type { HwpDocument } from '@rhwp/core'
import { HistoryError } from '@seuoffice/collab/client'
import { mapHwpCharShapeIds } from './collab-command'
import { applyHwpOp, textOp, type HwpOp } from './collab-editor-ops'
import {
  mapHwpPosition,
  parseHwpPosition,
  rebaseHwpText,
  sameHwpContainer,
  type HwpPosition,
  type HwpTextSplice,
} from './collab-text'
import { hwpRichTextEffect, writeHwpText } from './collab-text-native'
import { rebaseHwpOps } from './collab-transform'

// These displayed characters can represent authored controls rather than literal text.
const nonLiteral = /[\p{Cc}\p{Cs}\u00a0\u2007\u2028\u2029\ufffc]/u
type Runs = [number, number][] // relative scalar offset, native character shape ID
export type HwpTextFragment = { kind: 'restoreText'; at: HwpPosition; text: string; runs: Runs }

interface UndoDocument extends HwpDocument {
  forkCollabDocument(): UndoDocument
  getCollabFingerprint(): string
  getCollabLiteralFragment(s: number, p: number, path: string, start: number, end: number): string
  getCollabShapeWitness(ids: Uint32Array): string
}

function native(doc: HwpDocument): UndoDocument {
  for (const method of [
    'forkCollabDocument',
    'getCollabFingerprint',
    'getCollabLiteralFragment',
    'getCollabShapeWitness',
  ] as const) {
    if (typeof (doc as Partial<UndoDocument>)[method] !== 'function')
      throw new HistoryError('unsupported', 'native text undo proof unavailable')
  }
  return doc as UndoDocument
}

function validRuns(runs: unknown, length: number): runs is Runs {
  if (!Array.isArray(runs) || runs.length > 4096 || (runs.length === 0) !== (length === 0))
    return false
  let previous = -1
  for (const row of runs) {
    if (!Array.isArray(row) || row.length !== 2) return false
    const [start, id] = row
    if (
      !Number.isInteger(start) ||
      start <= previous ||
      start >= length ||
      (previous === -1 && start !== 0) ||
      !Number.isInteger(id) ||
      id < 0 ||
      id > 0xffffffff
    )
      return false
    previous = start
  }
  return true
}

export function parseHwpTextFragment(value: unknown): HwpTextFragment | null {
  if (
    !value ||
    typeof value !== 'object' ||
    Array.isArray(value) ||
    Object.keys(value).length !== 4 ||
    !['kind', 'at', 'text', 'runs'].every((key) => Object.hasOwn(value, key))
  )
    return null
  const { kind, at: address, text, runs } = value as Record<string, unknown>
  const at = parseHwpPosition(address)
  if (
    kind !== 'restoreText' ||
    !at ||
    typeof text !== 'string' ||
    !text.length ||
    text.length > 256 * 1024 ||
    nonLiteral.test(text)
  )
    return null
  const length = Array.from(text).length
  return at.offset + length <= 0x7fffffff && validRuns(runs, length)
    ? { kind, at, text, runs }
    : null
}

function fragment(
  doc: UndoDocument,
  at: HwpPosition,
  end: number,
): { text: string; runs: Runs; shapeCount: number } {
  try {
    const f = JSON.parse(
      doc.getCollabLiteralFragment(
        at.section,
        at.paragraph,
        JSON.stringify(at.path),
        at.offset,
        end,
      ),
    )
    if (
      typeof f?.text === 'string' &&
      !nonLiteral.test(f.text) &&
      f.text.length <= 256 * 1024 &&
      Array.from(f.text).length === end - at.offset &&
      validRuns(f.runs, end - at.offset) &&
      Number.isSafeInteger(f.shapeCount) &&
      f.shapeCount >= 0
    )
      return f
  } catch {
    /* Refuse an unresolved cell, field, or range with unrepresented provenance. */
  }
  throw new HistoryError('unsupported', 'range has no literal native fragment')
}

export function applyHwpTextFragment(doc: HwpDocument, op: HwpTextFragment): void {
  const { at, text, runs } = op
  const length = Array.from(text).length
  const count = fragment(native(doc), at, at.offset).shapeCount
  if (!validRuns(runs, length) || runs.some(([, id]) => id >= count))
    throw new HistoryError('invalid_history', 'invalid fragment shape')
  const end = at.offset + length
  const absolute = JSON.stringify(
    runs.map(([start, id], i) => ({
      startOffset: at.offset + start,
      endOffset: at.offset + (runs[i + 1]?.[0] ?? length),
      charShapeId: id,
    })),
  )
  if (JSON.parse(writeHwpText(doc, at, text))?.ok !== true)
    throw new HistoryError('invalid_history', 'fragment insertion failed')
  const result = at.path.length
    ? doc.setCharShapeRunsInCellByPath(
        at.section,
        at.paragraph,
        JSON.stringify(at.path),
        at.offset,
        end,
        absolute,
      )
    : doc.setCharShapeRuns(at.section, at.paragraph, at.offset, end, absolute)
  if (JSON.parse(result)?.ok !== true)
    throw new HistoryError('invalid_history', 'fragment style restoration failed')
}

const retained = new WeakMap<HwpDocument, UndoDocument>()

/** Dispose an abandoned capture if the rest of its retained history cannot be replayed. */
export function releaseHwpUndo(doc: HwpDocument): void {
  const saved = retained.get(doc)
  if (saved) {
    retained.delete(doc)
    saved.free()
  }
}

function paragraph(at: HwpPosition): number {
  return at.path.at(-1)?.cellParaIndex ?? at.paragraph
}

function literalTarget(doc: UndoDocument, target: HwpOp): HwpTextSplice {
  let op: HwpTextSplice | null = null
  if (textOp(target)) op = target
  else if (target.kind === 'restoreText')
    op = { kind: 'splice', from: target.at, to: target.at, text: target.text }
  else if (
    target.kind === 'nativeText' &&
    !('tailSlots' in target) &&
    !('field' in target && target.field !== null) &&
    !('afterInline' in target && target.afterInline) &&
    (!('text' in target) || !nonLiteral.test(target.text))
  ) {
    op = hwpRichTextEffect(doc, target)
  }
  if (
    !op ||
    op.kind !== 'splice' ||
    nonLiteral.test(op.text) ||
    !sameHwpContainer(op.from, op.to) ||
    paragraph(op.from) !== paragraph(op.to)
  ) {
    throw new HistoryError('unsupported', 'only a literal single-paragraph edit has a text inverse')
  }
  return op
}

// A formatted range or memo on the selected text cannot silently become a smaller or empty range.
function span(op: HwpOp): number | undefined {
  return op.kind === 'insertMemo' || (op.kind === 'command' && op.action === 'characters')
    ? op.end - op.at.offset
    : undefined
}

function matchesAfter(doc: UndoDocument, ops: readonly HwpOp[], expected: string): boolean {
  const trial = doc.forkCollabDocument()
  try {
    for (const op of ops) applyHwpOp(trial, op)
    return trial.getCollabFingerprint() === expected
  } catch {
    return false
  } finally {
    trial.free()
  }
}

function withShapes(ops: readonly HwpOp[], runs: Runs): HwpOp[] {
  return ops.flatMap((op): HwpOp[] => {
    if (op.kind !== 'splice' || !op.text) return [op]
    const insert: HwpTextFragment = { kind: 'restoreText', at: op.from, text: op.text, runs }
    return op.from.offset === op.to.offset && paragraph(op.from) === paragraph(op.to)
      ? [insert]
      : [{ ...op, text: '' }, insert]
  })
}

/** The caller has already admitted the original file. A trial model is never installed over the head. */
export function captureHwpTextUndo(
  doc: HwpDocument,
  target: HwpOp,
): (later: readonly HwpOp[]) => HwpOp[] | null {
  releaseHwpUndo(doc)
  const original = native(doc)
  const { from, to, text } = literalTarget(original, target)
  const removed = fragment(original, from, to.offset)
  const ids = [...new Set(removed.runs.map(([, id]) => id))]
  const witness = original.getCollabShapeWitness(Uint32Array.from(ids))
  const before = original.getCollabFingerprint()
  const end = { ...from, offset: from.offset + Array.from(text).length }
  const splice: HwpTextSplice = { kind: 'splice', from, to: end, text: removed.text }
  const without = original.forkCollabDocument()
  retained.set(doc, without)
  return (later) => {
    if (retained.get(doc) !== without) return null
    let after: UndoDocument | undefined
    try {
      if (later.length > 256)
        throw new HistoryError('unsupported', 'undo transform exceeds the retained operation bound')
      if (original.getCollabShapeWitness(Uint32Array.from(ids)) !== witness) return null
      for (let i = 0; i < ids.length; i += 512) {
        const source = ids.slice(i, i + 512)
        const mapped = mapHwpCharShapeIds(without, original, source)
        if (!mapped || mapped.some((id, j) => id !== source[j])) return null
      }
      if (without.getCollabFingerprint() !== before)
        throw new HistoryError('unsupported', 'native fork changed the source model')
      after = without.forkCollabDocument()
      applyHwpOp(after, target)
      let inverse: HwpOp[] = [splice]
      const needsShapes = !matchesAfter(after, inverse, before)
      if (needsShapes) {
        inverse = withShapes(inverse, removed.runs)
        if (!removed.text || !matchesAfter(after, inverse, before))
          throw new HistoryError(
            'unsupported',
            'native fragment does not restore the original model',
          )
      }
      let planned: HwpOp[], peers: HwpOp[]
      const literal =
        later.every(textOp) &&
        later.every((op) => paragraph(op.from) === paragraph(op.to) && !op.text.includes('\n'))
      let needsRelocations = false
      if (literal && removed.text) {
        let gap: HwpPosition = from
        for (const op of later) {
          if (
            sameHwpContainer(gap, op.from) &&
            paragraph(gap) === paragraph(op.from) &&
            op.from.offset < gap.offset &&
            gap.offset < op.to.offset
          )
            return null
          const next = mapHwpPosition(gap, [op])
          if (!next) {
            needsRelocations = true
            break
          }
          gap = next
        }
      }
      const pair = literal && !needsRelocations ? rebaseHwpText(later, [splice]) : null
      if (pair) {
        peers = pair.confirmed
        planned = needsShapes ? withShapes(pair.pending, removed.runs) : pair.pending
        for (const op of later) applyHwpOp(after, op)
      } else {
        const mappingBase = after.forkCollabDocument()
        try {
          const forward = rebaseHwpOps(after, later, inverse)
          const counter = rebaseHwpOps(mappingBase, inverse, later)
          if (
            forward.deleted.length ||
            forward.retry.length ||
            counter.deleted.length ||
            counter.retry.length ||
            counter.replay.length !== later.length ||
            later.some(
              (op, i) =>
                op.kind !== counter.replay[i]!.kind || span(op) !== span(counter.replay[i]!),
            )
          )
            return null
          planned = forward.replay
          peers = counter.replay
        } finally {
          mappingBase.free()
        }
      }
      if (!planned.length || after.getCollabFingerprint() !== original.getCollabFingerprint())
        return null
      for (const op of peers) applyHwpOp(without, op)
      const expected = without.getCollabFingerprint()
      if (matchesAfter(after, planned, expected)) return planned
      if (needsShapes || !removed.text) return null
      const styled = withShapes(planned, removed.runs)
      return matchesAfter(after, styled, expected) ? styled : null
    } catch (error) {
      if (error instanceof HistoryError && error.code === 'unsupported') throw error
      return null
    } finally {
      after?.free()
      releaseHwpUndo(doc)
    }
  }
}
