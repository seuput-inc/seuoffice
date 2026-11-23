import { createHash } from 'node:crypto'
import type { HistoryRequest, Json } from '@seuoffice/collab/client'
import { render } from './render'
import JSZip from 'jszip'
import { SaxesParser } from 'saxes'
import { describe, expect, it } from 'vitest'

const sheetNs = 'http://schemas.openxmlformats.org/spreadsheetml/2006/main'
const relNs = 'http://schemas.openxmlformats.org/officeDocument/2006/relationships'
const pkg = 'http://schemas.openxmlformats.org/package/2006/'
const setValues = 'sheet.mutation.set-range-values'
const paths = ['xl/worksheets/alpha.xml', 'xl/worksheets/beta.xml']
const baseCell = '<c r="A1" t="inlineStr"><is><t>Old</t></is></c>'

async function workbook(second = baseCell, first = '<c r="A1"><v>41</v></c>') {
  const zip = new JSZip()
  zip.file(
    '[Content_Types].xml',
    `<Types xmlns="${pkg}content-types">` +
      '<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>' +
      '<Default Extension="xml" ContentType="application/xml"/>' +
      '<Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>' +
      paths
        .map(
          (path) =>
            `<Override PartName="/${path}" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>`,
        )
        .join('') +
      '</Types>',
  )
  zip.file(
    '_rels/.rels',
    `<Relationships xmlns="${pkg}relationships"><Relationship Id="rId1" Type="${relNs}/officeDocument" Target="xl/workbook.xml"/></Relationships>`,
  )
  zip.file(
    'xl/workbook.xml',
    `<workbook xmlns="${sheetNs}" xmlns:r="${relNs}"><sheets>` +
      '<sheet name="First" sheetId="42" r:id="rId9"/><sheet name="Second" sheetId="7" r:id="rId1"/></sheets></workbook>',
  )
  zip.file(
    'xl/_rels/workbook.xml.rels',
    `<Relationships xmlns="${pkg}relationships">` +
      `<Relationship Id="rId1" Type="${relNs}/worksheet" Target="worksheets/beta.xml"/>` +
      `<Relationship Id="rId9" Type="${relNs}/worksheet" Target="worksheets/alpha.xml"/></Relationships>`,
  )
  for (const [i, cells] of [first, second].entries()) {
    zip.file(
      paths[i]!,
      `<worksheet xmlns="${sheetNs}"><sheetData><row r="1">${cells}</row></sheetData></worksheet>`,
    )
  }
  return zip.generateAsync({ type: 'uint8array', compression: 'DEFLATE', streamFiles: true })
}

function cell(id: string, sheet: string, column: number, value: Json): Json {
  return {
    id,
    m: [
      [
        setValues,
        {
          subUnitId: sheet,
          trigger: 'sheet.command.set-range-values',
          cellValue: { 0: { [column]: value } },
        },
      ],
    ],
  }
}

function request(base: Uint8Array, changes: Json[][], undoIndex?: number): HistoryRequest {
  return {
    format: { name: 'xlsx', schema: 3 },
    epoch: 'book',
    version: changes.length,
    baseHash: createHash('sha256').update(base).digest('hex'),
    changes: changes.map((ops, i) => ({
      v: i + 1,
      client: `client${i}`,
      author: `Author ${i}`,
      seq: 1,
      ops,
    })),
    ...(undoIndex === undefined ? {} : { undo: { epoch: 'book', v: 1, index: undoIndex } }),
  }
}

async function reopened(bytes: Uint8Array) {
  const zip = await JSZip.loadAsync(bytes)
  return Promise.all(
    paths.map(async (path) => {
      const parser = new SaxesParser({ xmlns: false })
      const cells: Record<string, string | number> = {}
      let address = '',
        text = '',
        inline = false,
        reading = false
      parser.on('opentag', ({ name, attributes }) => {
        if (name === 'c') {
          address = attributes.r!
          inline = attributes.t === 'inlineStr'
        }
        if (name === 'v' || name === 't') {
          reading = true
          text = ''
        }
      })
      parser.on('text', (chunk) => {
        if (reading) text += chunk
      })
      parser.on('closetag', ({ name }) => {
        if (name === 'v' || name === 't') {
          cells[address] = inline ? text : Number(text)
          reading = false
        }
      })
      parser.write(await zip.file(path)!.async('string')).close()
      return cells
    }),
  )
}

