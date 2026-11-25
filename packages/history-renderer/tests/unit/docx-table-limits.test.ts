import '@seuoffice/engine-wasm/node'
import type * as Tables from 'prosemirror-tables'
import { beforeEach, expect, it, vi } from 'vitest'
import { buildDocx } from '../../../docx-engine/tests/helpers/build-docx'

const repair = vi.hoisted(() => ({ calls: 0 }))
vi.mock('prosemirror-tables', async (original) => {
  const tables = await original<typeof Tables>()
  return {
    ...tables,
    fixTables: (...args: Parameters<typeof tables.fixTables>) => {
      repair.calls++
      return tables.fixTables(...args)
    },
  }
})
const { docxModel } = await import('../../src/docx')
beforeEach(() => {
  repair.calls = 0
})

const tc = (span?: string) =>
  `<w:tc>${span ? `<w:tcPr><w:gridSpan w:val="${span}"/></w:tcPr>` : ''}<w:p/></w:tc>`
// Without a grid or cell widths the engine keeps each w:gridSpan as written.
const tables = (count: number, rows: string) =>
  `<w:tbl><w:tblPr/>${rows}</w:tbl><w:p/>`.repeat(count)

it.each([
  ['a span past the grid limit', tables(1, `<w:tr>${tc('1000000000')}</w:tr><w:tr>${tc()}</w:tr>`)],
  ['a fractional span', tables(1, `<w:tr>${tc('1.5')}</w:tr><w:tr>${tc()}</w:tr>`)],
  // 2 x 65536 slots in each table; the editor would add 65535 empty cells to each second row.
  [
    'tables whose grids add up past the limit',
    tables(2, `<w:tr>${tc('65536')}</w:tr><w:tr>${tc()}</w:tr>`),
  ],
])('refuses %s before laying the table out', async (_, bodyXml) => {
  await expect(docxModel(await buildDocx({ bodyXml }))).rejects.toMatchObject({
    code: 'unsupported',
  })
  expect(repair.calls).toBe(0)
})

it('lays out a table within the limit', async () => {
  const bodyXml = tables(1, `<w:tr>${tc('4')}</w:tr><w:tr>${tc()}</w:tr>`)
  // The second row gains three cells of four positions each.
  expect((await docxModel(await buildDocx({ bodyXml }))).content.size).toBe(2 + 2 + 4 + 2 + 16 + 2)
  expect(repair.calls).toBeGreaterThan(0)
})
