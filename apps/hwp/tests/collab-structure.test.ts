import { readFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import { dirname, join } from 'node:path'
import init, { HwpDocument } from '@rhwp/core'
import JSZip from 'jszip'
import { beforeAll, expect, it } from 'vitest'
import {
  applyHwpStructure,
  captureHwpParagraphLoss,
  captureHwpStructure,
  mapHwpStructureOp,
  mapHwpStructurePosition,
  parseHwpStructure,
  type HwpStructureOp,
} from '../src/shared/collab-structure'
import { applyHwpText } from '../src/shared/collab-text-model'
import { mapHwpPosition, type HwpPosition, type HwpTextSplice } from '../src/shared/collab-text'

const require = createRequire(import.meta.url)
let png: Uint8Array
beforeAll(async () => {
  await init({
    module_or_path: readFileSync(join(dirname(require.resolve('@rhwp/core')), 'rhwp_bg.wasm')),
  })
  const zip = await JSZip.loadAsync(
    readFileSync(join(__dirname, 'fixtures/picture-brightness-contrast-cell-fill.hwpx')),
  )
  png = await zip.file('BinData/image1.png')!.async('uint8array')
})

function at(paragraph: number, offset = 0, path: HwpPosition['path'] = []): HwpPosition {
  return { section: 0, paragraph, offset, path }
}
function cell(cellIndex: number, controlIndex = 0): HwpPosition {
  return at(12, 0, [{ cellIndex, controlIndex, cellParaIndex: 0 }])
}
function picture(doc: HwpDocument, target: HwpPosition, method = 'insertPictureInCell') {
  const op = captureHwpStructure(doc, method, [
    target.section,
    target.paragraph,
    target.offset,
    JSON.stringify(target.path),
    png,
    4800,
    450,
    64,
    6,
    'png',
    '그림\r\n원본',
    -15,
    30,
  ])
  if (op?.kind !== 'insertCellPicture') throw new Error('Picture capture failed')
  return op
}
function deletion(from: HwpPosition, to: HwpPosition): HwpTextSplice {
  return { kind: 'splice', from, to, text: '' }
}

it.each(['insertPicture', 'insertPictureInCell'])(
  'replays the original %s anchoring and bytes after a wire round trip',
  async (method) => {
    const doc = new HwpDocument(readFileSync(join(__dirname, 'fixtures/nested-form.hwp')))
    try {
      const target = at(2, 1, [
        { controlIndex: 0, cellIndex: 9, cellParaIndex: 3 },
        { controlIndex: 0, cellIndex: 1, cellParaIndex: 0 },
      ])
      const captured = picture(doc, target, method)
      const op = parseHwpStructure(JSON.parse(JSON.stringify(captured)))!
      const reply = JSON.parse(applyHwpStructure(doc, op).result)
      const props =
        method === 'insertPicture'
          ? doc.getPictureProperties(0, 2, reply.controlIdx)
          : doc.getCellPicturePropertiesByPath(0, 2, JSON.stringify(target.path), reply.controlIdx)
      expect(JSON.parse(props)).toMatchObject({
        width: 4800,
        height: 450,
        description: '그림\r\n원본',
      })
      const zip = await JSZip.loadAsync(doc.exportHwpx())
      const entries = await Promise.all(
        zip.file(/^BinData\/.*\.png$/i).map((file) => file.async('uint8array')),
      )
      expect(entries.some((entry) => Buffer.from(entry).equals(png))).toBe(true)
    } finally {
      doc.free()
    }
  },
)

it('drops the deleted cell despite index reuse, and keeps a spanning cell and the independent row', () => {
  const doc = new HwpDocument(readFileSync(join(__dirname, 'fixtures/cell-line-break-form.hwpx')))
  try {
    expect(JSON.parse(doc.mergeTableCells(0, 12, 0, 0, 0, 1, 0)).ok).toBe(true)
    const spanning = picture(doc, cell(0)),
      lost = picture(doc, cell(1)),
      survivor = picture(doc, cell(3))
    const otherTable = picture(doc, cell(1, 1))
    doc.insertTextInCellByPath(0, 12, JSON.stringify(survivor.at.path), 0, '남음')
    const row = { kind: 'deleteRow', section: 0, paragraph: 12, control: 0, row: 0 } as const
    expect(captureHwpStructure(doc, 'deleteTableRow', [0, 12, 0, 0])).toEqual(row)
    const effect = applyHwpStructure(doc, row, () => doc.deleteTableRow(0, 12, 0, 0))
    expect(JSON.parse(effect.result)).toMatchObject({ ok: true, rowCount: 1 })
    expect(mapHwpStructureOp(row, effect)).toBeNull()
    expect(mapHwpStructureOp({ ...row, row: 1 }, effect)).toEqual({ ...row, row: 0 })
    expect(mapHwpStructureOp(lost, effect)).toBeNull()
    expect(mapHwpStructureOp(spanning, effect)).toEqual(spanning)
    expect(mapHwpStructureOp(otherTable, effect)).toEqual(otherTable)
    const kept = mapHwpStructureOp(survivor, effect)
    expect(kept).toEqual({ ...survivor, at: cell(1) })
    expect(doc.getTextInCellByPath(0, 12, JSON.stringify(cell(1).path), 0, 2)).toBe('남음')
    expect(JSON.parse(doc.getCellInfo(0, 12, 0, 0))).toMatchObject({ row: 0, rowSpan: 1 })
    const reply = JSON.parse(applyHwpStructure(doc, kept!).result)
    expect(
      JSON.parse(
        doc.getCellPicturePropertiesByPath(0, 12, JSON.stringify(cell(1).path), reply.controlIdx),
      ).description,
    ).toBe('그림\r\n원본')
  } finally {
    doc.free()
  }
})

it('deletes the whole native paragraph and remaps the next host and its table, not just its characters', () => {
  const doc = new HwpDocument(readFileSync(join(__dirname, 'fixtures/cell-line-break-form.hwpx')))
  try {
    const target = at(2, 1, [{ controlIndex: 0, cellIndex: 0, cellParaIndex: 0 }])
    const next = { ...target, paragraph: 4 }
    expect(doc.getTextInCellByPath(0, 2, JSON.stringify(target.path), 0, 20)).toBe('프로젝트 개요')
    const op = captureHwpStructure(doc, 'deleteParagraph', [0, 2])!
    const effect = applyHwpStructure(doc, op)
    expect(mapHwpStructurePosition(target, effect)).toBeNull()
    expect(mapHwpStructurePosition(at(2), effect)).toBeNull()
    expect(mapHwpStructurePosition(next, effect)).toEqual({ ...next, paragraph: 3 })
    expect(doc.getTextInCellByPath(0, 3, JSON.stringify(next.path), 0, 20)).toBe('키워드')
  } finally {
    doc.free()
  }
})

it('distinguishes fully consumed paragraph targets from partial boundary text and same-paragraph deletion', () => {
  const doc = new HwpDocument(readFileSync(join(__dirname, 'fixtures/generated-form-tables.hwpx')))
  try {
    const first = doc.getTextRange(0, 2, 0, 1000),
      last = doc.getTextRange(0, 6, 0, 1000)
    expect(first.length).toBeGreaterThan(2)
    expect(last.length).toBeGreaterThan(2)
    const splice = deletion(at(2, 1), at(6, 2))
    const lost = captureHwpParagraphLoss(doc, splice)
    expect(lost(at(2))).toBe(false)
    expect(lost(at(3))).toBe(true)
    expect(lost(at(6, 2))).toBe(false)
    expect(lost(at(7))).toBe(false)
    expect(captureHwpParagraphLoss(doc, deletion(at(2), at(3)))(at(2))).toBe(true)
    expect(
      captureHwpParagraphLoss(
        doc,
        deletion(at(2, first.length), at(3, doc.getParagraphLength(0, 3))),
      )(at(3)),
    ).toBe(true)
    expect(captureHwpParagraphLoss(doc, deletion(at(2), at(2, first.length)))(at(2))).toBe(false)
    expect(captureHwpParagraphLoss(doc, deletion(at(2, first.length), at(3)))(at(3))).toBe(false)
    const nextIsEmpty = captureHwpParagraphLoss(doc, deletion(at(3), at(4)))
    expect(nextIsEmpty(at(3))).toBe(true)
    expect(nextIsEmpty(at(4))).toBe(false)
    const mergeEmpty = captureHwpParagraphLoss(doc, deletion(at(4), at(5)))
    expect(mergeEmpty(at(4))).toBe(false)
    expect(mergeEmpty(at(5))).toBe(true)
    applyHwpText(doc, splice)
    const mapped = mapHwpPosition(at(6, 2), [splice])!
    expect(mapped).toEqual(at(2, 1))
    expect(doc.getTextRange(0, mapped.paragraph, mapped.offset, 2)).toBe(
      [...last].slice(2, 4).join(''),
    )
  } finally {
    doc.free()
  }
})

it('rejects out-of-bounds native addresses and malformed or excessive wire payloads', () => {
  const doc = new HwpDocument(readFileSync(join(__dirname, 'fixtures/cell-line-break-form.hwpx')))
  try {
    const op = picture(doc, cell(0))
    const forged = Buffer.from(png)
    forged.writeUInt32BE(0x7fffffff, 16)
    const wrong: unknown[] = [
      { ...op, saved: true },
      { ...op, at: at(0) },
      { ...op, at: { ...op.at, offset: -1 } },
      { ...op, data: png },
      { ...op, data: 'not-an-image' },
      { ...op, data: op.data + '====' },
      { ...op, extension: 'webp' },
      { ...op, extension: '../png' },
      { ...op, description: 'x'.repeat(4097) },
      { ...op, width: -1 },
      { ...op, height: NaN },
      { ...op, naturalWidthPx: 65535, naturalHeightPx: 65535 },
      { ...op, offsetYHu: 0x80000000 },
      { ...op, data: forged.toString('base64') },
      { ...op, data: op.data.repeat(45_000) },
      { kind: 'deleteRow', section: 0, paragraph: 12, control: 0, row: 65536 },
      { kind: 'deleteParagraph', section: 0, paragraph: 1, length: 2 },
    ]
    for (const bad of wrong) expect(parseHwpStructure(bad)).toBeNull()
    for (const at of [cell(99), { ...cell(0), offset: 1_000_000 }]) {
      expect(() => applyHwpStructure(doc, { ...op, at })).toThrow()
    }
    const badRow: HwpStructureOp = {
      kind: 'deleteRow',
      section: 0,
      paragraph: 12,
      control: 0,
      row: 2,
    }
    expect(() => applyHwpStructure(doc, badRow)).toThrow()
    expect(JSON.parse(doc.getTableDimensions(0, 12, 0)).rowCount).toBe(2)
  } finally {
    doc.free()
  }
})