describe('XLSX sheet-id and typed cell history', () => {
  const target = cell('alice', 'sheet-7', 0, {
    v: 'Alice & <team>',
    t: 1,
    p: null,
    f: null,
    si: null,
  })
  it('reopens the prefix and appends peer-readable compensation without changing another cell or another sheet', async () => {
    const base = await workbook()
    const prior = cell('prior', 'sheet-7', 0, { v: '007', t: 1 })
    const other = [
      cell('bob', 'sheet-42', 0, { v: 0, t: 2 }),
      cell('carol', 'sheet-7', 1, { v: 12.5 }),
    ]
    const first = [prior, target]
    const prefix = await render(base, request(base, [first]))
    expect(await reopened(prefix.bytes)).toEqual([{ A1: 41 }, { A1: 'Alice & <team>' }])
    const result = await render(base, request(base, [first, other], 1))
    expect(result.compensation).toEqual([
      {
        id: expect.any(String),
        m: [[setValues, { subUnitId: 'sheet-7', cellValue: { 0: { 0: { v: '007', t: 1 } } } }]],
      },
    ])
    expect(await reopened(result.bytes)).toEqual([{ A1: 0 }, { A1: '007', B1: 12.5 }])
    const peer = await render(base, request(base, [first, other, result.compensation!]))
    expect(await reopened(peer.bytes)).toEqual(await reopened(result.bytes))
    expect(await reopened(base)).toEqual([{ A1: 41 }, { A1: 'Old' }])
  })

  it('restores the absence of a new cell without clearing a neighboring later write', async () => {
    const base = await workbook()
    const result = await render(
      base,
      request(base, [[cell('new', 'sheet-7', 1, { v: 'fresh' }), target]], 0),
    )
    expect(result.compensation).toEqual([
      {
        id: expect.any(String),
        m: [[setValues, { subUnitId: 'sheet-7', cellValue: { 0: { 1: { v: null } } } }]],
      },
    ])
    expect(await reopened(result.bytes)).toEqual([{ A1: 41 }, { A1: 'Alice & <team>' }])
  })

  it('restores the file reader’s inline text rather than the raw XML whitespace', async () => {
    const base = await workbook('<c r="A1" t="inlineStr"><is><t>  left_x000D_right  </t></is></c>')
    const result = await render(base, request(base, [[target]], 0))
    expect(await reopened(result.bytes)).toEqual([{ A1: 41 }, { A1: 'left\nright' }])
  })

  it('conflicts even if a later write to the same cell in the same change repeats the value', async () => {
    const base = await workbook()
    await expect(
      render(
        base,
        request(base, [[target, cell('later', 'sheet-7', 0, { v: 'Alice & <team>' })]], 0),
      ),
    ).rejects.toMatchObject({ code: 'conflict' })
  })

  const rejected: [string, Json, string][] = [
    ['coerced untyped string', cell('op', 'sheet-7', 0, { v: '007' }), 'unsupported'],
    ['boolean type is not a number', cell('op', 'sheet-7', 0, { v: 1, t: 3 }), 'unsupported'],
    ['nonliteral formula', cell('op', 'sheet-7', 0, { f: '=1+1' }), 'unsupported'],
    [
      'structural operation',
      {
        id: 'op',
        m: [
          [
            'sheet.mutation.insert-row',
            {
              subUnitId: 'sheet-7',
              range: { startRow: 0, endRow: 0, startColumn: 0, endColumn: 25 },
            },
          ],
        ],
      },
      'unsupported',
    ],
    ['sheet ordinal is not its id', cell('op', 'sheet-2', 0, { v: 1 }), 'invalid_history'],
    ['malformed matrix index', cell('op', 'sheet-7', -1, { v: 1 }), 'invalid_history'],
  ]
  it.each(rejected)('rejects %s', async (_reason, op, code) => {
    const base = await workbook()
    await expect(render(base, request(base, [[op]]))).rejects.toMatchObject({ code })
  })

  it('rejects a formula on another sheet before assuming a literal edit is independent', async () => {
    const base = await workbook(baseCell, '<c r="A1"><f>Second!A1</f><v>2</v></c>')
    await expect(render(base, request(base, [[target]], 0))).rejects.toMatchObject({
      code: 'unsupported',
    })
  })

  it('rejects ambiguous source addresses instead of picking one before-image', async () => {
    const base = await workbook(baseCell + baseCell)
    await expect(render(base, request(base, [[target]], 0))).rejects.toMatchObject({
      code: 'invalid_history',
    })
  })
})
