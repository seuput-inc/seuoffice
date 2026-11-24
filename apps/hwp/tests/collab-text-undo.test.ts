import { readFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import { dirname, join } from 'node:path'
import init, { HwpDocument } from '@rhwp/core'
import { beforeAll, expect, it } from 'vitest'
import { applyHwpText } from '../src/shared/collab-text-model'
import { captureHwpTextUndo } from '../src/shared/collab-text-undo'
import type { HwpPosition, HwpTextSplice } from '../src/shared/collab-text'

const require = createRequire(import.meta.url)
beforeAll(async () => {
  await init({
    module_or_path: readFileSync(join(dirname(require.resolve('@rhwp/core')), 'rhwp_bg.wasm')),
  })
})

function body(text: string): HwpDocument {
  const doc = HwpDocument.createEmpty()
  doc.createBlankDocument()
  doc.splitParagraph(0, 0, doc.getParagraphLength(0, 0))
  doc.insertText(0, 1, 0, text)
  return doc
}

const pos: HwpPosition = { section: 0, paragraph: 1, path: [], offset: 0 }
function edit(from: number, to: number, text: string, p = pos): HwpTextSplice {
  return { kind: 'splice', from: { ...p, offset: from }, to: { ...p, offset: to }, text }
}

function applyCompensation(doc: HwpDocument, target: HwpTextSplice, later: HwpTextSplice[]) {
  const plan = captureHwpTextUndo(doc, target)
  applyHwpText(doc, target)
  for (const op of later) applyHwpText(doc, op)
  const inverse = plan(later)
  expect(inverse).not.toBeNull()
  for (const op of inverse!) applyHwpText(doc, op)
}

it('restores deleted body scalars at the new offset without erasing later text', () => {
  const doc = body('한🙂글')
  try {
    applyCompensation(doc, edit(1, 2, ''), [edit(0, 0, '먼저'), edit(4, 4, '!')])
    expect(doc.getTextRange(0, 1, 0, 100)).toBe('먼저한🙂글!')
  } finally {
    doc.free()
  }
})

it('undoes an insertion without deleting a later insertion or replacement inside it', () => {
  const doc = body('가나')
  try {
    applyCompensation(doc, edit(1, 1, 'A🙂B'), [edit(2, 2, '동료'), edit(4, 5, '𠮷')])
    expect(doc.getTextRange(0, 1, 0, 100)).toBe('가동료𠮷나')
  } finally {
    doc.free()
  }
})

it('restores deleted text with its authored shape inside a table nested in another cell', () => {
  const doc = body('본문')
  try {
    const outer = JSON.parse(doc.createTable(0, 1, 2, 1, 1))
    const parent = outer.paraIdx
    const outerPath = [{ controlIndex: outer.controlIdx, cellIndex: 0, cellParaIndex: 0 }]
    const inner = JSON.parse(
      doc.createTableInCellByPath(
        0,
        parent,
        JSON.stringify(outerPath),
        0,
        '{"rowCount":1,"colCount":1}',
      ),
    )
    expect(inner.cellPath).toHaveLength(2)
    const p = { ...pos, paragraph: parent, path: inner.cellPath }
    const path = JSON.stringify(p.path)
    doc.insertTextInCellByPath(0, parent, path, 0, '진한🙂글')
    doc.applyCharFormatInCellByPath(0, parent, path, 0, 4, '{"bold":true,"fontSize":1800}')
    const originalRuns = JSON.parse(doc.getCharShapeRunsInCellByPath(0, parent, path, 0, 4))
    expect(originalRuns).toHaveLength(1)
    expect(JSON.parse(doc.getCellCharPropertiesAtByPath(0, parent, path, 2)).bold).toBe(true)
    applyCompensation(doc, edit(1, 3, '', p), [edit(1, 1, '동료', p)])
    expect(doc.getTextInCellByPath(0, parent, path, 0, 100)).toBe('진동료한🙂글')
    expect(JSON.parse(doc.getCharShapeRunsInCellByPath(0, parent, path, 0, 6))).toEqual([
      { ...originalRuns[0], endOffset: 6 },
    ])
    expect(doc.getTextRange(0, 1, 0, 2)).toBe('본문')
  } finally {
    doc.free()
  }
})

it('refuses a mixed-shape deletion, a paragraph merge, and a field instead of restoring plain text', () => {
  const doc = body('abcd')
  try {
    doc.applyCharFormat(0, 1, 1, 2, '{"bold":true}')
    expect(() => captureHwpTextUndo(doc, edit(0, 3, ''))).toThrow(
      expect.objectContaining({ code: 'unsupported' }),
    )
    const merge = { kind: 'splice', from: pos, to: { ...pos, paragraph: 2 }, text: '' } as const
    expect(() => captureHwpTextUndo(doc, merge)).toThrow(
      expect.objectContaining({ code: 'unsupported' }),
    )
    doc.splitParagraph(0, 1, 4)
    doc.insertClickHereField(0, 2, 0, '입력', '', '', true)
    expect(() => captureHwpTextUndo(doc, edit(0, 1, '', { ...pos, paragraph: 2 }))).toThrow(
      expect.objectContaining({ code: 'unsupported' }),
    )
  } finally {
    doc.free()
  }
})

it('fails closed when the loaded engine does not expose range certification', () => {
  const doc = body('abcd')
  try {
    Object.defineProperty(doc, 'getCollabTextUndoContext', { value: undefined })
    expect(() => captureHwpTextUndo(doc, edit(1, 2, ''))).toThrow(
      expect.objectContaining({ code: 'unsupported' }),
    )
    expect(doc.getTextRange(0, 1, 0, 4)).toBe('abcd')
  } finally {
    doc.free()
  }
})

it('returns no compensation when the head does not match replay, and refuses a now-mixed shape', () => {
  const doc = body('abcd')
  try {
    const erase = edit(1, 2, '')
    const plan = captureHwpTextUndo(doc, erase)
    applyHwpText(doc, erase)
    const extra = edit(3, 3, '!')
    applyHwpText(doc, extra)
    expect(plan([])).toBeNull()
    doc.applyCharFormat(0, 1, 1, 2, '{"italic":true}')
    expect(() => plan([extra])).toThrow(expect.objectContaining({ code: 'unsupported' }))
    expect(doc.getTextRange(0, 1, 0, 100)).toBe('acd!')
  } finally {
    doc.free()
  }
})
