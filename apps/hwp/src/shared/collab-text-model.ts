import type { HwpDocument } from '@rhwp/core'
import type { HwpPosition, HwpTextSplice } from './collab-text'

function checked(result: string) {
  const reply = JSON.parse(result) as { ok: boolean; charOffset: number }
  if (reply.ok !== true) throw new Error('HWP rejected the text operation')
  return reply
}

function checkOffset(doc: HwpDocument, at: HwpPosition) {
  const length = at.path.length
    ? doc.getCellParagraphLengthByPath(at.section, at.paragraph, JSON.stringify(at.path))
    : doc.getParagraphLength(at.section, at.paragraph)
  if (at.offset > length) throw new RangeError('HWP text offset is outside its paragraph')
}

/** Apply one decoded splice. The caller owns the transaction if this is part of a larger edit. */
export function applyHwpText(doc: HwpDocument, op: HwpTextSplice): void {
  const { from, to } = op
  checkOffset(doc, from)
  checkOffset(doc, to)
  const { section, paragraph, path } = from
  const cell = path.at(-1)
  const first = cell?.cellParaIndex ?? paragraph
  const end = to.path.at(-1)?.cellParaIndex ?? to.paragraph
  const cellPath = cell ? path.map((entry) => ({ ...entry })) : undefined
  let target = cellPath ? JSON.stringify(cellPath) : undefined
  if (first !== end || from.offset !== to.offset) {
    checked(
      target === undefined
        ? doc.deleteRange(section, first, from.offset, end, to.offset)
        : doc.deleteRangeInCellByPath(
            section,
            paragraph,
            target,
            first,
            from.offset,
            end,
            to.offset,
          ),
    )
  }
  const lines = op.kind === 'lineBreak' ? [op.text] : op.text.split('\n')
  let offset = from.offset
  for (const [i, line] of lines.entries()) {
    if (i > 0) {
      checked(
        target === undefined
          ? doc.splitParagraph(
              section,
              first + i - 1,
              doc.textToLogicalOffset(section, first + i - 1, offset),
            )
          : doc.splitParagraphInCellByPath(section, paragraph, target, offset),
      )
      if (cellPath) {
        cellPath[cellPath.length - 1]!.cellParaIndex = first + i
        target = JSON.stringify(cellPath)
      }
      offset = 0
    }
    if (!line) continue
    offset = checked(
      target === undefined
        ? doc.insertText(section, first + i, offset, line)
        : doc.insertTextInCellByPath(section, paragraph, target, offset, line),
    ).charOffset
  }
}
