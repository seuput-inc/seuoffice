import type { HwpDocument } from '@rhwp/core'
import { HistoryError } from '@seuoffice/collab/client'
import {
  mapHwpPosition,
  rebaseHwpText,
  sameHwpContainer,
  type HwpPosition,
  type HwpTextSplice,
} from './collab-text'

// Tabs and control-backed spaces can carry authored data not represented by the displayed scalar.
const nonLiteral = /[\u0000-\u001f\u007f-\u009f\u00a0\u2007\u2028\u2029\ufffc]/u
const maxUnits = 256 * 1024

type UndoDocument = HwpDocument & {
  getCollabTextUndoContext?(section: number, paragraph: number, pathJson: string): string
}

function certified(doc: UndoDocument, at: HwpPosition): { text: string; charShapeId: number } {
  if (typeof doc.getCollabTextUndoContext !== 'function') throw new HistoryError('unsupported')
  try {
    const result = JSON.parse(
      doc.getCollabTextUndoContext(at.section, at.paragraph, JSON.stringify(at.path)),
    )
    if (
      result?.ok === true &&
      typeof result.text === 'string' &&
      result.text.length <= maxUnits &&
      !nonLiteral.test(result.text) &&
      Number.isInteger(result.charShapeId) &&
      result.charShapeId >= 0 &&
      result.charShapeId <= 0xffffffff
    ) {
      return { text: result.text, charShapeId: result.charShapeId }
    }
  } catch {
    // Unresolved control paths and engines that cannot certify the paragraph are not plain text.
  }
  throw new HistoryError('unsupported')
}

function paragraph(at: HwpPosition): number {
  return at.path.at(-1)?.cellParaIndex ?? at.paragraph
}

/** Capture immediately before application; plan against the same detached document after `later` has been replayed. */
export function captureHwpTextUndo(
  doc: UndoDocument,
  target: HwpTextSplice,
): (later: readonly HwpTextSplice[]) => HwpTextSplice[] | null {
  const { from, to, text } = target
  if (
    target.kind !== 'splice' ||
    !sameHwpContainer(from, to) ||
    paragraph(from) !== paragraph(to) ||
    nonLiteral.test(text)
  ) {
    throw new HistoryError('unsupported')
  }
  const before = certified(doc, from)
  const chars = Array.from(before.text)
  if (to.offset > chars.length || from.offset > to.offset) throw new HistoryError('invalid_history')
  const removed = chars.slice(from.offset, to.offset).join('')
  const inserted = Array.from(text)
  const after = chars.slice(0, from.offset).concat(inserted, chars.slice(to.offset))
  const inverse: HwpTextSplice = {
    kind: 'splice',
    from,
    to: { ...from, offset: from.offset + inserted.length },
    text: removed,
  }
  return (later) => {
    let expected = after
    let host = { ...from, offset: 0 }
    let undo: HwpTextSplice[] = [inverse]
    // A replay request can contain many small writes to the same long paragraph.
    let scalarWork = 16 * 1024 * 1024
    for (const op of later) {
      const p = paragraph(host)
      if (sameHwpContainer(host, op.from) && paragraph(op.from) <= p && p <= paragraph(op.to)) {
        // The certificate covers neither joining another paragraph nor the metadata of a new one.
        if (
          paragraph(op.from) !== paragraph(op.to) ||
          op.kind !== 'splice' ||
          nonLiteral.test(op.text)
        ) {
          throw new HistoryError('unsupported')
        }
        if (op.to.offset > expected.length) return null
        scalarWork -= expected.length + op.text.length + removed.length
        if (scalarWork < 0) throw new HistoryError('unsupported')
        expected = expected
          .slice(0, op.from.offset)
          .concat(Array.from(op.text), expected.slice(op.to.offset))
        if (expected.length > maxUnits) throw new HistoryError('unsupported')
      } else {
        const mapped = mapHwpPosition(host, [op], 'before')
        if (!mapped) return null
        if (mapped === host) continue
        scalarWork -= removed.length + op.text.length
        if (scalarWork < 0) throw new HistoryError('unsupported')
        host = mapped
      }
      const pair = rebaseHwpText([op], undo)
      if (!pair?.pending.length) return null
      undo = pair.pending
    }
    const head = certified(doc, host)
    // Compare the entire certified paragraph, not just the removed span. A stale plan cannot bind to a matching substring.
    if (head.text !== expected.join('') || head.charShapeId !== before.charShapeId) return null
    return undo
  }
}
