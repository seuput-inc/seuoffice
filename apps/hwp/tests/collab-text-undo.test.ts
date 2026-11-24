import { readFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import { dirname, join } from 'node:path'
import init, { HwpDocument } from '@rhwp/core'
import { beforeAll, expect, it } from 'vitest'
import { applyHwpOp, decodeHwpOp, encodeHwpOp, type HwpOp } from '../src/shared/collab-editor-ops'
import { applyHwpText } from '../src/shared/collab-text-model'
import {
  captureHwpTextUndo,
  parseHwpTextFragment,
  releaseHwpUndo,
} from '../src/shared/collab-text-undo'
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

function applyCompensation(doc: HwpDocument, target: HwpOp, later: HwpOp[]) {
  try {
    const plan = captureHwpTextUndo(doc, target)
    applyHwpOp(doc, target)
    for (const op of later) applyHwpOp(doc, op)
    const wire = plan(later)?.map(encodeHwpOp)
    expect(wire?.length).toBeGreaterThan(0)
    for (const op of wire!) applyHwpOp(doc, decodeHwpOp(JSON.parse(JSON.stringify(op))))
  } finally {
    releaseHwpUndo(doc)
  }
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

it('refuses a paragraph merge and a field instead of restoring visible text alone', () => {
  const doc = body('abcd')
  try {
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
    Object.defineProperty(doc, 'getCollabLiteralFragment', { value: undefined })
    expect(() => captureHwpTextUndo(doc, edit(1, 2, ''))).toThrow(
      expect.objectContaining({ code: 'unsupported' }),
    )
    expect(doc.getTextRange(0, 1, 0, 4)).toBe('abcd')
  } finally {
    doc.free()
  }
})

it('rejects an unlogged edit even if it only changes the formatting of the insertion being undone', () => {
  const doc = body('abcd')
  try {
    const target = edit(1, 1, '🙂')
    const plan = captureHwpTextUndo(doc, target)
    applyHwpText(doc, target)
    const extra = edit(0, 0, '동료')
    applyHwpText(doc, extra)
    doc.applyCharFormat(0, 1, 3, 4, '{"italic":true}')
    expect(plan([extra])).toBeNull()
    expect(doc.getTextRange(0, 1, 0, 100)).toBe('동료a🙂bcd')
  } finally {
    doc.free()
  }
})

const formats = ['hwp', 'hwpx'] as const
function save(doc: HwpDocument, format: (typeof formats)[number]) {
  return format === 'hwp' ? doc.exportHwp() : doc.exportHwpx()
}

it.each(formats)(
  'undoes typing in the first paragraph of a closed %s with an independent later insert',
  (format) => {
    const blank = readFileSync(join(__dirname, '..', 'src/renderer/assets/blank.hwp'))
    const seed = new HwpDocument(blank)
    const doc = new HwpDocument(format === 'hwp' ? blank : seed.exportHwpx())
    try {
      const first = { ...pos, paragraph: 0 }
      applyCompensation(doc, edit(0, 0, '나🙂', first), [edit(1, 1, '동료', first)])
      const reopened = new HwpDocument(save(doc, format))
      try {
        expect(reopened.getParagraphCount(0)).toBe(1)
        expect(reopened.getTextRange(0, 0, 0, 16)).toBe('동료')
        expect(JSON.parse(reopened.getPageDef(0))).toMatchObject({ width: 59528, height: 84186 })
      } finally {
        reopened.free()
      }
    } finally {
      seed.free()
      doc.free()
    }
  },
)

it.each(formats)(
  'removes only my insertion in the original Hancom nested cell after another %s edit',
  (format) => {
    const bytes = readFileSync(join(__dirname, 'fixtures/nested-form.hwp'))
    const seed = new HwpDocument(bytes)
    const doc = new HwpDocument(format === 'hwp' ? bytes : seed.exportHwpx())
    const p = {
      ...pos,
      paragraph: 2,
      path: [
        { controlIndex: 0, cellIndex: 9, cellParaIndex: 3 },
        { controlIndex: 0, cellIndex: 1, cellParaIndex: 0 },
      ],
    }
    try {
      const path = JSON.stringify(p.path)
      expect(doc.getTextInCellByPath(0, 2, path, 0, 16)).toBe('정기시험')
      applyCompensation(doc, edit(1, 1, 'A🙂B', p), [edit(2, 2, '동료', p)])
      const reopened = new HwpDocument(save(doc, format))
      try {
        expect(reopened.getTextInCellByPath(0, 2, path, 0, 16)).toBe('정동료기시험')
      } finally {
        reopened.free()
      }
    } finally {
      doc.free()
      seed.free()
    }
  },
)

it.each(formats)(
  'undoes a native deletion in a reopened %s cell with the exact mixed character runs',
  (format) => {
    const input = readFileSync(join(__dirname, 'fixtures/nested-form.hwp'))
    const p = {
      ...pos,
      paragraph: 2,
      path: [
        { controlIndex: 0, cellIndex: 9, cellParaIndex: 3 },
        { controlIndex: 0, cellIndex: 1, cellParaIndex: 0 },
      ],
    }
    const path = JSON.stringify(p.path)
    const seed = new HwpDocument(input)
    seed.applyCharFormatInCellByPath(0, 2, path, 1, 2, '{"bold":true,"textColor":"#aa3322"}')
    const doc = new HwpDocument(save(seed, format))
    seed.free()
    try {
      expect(doc.getTextInCellByPath(0, 2, path, 0, 4)).toBe('정기시험')
      const before = JSON.parse(doc.getCharShapeRunsInCellByPath(0, 2, path, 0, 4))
      expect(before.length).toBeGreaterThan(1)
      const peer = edit(2, 2, '!', p)
      applyCompensation(
        doc,
        { kind: 'nativeText', action: 'delete', at: { ...p, offset: 1 }, count: 2 },
        [peer],
      )
      const reopened = new HwpDocument(save(doc, format))
      try {
        expect(reopened.getTextInCellByPath(0, 2, path, 0, 16)).toBe('정기시험!')
        expect(JSON.parse(reopened.getCharShapeRunsInCellByPath(0, 2, path, 0, 4))).toEqual(before)
      } finally {
        reopened.free()
      }
    } finally {
      doc.free()
    }
  },
)

it('preserves a later character format; conflicts when the peer styled only the text being removed', () => {
  for (const overlap of [false, true]) {
    const doc = body('원문')
    try {
      const target = edit(1, 1, '나')
      const peer: HwpOp = {
        kind: 'command',
        action: 'characters',
        at: { ...pos, offset: overlap ? 1 : 0 },
        end: overlap ? 2 : 1,
        props: { bold: true },
      }
      const plan = captureHwpTextUndo(doc, target)
      applyHwpOp(doc, target)
      applyHwpOp(doc, peer)
      const inverse = plan([peer])
      if (overlap) expect(inverse).toBeNull()
      else {
        expect(inverse?.length).toBeGreaterThan(0)
        for (const op of inverse!) applyHwpOp(doc, op)
      }
      expect(doc.getTextRange(0, 1, 0, 8)).toBe(overlap ? '원나문' : '원문')
      expect(JSON.parse(doc.getCharPropertiesAt(0, 1, overlap ? 1 : 0)).bold).toBe(true)
    } finally {
      releaseHwpUndo(doc)
      doc.free()
    }
  }
})

it('follows the original paragraph when an earlier paragraph is removed', () => {
  const doc = body('앞 문단')
  try {
    doc.splitParagraph(0, 1, 4)
    doc.insertText(0, 2, 0, '원문')
    applyCompensation(doc, edit(1, 1, '삭제할 글', { ...pos, paragraph: 2 }), [
      { kind: 'deleteParagraph', section: 0, paragraph: 1 },
    ])
    expect(doc.getParagraphCount(0)).toBe(2)
    expect(doc.getTextRange(0, 1, 0, 16)).toBe('원문')
  } finally {
    doc.free()
  }
})

it('does not put a deleted fragment back into a gap swallowed by a later replacement', () => {
  const doc = body('앞🙂뒤')
  try {
    const erase = edit(1, 2, '')
    const plan = captureHwpTextUndo(doc, erase)
    applyHwpText(doc, erase)
    const replace = edit(0, 2, '동료')
    applyHwpText(doc, replace)
    expect(plan([replace])).toBeNull()
    expect(doc.getTextRange(0, 1, 0, 16)).toBe('동료')
  } finally {
    releaseHwpUndo(doc)
    doc.free()
  }
})

it('rejects malformed fragment runs and an unknown shape before inserting any text', () => {
  const doc = body('원본')
  const f = { kind: 'restoreText', at: pos, text: '🙂한' }
  try {
    for (const runs of [
      [],
      [[1, 1]],
      [
        [0, 1],
        [2, 1],
      ],
      [
        [0, 1],
        [0, 2],
      ],
      [[0, -1]],
      [[0, 3, 4]],
    ])
      expect(parseHwpTextFragment({ ...f, runs })).toBeNull()
    expect(parseHwpTextFragment({ ...f, runs: [[0, 0]], text: 'x\t' })).toBeNull()
    expect(() =>
      applyHwpOp(doc, decodeHwpOp({ ...f, at: { ...pos }, runs: [[0, 0xffffffff]] })),
    ).toThrow()
    expect(doc.getTextRange(0, 1, 0, 16)).toBe('원본')
  } finally {
    doc.free()
  }
})
