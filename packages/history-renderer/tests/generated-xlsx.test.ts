import { createHash } from 'node:crypto'
import { readFile } from 'node:fs/promises'
import { join } from 'node:path'
import type { HistoryRequest, Json } from '@seuoffice/collab/client'
import JSZip from 'jszip'
import { expect, it } from 'vitest'
import { render } from './render'

const fixtures = process.env.GENERATED_FIXTURES
const sheet = 'xl/worksheets/sheet1.xml'
const mutation = 'sheet.mutation.set-range-values'
function write(id: string, col: number, v: string | number): Json {
  return { id, m: [[mutation, { subUnitId: 'sheet-1', cellValue: { 0: { [col]: { v } } } }]] }
}
function request(bytes: Uint8Array, ops: Json[][], undo = false): HistoryRequest {
  return {
    format: { name: 'xlsx', schema: 3 },
    epoch: 'book',
    version: ops.length,
    baseHash: createHash('sha256').update(bytes).digest('hex'),
    changes: ops.map((ops, i) => ({
      ops,
      v: i + 1,
      client: `client${i}`,
      author: `Author ${i}`,
      seq: 1,
    })),
    ...(undo ? { undo: { epoch: 'book', v: 1, index: 0 } } : {}),
  }
}

it.skipIf(!fixtures)(
  'preserves the openpyxl 3.1.5 workbook across edits, compensation and replay',
  async () => {
    const bytes = await readFile(join(fixtures!, 'plain.xlsx'))
    const base = await JSZip.loadAsync(bytes)
    const changes = [[write('alice', 0, 'changed')], [write('bob', 1, 9)]]
    const exported = await render(bytes, request(bytes, changes))
    const undone = await render(bytes, request(bytes, changes, true))
    expect(undone.compensation).toEqual([
      {
        id: expect.any(String),
        m: [
          [
            mutation,
            {
              subUnitId: 'sheet-1',
              cellValue: { 0: { 0: { v: 'base', t: 1 } } },
            },
          ],
        ],
      },
    ])
    const peer = await render(bytes, request(bytes, [...changes, undone.compensation!]))
    const original = await base.file(sheet)!.async('string')
    const baseA = '<c r="A1" t="inlineStr"><is><t>base</t></is></c>'
    const newA = '<c r="A1" t="inlineStr"><is><t xml:space="preserve">changed</t></is></c>'
    const withBob = original.replace('<c r="B1" t="n"><v>2</v></c>', '<c r="B1"><v>9</v></c>')
    expect(original).toContain(baseA)
    expect(withBob).not.toBe(original)
    for (const [result, expected] of [
      [exported, withBob.replace(baseA, newA)],
      [undone, withBob],
      [peer, withBob],
    ] as const) {
      const saved = await JSZip.loadAsync(result.bytes)
      expect(Object.keys(saved.files).sort()).toEqual(Object.keys(base.files).sort())
      for (const [path, entry] of Object.entries(base.files)) {
        const actual = saved.files[path]!
        expect(actual.date).toEqual(entry.date)
        expect(actual.comment).toEqual(entry.comment)
        expect(actual.unixPermissions).toEqual(entry.unixPermissions)
        if (path === sheet) expect(await actual.async('string')).toBe(expected)
        else expect(await actual.async('uint8array')).toEqual(await entry.async('uint8array'))
      }
    }
  },
)

it.skipIf(!fixtures).each([
  ['xl/workbook.xml', '<workbookProtection/>', '<workbookProtection lockStructure="1"/>'],
  ['xl/workbook.xml', 'tabRatio="600"', 'unknown="600"'],
  ['xl/workbook.xml', 'tabRatio="600"', 'tabRatio="601"'],
  [
    'xl/workbook.xml',
    '<workbookPr/>',
    '<workbookPr a="0" b="0" c="0" d="0" e="0" f="0" g="0" h="0" i="0"/>',
  ],
  [sheet, '<sheetPr>', '<sheetProtection sheet="1"/><sheetPr>'],
  [sheet, '<v>2</v>', '<f>SEQUENCE(3)</f><v>2</v>'],
])('refuses unsupported openpyxl markup: %s %s', async (path, before, after) => {
  const base = await JSZip.loadAsync(await readFile(join(fixtures!, 'plain.xlsx')))
  const original = await base.file(path)!.async('string')
  expect(original).toContain(before)
  base.file(path, original.replace(before, after), { createFolders: false })
  const bytes = await base.generateAsync({ type: 'uint8array' })
  await expect(
    render(bytes, request(bytes, [[write('alice', 0, 'changed')]])),
  ).rejects.toMatchObject({ code: 'unsupported' })
})
