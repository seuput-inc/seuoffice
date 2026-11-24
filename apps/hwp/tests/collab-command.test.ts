// @vitest-environment node
import { readFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import { dirname, join } from 'node:path'
import init, { HwpDocument } from '@rhwp/core'
import JSZip from 'jszip'
import { beforeAll, expect, it } from 'vitest'
import {
  applyHwpCommand,
  captureHwpCommand,
  captureHwpCommandEffect,
  findHwpMemo,
  hwpCommandTextEffect,
  mapHwpCharShapeIds,
  parseHwpCommand,
} from '../src/shared/collab-command'
import { applyHwpStructure, captureHwpStructure } from '../src/shared/collab-structure'
import type { HwpPosition } from '../src/shared/collab-text'

const require = createRequire(import.meta.url)
const fixture = (name: string) => readFileSync(join(__dirname, 'fixtures', name))
beforeAll(async () => {
  await init({
    module_or_path: readFileSync(join(dirname(require.resolve('@rhwp/core')), 'rhwp_bg.wasm')),
  })
})

function captured(doc: HwpDocument, method: string, args: readonly unknown[]) {
  const local = captureHwpCommand(doc, method, args)
  const op = parseHwpCommand(JSON.parse(JSON.stringify(local)))
  if (!op) throw new Error(`${method} was not represented`)
  return op
}
function at(paragraph: number, offset = 0, path: HwpPosition['path'] = []): HwpPosition {
  return { section: 0, paragraph, offset, path }
}
function textSeed() {
  const seed = HwpDocument.createEmpty()
  try {
    seed.createBlankDocument()
    seed.splitParagraph(0, 0, seed.getParagraphLength(0, 0))
    seed.insertText(0, 1, 0, '가🙂나다')
    seed.splitParagraph(0, 1, 4)
    seed.insertText(0, 2, 0, '끝')
    return seed.exportHwpx()
  } finally {
    seed.free()
  }
}

it('replays only the selected font properties in a nested cell, without replacing its paragraph or its neighbours', () => {
  const bytes = fixture('nested-form.hwp'),
    a = new HwpDocument(bytes),
    b = new HwpDocument(bytes)
  try {
    const path = JSON.stringify([
      { controlIndex: 0, cellIndex: 9, cellParaIndex: 3 },
      { controlIndex: 0, cellIndex: 1, cellParaIndex: 0 },
    ])
    const args = [0, 2, path, 1, 3, '{"bold":true,"textColor":"#ac7532"}'] as const
    const op = captured(a, 'applyCharFormatInCellByPath', args)
    a.applyCharFormatInCellByPath(...args)
    applyHwpCommand(b, op)
    expect(b.getCharShapeRunsInCellByPath(0, 2, path, 0, 4)).toBe(
      a.getCharShapeRunsInCellByPath(0, 2, path, 0, 4),
    )
    expect(b.getTextInCellByPath(0, 2, path, 0, 4)).toBe(a.getTextInCellByPath(0, 2, path, 0, 4))
    const paraArgs = [0, 2, path, '{"alignment":"center","keepWithNext":true}'] as const
    const para = captured(a, 'applyParaFormatInCellByPath', paraArgs)
    a.applyParaFormatInCellByPath(...paraArgs)
    applyHwpCommand(b, para)
    expect(b.getCellParaPropertiesAtByPath(0, 2, path)).toBe(
      a.getCellParaPropertiesAtByPath(0, 2, path),
    )
    expect(
      captureHwpCommand(a, 'applyCharFormat', [0, 0, 0, 1, '{"bold":true,"bold":false}']),
    ).toBeNull()
    expect(parseHwpCommand({ ...op, props: { bold: true, url: 'unsupported' } })).toBeNull()
    expect(
      parseHwpCommand({
        ...para,
        props: { tabStops: [{ type: 0, position: 1, fill: 0, script: '' }] },
      }),
    ).toBeNull()
  } finally {
    a.free()
    b.free()
  }
})

it('pastes mixed styles without reading, replacing, or clearing the receiving clipboard', () => {
  const bytes = textSeed(),
    a = new HwpDocument(bytes),
    b = new HwpDocument(bytes)
  try {
    const style = captured(a, 'applyCharFormat', [0, 1, 1, 2, '{"italic":true}'])
    applyHwpCommand(a, style)
    applyHwpCommand(b, style)
    a.copySelection(0, 1, 0, 1, 3)
    b.copySelection(0, 2, 0, 2, 1)
    const op = captured(a, 'pasteInternal', [0, 2, 0])
    expect(op).toMatchObject({ action: 'pasteText', fragment: { text: '가🙂나' } })
    const after = captureHwpCommandEffect(b, op)
    a.pasteInternal(0, 2, 0)
    const change = after(applyHwpCommand(b, op))
    expect(b.getClipboardText()).toBe('끝')
    expect(b.getTextRange(0, 2, 0, 4)).toBe('가🙂나끝')
    expect(b.getCharShapeRuns(0, 2, 0, 4)).toBe(a.getCharShapeRuns(0, 2, 0, 4))
    expect(change.mapPosition(at(2, 1), 'after')).toEqual(at(2, 4))
    expect(change.unmapPosition(at(2, 2), 'after')).toBeNull()
    expect(change.mapCreatedPosition(at(2, 2), change)).toEqual(at(2, 2))
    expect(hwpCommandTextEffect(op)).toMatchObject([{ text: '가🙂나' }])
    a.copySelection(0, 1, 1, 2, 1)
    expect(captureHwpCommand(a, 'pasteInternal', [0, 2, 0])).toBeNull()
  } finally {
    a.free()
    b.free()
  }
})

it('finds the authored shape after a peer allocated the same ordinal for another style', () => {
  const bytes = textSeed(),
    a = new HwpDocument(bytes),
    b = new HwpDocument(bytes)
  try {
    a.applyCharFormat(0, 1, 1, 2, '{"italic":true}')
    b.applyCharFormat(0, 1, 2, 3, '{"bold":true}')
    a.copySelection(0, 1, 1, 1, 2)
    const op = captured(a, 'pasteInternal', [0, 2, 0])
    if (op.action !== 'pasteText') throw new Error('Missing formatted paste')
    const ids = op.fragment.runs.map(([, id]) => id)
    expect(ids.length).toBeGreaterThan(0)
    expect(mapHwpCharShapeIds(a, b, ids)).toBeNull()
    b.applyCharFormat(0, 1, 1, 2, '{"italic":true}')
    const mapped = mapHwpCharShapeIds(a, b, ids)!
    expect(mapped).not.toEqual(ids)
    applyHwpCommand(b, {
      ...op,
      fragment: { ...op.fragment, runs: op.fragment.runs.map(([i], j) => [i, mapped[j]]) },
    })
    expect(JSON.parse(b.getCharPropertiesAt(0, 2, 0))).toMatchObject({ italic: true, bold: false })
  } finally {
    a.free()
    b.free()
  }
})

it('maps both sides of a table inserted in the middle of text and keeps the new table addressable', () => {
  const bytes = textSeed(),
    a = new HwpDocument(bytes),
    b = new HwpDocument(bytes)
  try {
    const args = [0, 1, 2, 2, 3] as const,
      op = captured(a, 'createTable', args)
    const observe = captureHwpCommandEffect(b, op),
      local = JSON.parse(a.createTable(...args))
    const effect = observe(applyHwpCommand(b, op))
    expect(b.getTextRange(0, 1, 0, 4)).toBe('가🙂')
    expect(b.getTextRange(0, 4, 0, 4)).toBe('나다')
    expect(b.getTableDimensions(0, local.paraIdx, local.controlIdx)).toBe(
      a.getTableDimensions(0, local.paraIdx, local.controlIdx),
    )
    expect(effect.mapPosition(at(1, 3), 'after')).toEqual(at(4, 1))
    expect(effect.unmapPosition(at(4, 1), 'after')).toEqual(at(1, 3))
    expect(effect.mapPosition(at(2), 'after')).toEqual(at(5))
    const target = at(2, 0, [{ controlIndex: local.controlIdx, cellIndex: 3, cellParaIndex: 0 }])
    expect(effect.unmapPosition(target, 'after')).toBeNull()
    expect(effect.mapCreatedPosition(target, effect)).toEqual(target)
    expect(parseHwpCommand({ ...op, rows: 65535 })).toBeNull()
  } finally {
    a.free()
    b.free()
  }
})

it('maps cells by their actual row and column and does not reuse a deleted cell ordinal', () => {
  const doc = new HwpDocument(fixture('cell-line-break-form.hwpx'))
  try {
    const pos = (index: number) =>
      at(12, 0, [{ controlIndex: 0, cellIndex: index, cellParaIndex: 0 }])
    const old = JSON.parse(doc.getCellInfo(0, 12, 0, 1))
    const add = captured(doc, 'insertTableColumn', [0, 12, 0, 0, false]),
      observe = captureHwpCommandEffect(doc, add)
    const change = observe(applyHwpCommand(doc, add))
    const kept = change.mapPosition(pos(1), 'after')!
    expect(JSON.parse(doc.getCellInfo(0, 12, 0, kept.path[0].cellIndex))).toMatchObject({
      row: old.row,
      col: old.col + 1,
    })
    expect(change.unmapPosition(kept, 'after')).toEqual(pos(1))
    expect(change.mapPosition(pos(65534), 'after')).toEqual(pos(65534))
    expect(change.unmapPosition(pos(0), 'after')).toBeNull()
    expect(change.mapCreatedPosition(pos(0), change, (i) => i)).toEqual(pos(0))
    const remove = captured(doc, 'deleteTableColumn', [0, 12, 0, 0]),
      onDelete = captureHwpCommandEffect(doc, remove)
    const removed = onDelete(applyHwpCommand(doc, remove))
    expect(removed.mapPosition(pos(0), 'after')).toBeNull()
    expect(removed.mapPosition(kept, 'after')).toEqual(pos(1))
  } finally {
    doc.free()
  }
})

it('replays memo replacement and removal by identity and preserves the assigned picture bytes', async () => {
  const bytes = fixture('memo-source.hwp'),
    a = new HwpDocument(bytes),
    b = new HwpDocument(bytes)
  try {
    const memo = JSON.parse(a.getMemoList()).find((m: { editable: boolean }) => m.editable)
    expect(memo).toBeDefined()
    const op = captured(a, 'setMemoText', [memo.id, '바꾼 메모\n둘째 줄'])
    a.setMemoText(memo.id, '바꾼 메모\n둘째 줄')
    applyHwpCommand(b, op)
    expect(b.getMemoList()).toBe(a.getMemoList())
    expect(() => applyHwpCommand(b, op)).toThrow('memo changed')
    const target = findHwpMemo(b, memo.id)!
    const remove = captured(a, 'removeMemo', [memo.id]),
      observe = captureHwpCommandEffect(b, remove)
    a.removeMemo(memo.id)
    const effect = observe(applyHwpCommand(b, remove))
    expect(b.getMemoList()).toBe(a.getMemoList())
    const child = {
      ...target.at,
      path: [
        ...target.at.path,
        { controlIndex: target.control + 1, cellIndex: 0, cellParaIndex: 0 },
      ],
    }
    expect(effect.mapPosition(child, 'after')!.path.at(-1)?.controlIndex).toBe(target.control)
    const zip = await JSZip.loadAsync(fixture('picture-brightness-contrast-cell-fill.hwpx'))
    const png = await zip.file('BinData/image1.png')!.async('uint8array')
    const options = JSON.stringify({
      sectionIdx: 0,
      paraIdx: 0,
      charOffset: 0,
      width: 7200,
      height: 5400,
      naturalWidthPx: 64,
      naturalHeightPx: 6,
      extension: 'png',
      description: '원본 그림',
    })
    const inserted = captureHwpStructure(a, 'insertPictureEx', [options, png])
    if (!inserted || inserted.kind !== 'insertCellPicture')
      throw new Error('Missing picture operation')
    const local = applyHwpStructure(a, inserted, () => a.insertPictureEx(options, png))
    const replay = applyHwpStructure(b, inserted)
    expect(replay.result).toBe(local.result)
    const p = JSON.parse(replay.result)
    const pic = captured(b, 'setPictureProperties', [
      0,
      p.paraIdx,
      p.controlIdx,
      '{"brightness":20}',
    ])
    applyHwpCommand(b, pic)
    expect(JSON.parse(b.getPictureProperties(0, p.paraIdx, p.controlIdx))).toMatchObject({
      brightness: 20,
      description: '원본 그림',
    })
    const img = captured(b, 'assignPictureImage', [
      0,
      p.paraIdx,
      '',
      p.controlIdx,
      png,
      64,
      6,
      'png',
    ])
    applyHwpCommand(b, img)
    expect(b.getControlImageData(0, p.paraIdx, '[]', p.controlIdx)).toEqual(png)
    expect(parseHwpCommand({ ...img, data: 'AAAA' })).toBeNull()
  } finally {
    a.free()
    b.free()
  }
})

it('addresses an actual HWPX checkbox without transmitting form scripts or unrelated text', async () => {
  const zip = await JSZip.loadAsync(fixture('single-line-record-paragraph.hwpx')),
    file = 'Contents/section0.xml'
  const xml = await zip.file(file)!.async('string')
  zip.file(
    file,
    xml.replace(
      '<hp:t>',
      '<hp:checkBtn name="choice" enabled="1" caption="원본" value="UNCHECKED"><hp:sz width="1200" height="1200"/></hp:checkBtn><hp:t>',
    ),
  )
  const bytes = await zip.generateAsync({ type: 'uint8array' }),
    a = new HwpDocument(bytes),
    b = new HwpDocument(bytes)
  try {
    const count = JSON.parse(a.getControlTextPositions(0, 0)).length
    const ci = Array.from({ length: count }, (_, i) => i).find(
      (i) => JSON.parse(a.getFormValue(0, 0, i)).name === 'choice',
    )
    if (ci === undefined) throw new Error('Missing fixture checkbox')
    const args = [0, 0, ci, '{"value":1}'] as const
    const op = captured(a, 'setFormValue', args)
    a.setFormValue(...args)
    applyHwpCommand(b, op)
    expect(b.getFormValue(0, 0, ci)).toBe(a.getFormValue(0, 0, ci))
    const reopened = new HwpDocument(b.exportHwpx())
    try {
      expect(JSON.parse(reopened.getFormValue(0, 0, ci))).toMatchObject({
        value: 1,
        caption: '원본',
      })
    } finally {
      reopened.free()
    }
    expect(parseHwpCommand({ ...op, props: { value: 1, script: 'not transported' } })).toBeNull()
    expect(captureHwpCommand(a, 'restoreSnapshot', [0])).toBeNull()
  } finally {
    a.free()
    b.free()
  }
})
