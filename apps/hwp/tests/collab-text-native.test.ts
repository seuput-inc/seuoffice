import { readFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import { dirname, join } from 'node:path'
import init, { HwpDocument } from '@rhwp/core'
import { beforeAll, expect, it } from 'vitest'
import { applyHwpText } from '../src/shared/collab-text-model'
import {
  applyHwpRichText,
  captureHwpCaret,
  captureHwpText,
  hwpRichTextEffect,
  mapHwpRichText,
  parseHwpRichText,
  remapHwpRichText,
  restoreHwpCaret,
  richTextFragment,
  type HwpRichTextOp,
} from '../src/shared/collab-text-native'
import type { HwpTextSplice } from '../src/shared/collab-text'

const require = createRequire(import.meta.url)
beforeAll(async () => {
  await init({
    module_or_path: readFileSync(join(dirname(require.resolve('@rhwp/core')), 'rhwp_bg.wasm')),
  })
})

function body(text: string) {
  const doc = HwpDocument.createEmpty()
  doc.createBlankDocument()
  doc.splitParagraph(0, 0, doc.getParagraphLength(0, 0))
  if (text) doc.insertText(0, 1, 0, text)
  return doc
}

function replay(doc: HwpDocument, op: HwpTextSplice | HwpRichTextOp | null) {
  if (!op) throw new Error('Text call was not captured')
  if (op.kind !== 'nativeText') return applyHwpText(doc, op)
  const decoded = parseHwpRichText(JSON.parse(JSON.stringify(op)))
  if (!decoded) throw new Error('Native text call did not survive the wire')
  applyHwpRichText(doc, decoded)
}

it.each([true, false])('uses the sender field at a shared end boundary (active=%s)', (active) => {
  const seed = body('앞뒤')
  seed.insertClickHereField(0, 1, 1, '이름', '', 'name', true)
  seed.setActiveField(0, 1, 1)
  seed.insertText(0, 1, 1, '기')
  const bytes = seed.exportHwpx()
  seed.free()
  const sender = new HwpDocument(bytes),
    receiver = new HwpDocument(bytes)
  try {
    const id = JSON.parse(sender.getFieldList())[0].fieldId
    const args = [0, 1, 2, 0, '한🙂'] as const
    sender.clearActiveField()
    receiver.clearActiveField()
    if (active) sender.setActiveField(0, 1, 2)
    else receiver.setActiveField(0, 1, 2)
    const op = captureHwpText(sender, 'replaceBodyTextLocal', args)
    expect(op).toMatchObject({
      kind: 'nativeText',
      action: 'bodyLocal',
      field: active ? expect.any(Number) : null,
    })
    sender.replaceBodyTextLocal(...args)
    replay(receiver, op)
    expect(receiver.getTextRange(0, 1, 0, 100)).toBe(sender.getTextRange(0, 1, 0, 100))
    expect(receiver.getFieldValue(id)).toBe(sender.getFieldValue(id))
    expect(JSON.parse(receiver.getFieldValue(id)).value).toBe(active ? '기한🙂' : '기')
    expect(captureHwpText(receiver, 'insertText', [0, 1, 1, '!'])).toMatchObject({
      field: active ? null : expect.any(Number),
    })
  } finally {
    sender.free()
    receiver.free()
  }
})

it('replays an atomic replacement over an entire memo anchor', () => {
  const seed = body('한🙂글')
  seed.insertMemo(0, 1, 1, 2, JSON.stringify({ author: '작성자', text: '메모' }))
  const bytes = seed.exportHwpx()
  seed.free()
  const sender = new HwpDocument(bytes),
    receiver = new HwpDocument(bytes)
  try {
    const args = [0, 1, 1, 1, '동료'] as const
    const op = captureHwpText(sender, 'replaceBodyTextLocal', args)
    expect(op).toMatchObject({ kind: 'nativeText', action: 'bodyLocal', count: 1 })
    sender.replaceBodyTextLocal(...args)
    replay(receiver, op)
    expect(JSON.parse(receiver.getMemoList())).toMatchObject([
      { startCharIdx: 1, endCharIdx: 3, text: '메모' },
    ])
    expect(receiver.getMemoList()).toBe(sender.getMemoList())
    expect(receiver.getTextRange(0, 1, 0, 100)).toBe('한동료글')
  } finally {
    sender.free()
    receiver.free()
  }
})

it('keeps text on the same side of an inline footnote, including when splitting behind it', () => {
  const seed = body('앞뒤')
  seed.insertFootnote(0, 1, 1)
  const bytes = seed.exportHwpx()
  seed.free()
  const sender = new HwpDocument(bytes),
    receiver = new HwpDocument(bytes)
  try {
    const args = [0, 1, 2, '🙂한'] as const
    const insert = captureHwpText(sender, 'insertTextLogical', args)
    expect(insert).toMatchObject({ action: 'insert', at: { offset: 1 }, afterInline: true })
    sender.insertTextLogical(...args)
    replay(receiver, insert)
    expect(receiver.getControlTextPositions(0, 1)).toBe(sender.getControlTextPositions(0, 1))
    const split = captureHwpText(sender, 'splitParagraph', [0, 1, 2, undefined])
    expect(split).toMatchObject({ action: 'split', at: { offset: 1 }, logical: 2, meta: null })
    sender.splitParagraph(0, 1, 2)
    replay(receiver, split)
    expect(receiver.getLogicalLength(0, 1)).toBe(2)
    expect(receiver.getTextRange(0, 2, 0, 100)).toBe('🙂한뒤')
    expect(receiver.getControlTextPositions(0, 1)).toBe(sender.getControlTextPositions(0, 1))
  } finally {
    sender.free()
    receiver.free()
  }
})

const nested = [
  { controlIndex: 0, cellIndex: 9, cellParaIndex: 3 },
  { controlIndex: 0, cellIndex: 1, cellParaIndex: 0 },
]
it('keeps the full nested-cell field address instead of activating a visible text range', () => {
  const input = readFileSync(join(__dirname, 'fixtures/nested-form.hwp'))
  const path = JSON.stringify(nested)
  const sender = new HwpDocument(input),
    receiver = new HwpDocument(input)
  try {
    for (const doc of [sender, receiver])
      doc.insertClickHereFieldByPath(0, 2, path, 1, '성명', '', 'nestedAuthor', true)
    receiver.clearActiveField()
    sender.setActiveFieldByPath(0, 2, path, 1)
    const args = [0, 2, path, 1, '조한글'] as const
    const op = captureHwpText(sender, 'insertTextInCellByPath', args)
    expect(op).toMatchObject({ action: 'insert', at: { path: nested }, field: expect.any(Number) })
    sender.insertTextInCellByPath(...args)
    replay(receiver, op)
    expect(JSON.parse(receiver.getFieldValueByName('nestedAuthor')).value).toBe('조한글')
    expect(receiver.getTextInCellByPath(0, 2, path, 0, 100)).toBe(
      sender.getTextInCellByPath(0, 2, path, 0, 100),
    )
    const split = captureHwpText(receiver, 'splitParagraphInCellByPath', [0, 2, path, 1])
    replay(receiver, split)
    const second = JSON.stringify([nested[0], { ...nested[1], cellParaIndex: 1 }])
    replay(receiver, captureHwpText(receiver, 'mergeParagraphInCellByPath', [0, 2, second]))
    expect(receiver.getTextInCellByPath(0, 2, path, 0, 100)).toBe(
      sender.getTextInCellByPath(0, 2, path, 0, 100),
    )
  } finally {
    sender.free()
    receiver.free()
  }
})

it('captures deferred flat-cell IME operands and treats embedded LF as text, not Enter', () => {
  const doc = body('본문')
  try {
    const { paraIdx, controlIdx } = JSON.parse(doc.createTable(0, 1, 2, 1, 1))
    const here = [0, paraIdx, controlIdx, 0, 0] as const
    replay(doc, captureHwpText(doc, 'insertTextInCellDeferredPagination', [...here, 0, '가🙂']))
    const op = captureHwpText(doc, 'replaceTextInCellDeferredPagination', [...here, 1, 1, '각'])
    expect(op).toMatchObject({ action: 'cellReplace', at: { offset: 1 }, text: '각', count: 1 })
    replay(doc, op)
    replay(doc, captureHwpText(doc, 'deleteTextInCellDeferredPagination', [...here, 0, 1]))
    const literal = captureHwpText(doc, 'insertTextInCell', [...here, 1, '한\n글'])
    expect(literal).toMatchObject({ action: 'insert', text: '한\n글' })
    replay(doc, literal)
    const path = JSON.stringify([{ controlIndex: controlIdx, cellIndex: 0, cellParaIndex: 0 }])
    expect(doc.getTextInCellByPath(0, paraIdx, path, 0, 100)).toBe('각한\n글')
  } finally {
    doc.free()
  }
})

it('accepts the exact metadata returned by merge and rejects extra methods, metadata and paths', () => {
  const doc = body('문단')
  try {
    doc.splitParagraph(0, 1, 1)
    const restored = JSON.parse(doc.mergeParagraph(0, 2)).removedParaMeta
    const op = captureHwpText(doc, 'splitParagraph', [0, 1, 1, JSON.stringify(restored)])
    expect(op).toMatchObject({ kind: 'nativeText', action: 'split', meta: restored })
    expect(parseHwpRichText({ ...op, action: 'exportHwp' })).toBeNull()
    expect(parseHwpRichText({ ...op, meta: { ...restored, other: 0 } })).toBeNull()
    expect(parseHwpRichText({ ...op, meta: { ...restored, raw_header_extra: [-1] } })).toBeNull()
    expect(parseHwpRichText({ ...op, meta: { ...restored, tab_extended: [[1]] } })).toBeNull()
    replay(doc, op)
    expect(JSON.parse(doc.mergeParagraph(0, 2)).removedParaMeta).toEqual(restored)
    expect(captureHwpText(doc, 'exportHwp', [])).toBeNull()
    expect(
      captureHwpText(doc, 'insertTextInCellByPath', [
        0,
        1,
        '[{"controlIndex":1,"controlIndex":2,"cellIndex":0,"cellParaIndex":0}]',
        0,
        'x',
      ]),
    ).toBeNull()
  } finally {
    doc.free()
  }
})

it('moves an atomic Korean replacement past a peer insertion but never splits it around new text', () => {
  const before = body('가🙂나다'),
    after = body('가🙂나다')
  try {
    const op = captureHwpText(before, 'replaceBodyTextLocal', [0, 1, 1, 2, '완성'])
    if (!op || op.kind !== 'nativeText') throw new Error('Expected an atomic replacement')
    expect(richTextFragment(op)).toBe('완성')
    const p = { section: 0, paragraph: 1, path: [], offset: 0 }
    const prior = { kind: 'splice', from: p, to: p, text: '먼🙂' } as const
    applyHwpText(after, prior)
    const moved = mapHwpRichText(op, before, after, [prior])
    expect(moved).toMatchObject({ action: 'bodyLocal', at: { offset: 3 }, count: 2 })
    const inside = { ...p, offset: 4 }
    const conflict = { kind: 'splice', from: inside, to: inside, text: '동료' } as const
    applyHwpText(after, conflict)
    expect(mapHwpRichText(op, before, after, [prior, conflict])).toBeNull()
  } finally {
    before.free()
    after.free()
  }
})

it('preserves a body caret slot and reports loss when the inline object is gone', () => {
  const withNote = body('앞뒤'),
    noNote = body('앞뒤')
  try {
    withNote.insertFootnote(0, 1, 1)
    const logical = { section: 0, paragraph: 1, path: [], offset: 2 }
    const caret = captureHwpCaret(withNote, logical)
    expect(caret).toEqual({ at: { ...logical, offset: 1 }, slot: 1 })
    expect(restoreHwpCaret(withNote, caret)).toEqual(logical)
    expect(restoreHwpCaret(noNote, caret)).toBeNull()
  } finally {
    withNote.free()
    noNote.free()
  }
})

it.each(['insertText', 'replaceBodyTextLocal'] as const)(
  'retains trailing native slots in %s rather than clamping to scalar end',
  (method) => {
    const sender = body('끝'),
      receiver = body('끝')
    try {
      for (const doc of [sender, receiver]) doc.insertFootnote(0, 1, 1)
      const op = captureHwpText(
        sender,
        method,
        method === 'insertText' ? [0, 1, 2, '한'] : [0, 1, 2, 0, '한'],
      )
      expect(op).toMatchObject({ kind: 'nativeText', at: { offset: 1 }, tailSlots: 1 })
      if (method === 'insertText') sender.insertText(0, 1, 2, '한')
      else sender.replaceBodyTextLocal(0, 1, 2, 0, '한')
      replay(receiver, op)
      expect(receiver.getControlTextPositions(0, 1)).toBe(sender.getControlTextPositions(0, 1))
      expect(receiver.getTextRange(0, 1, 0, 2)).toBe('끝한')
    } finally {
      sender.free()
      receiver.free()
    }
  },
)

it('rebases logical deletion of only a footnote without deleting its neighboring scalars', () => {
  const before = body('앞뒤'),
    after = body('앞뒤')
  try {
    for (const d of [before, after]) d.insertFootnote(0, 1, 1)
    const op = captureHwpText(before, 'deleteRangeLogical', [0, 1, 1, 1, 2])
    if (op?.kind !== 'nativeText') throw new Error('Expected the original object selection')
    const p = { section: 0, paragraph: 1, path: [], offset: 1 }
    const shape = hwpRichTextEffect(before, op)
    expect(shape).toEqual({ kind: 'splice', from: p, to: p, text: '' })
    if (!shape) throw new Error('Expected a positional effect')
    expect(
      remapHwpRichText(
        op,
        before,
        after,
        (pos) => (pos.path.length ? null : pos),
        shape,
        (at, control) => ({ at, control }),
      ),
    ).toEqual(op)
    expect(
      remapHwpRichText(
        op,
        before,
        after,
        (pos) => pos,
        shape,
        () => null,
      ),
    ).toBeNull()
    const start = { ...p, offset: 0 }
    const peer = { kind: 'splice', from: start, to: start, text: '🙂' } as const
    applyHwpText(after, peer)
    const moved = mapHwpRichText(op, before, after, [peer])
    expect(moved).toMatchObject({ action: 'logicalRange', logical: 2, endLogical: 3 })
    replay(after, moved)
    expect(after.getTextRange(0, 1, 0, 100)).toBe('🙂앞뒤')
    expect(after.getLogicalLength(0, 1)).toBe(3)
  } finally {
    before.free()
    after.free()
  }
})
