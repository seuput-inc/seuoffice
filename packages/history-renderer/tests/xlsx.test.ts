import { createHash } from 'node:crypto'
import type { HistoryRequest, Json } from '@seuoffice/collab/client'
import { render } from './render'
import JSZip from 'jszip'
import { SaxesParser } from 'saxes'
import { describe, expect, it } from 'vitest'
import {
  editorCellOp,
  editorClearOp,
  typedCell,
} from '../../../apps/sheets/tests/collab/editor-wire'

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

function cellCompensation(sheet: string, matrix: Json) {
  return { id: expect.any(String), m: [[setValues, { subUnitId: sheet, cellValue: matrix }]] }
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
      cellCompensation('sheet-7', { 0: { 0: { v: '007', t: 1 } } }),
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
    expect(result.compensation).toEqual([cellCompensation('sheet-7', { 0: { 1: { v: null } } })])
    expect(await reopened(result.bytes)).toEqual([{ A1: 41 }, { A1: 'Alice & <team>' }])
  })

  it('restores the file reader’s inline text rather than the raw XML whitespace', async () => {
    const base = await workbook('<c r="A1" t="inlineStr"><is><t>  left_x000D_right  </t></is></c>')
    const result = await render(base, request(base, [[target]], 0))
    expect(result.compensation).toEqual([
      cellCompensation('sheet-7', { 0: { 0: { v: 'left\nright', t: 1 } } }),
    ])
    expect(result.bytes).toEqual(base)
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

  it('patches styled shared strings and sparse ranges without rebuilding independent workbook parts', async () => {
    const fx = '<c r="C1"><f>SUM(D1:D2)</f><v>4</v></c>'
    const source = await JSZip.loadAsync(
      await workbook(`<c r="A1" t="s" s="1"><v>0</v></c>${fx}<c r="D1"><v>4</v></c>`),
    )
    const modify = async (path: string, anchor: string, prefix: string) =>
      source.file(path, (await source.file(path)!.async('string')).replace(anchor, prefix + anchor))
    for (const [part, kind] of [
      ['styles', 'styles'],
      ['sharedStrings', 'sharedStrings'],
    ] as const) {
      await modify(
        'xl/_rels/workbook.xml.rels',
        '</Relationships>',
        `<Relationship Id="${part}" Type="${relNs}/${kind}" Target="${part}.xml"/>`,
      )
      await modify(
        '[Content_Types].xml',
        '</Types>',
        `<Override PartName="/xl/${part}.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.${kind}+xml"/>`,
      )
    }
    source.file(
      'xl/styles.xml',
      `<styleSheet xmlns="${sheetNs}"><cellXfs count="2"><xf numFmtId="0"/><xf numFmtId="4"/></cellXfs></styleSheet>`,
    )
    source.file('xl/sharedStrings.xml', `<sst xmlns="${sheetNs}"><si><t>Shared</t></si></sst>`)
    source.file('customXml/extra.bin', Uint8Array.of(0, 255, 33))
    source.file(
      'xl/charts/chart2.xml',
      '<c:chartSpace xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart"/>',
    )
    const merge = '<mergeCells count="1"><mergeCell ref="B2:C2"/></mergeCells>'
    await modify(paths[1]!, '</worksheet>', merge)
    const base = await source.generateAsync({ type: 'uint8array' })
    const matrix = { 0: { 0: { v: 'Changed' }, 1: { v: 5 } }, 2: { 0: { v: 7 } } }
    const patch: Json = {
      id: 'batch',
      m: [[setValues, { subUnitId: 'sheet-7', cellValue: matrix }]],
    }
    const grid: Json = {
      id: 'grid',
      m: [['sheet.mutation.toggle-gridlines', { subUnitId: 'sheet-7', showGridlines: 0 }]],
    }
    const prefix = await JSZip.loadAsync((await render(base, request(base, [[patch]]))).bytes)
    const patched = await prefix.file(paths[1]!)!.async('string')
    expect(patched).toContain(
      '<c r="A1" s="1" t="inlineStr"><is><t xml:space="preserve">Changed</t></is></c>',
    )
    expect(patched).toContain('<c r="B1"><v>5</v></c>')
    expect(patched).toContain('<c r="A3"><v>7</v></c>')
    for (const fragment of [fx, merge]) expect(patched).toContain(fragment)
    const undone = await render(base, request(base, [[patch], [grid]], 0))
    expect(undone.compensation).toEqual([
      cellCompensation('sheet-7', {
        0: { 0: { v: 'Shared', t: 1 }, 1: { v: null } },
        2: { 0: { v: null } },
      }),
    ])
    const saved = await JSZip.loadAsync(undone.bytes)
    const xml = await saved.file(paths[1]!)!.async('string')
    expect(xml).toContain('<c r="A1" t="s" s="1"><v>0</v></c>')
    expect(xml).toContain('showGridLines="0"')
    expect(xml).not.toMatch(/r="(?:B1|A3)"/)
    for (const path of [
      'customXml/extra.bin',
      'xl/charts/chart2.xml',
      'xl/sharedStrings.xml',
      'xl/styles.xml',
      paths[0]!,
    ]) {
      const expected = await source.file(path)!.async('uint8array')
      expect(await prefix.file(path)!.async('uint8array')).toEqual(expected)
      expect(await saved.file(path)!.async('uint8array')).toEqual(expected)
    }
    const overlap = cell('peer', 'sheet-7', 1, { v: 10 })
    await expect(render(base, request(base, [[patch], [overlap]], 0))).rejects.toMatchObject({
      code: 'conflict',
    })
    const merged: Json = {
      id: 'merged',
      m: [[setValues, { subUnitId: 'sheet-7', cellValue: { 1: { 2: { v: 'hidden' } } } }]],
    }
    for (const unsafe of [
      merged,
      cell('formula', 'sheet-7', 2, { v: 4 }),
      cell('precedent', 'sheet-7', 3, { v: 5 }),
    ]) {
      await expect(render(base, request(base, [[unsafe]]))).rejects.toMatchObject({
        code: 'unsupported',
      })
    }
  })

  it('undoes independent sheet view flags without clobbering a later cell or another flag', async () => {
    const base = await workbook()
    const hide: Json = {
      id: 'view',
      m: [['sheet.mutation.toggle-gridlines', { subUnitId: 'sheet-7', showGridlines: 0 }]],
    }
    const rtl: Json = {
      id: 'mirror',
      m: [['sheet.mutation.set-worksheet-right-to-left', { subUnitId: 'sheet-7', rightToLeft: 1 }]],
    }
    const result = await render(base, request(base, [[hide], [target, rtl]], 0))
    expect(result.compensation).toEqual([
      {
        id: expect.any(String),
        m: [['sheet.mutation.toggle-gridlines', { subUnitId: 'sheet-7', showGridlines: 1 }]],
      },
    ])
    const sheet = (await JSZip.loadAsync(result.bytes)).file(paths[1]!)!
    const xml = await sheet.async('string')
    expect(xml).toContain('rightToLeft="1"')
    expect(xml).not.toContain('showGridLines="0"')
    expect(xml).toContain('Alice &amp; &lt;team&gt;')
    await expect(render(base, request(base, [[hide], [hide]], 0))).rejects.toMatchObject({
      code: 'conflict',
    })
  })

  const rejected: [string, Json, string][] = [
    ['coerced untyped string', cell('op', 'sheet-7', 0, { v: '007' }), 'unsupported'],
    ['boolean type is not a number', cell('op', 'sheet-7', 0, { v: 1, t: 3 }), 'unsupported'],
    ['nonliteral formula', cell('op', 'sheet-7', 0, { f: '=1+1' }), 'unsupported'],
    [
      'unexpected field on a view setting',
      {
        id: 'op',
        m: [
          [
            'sheet.mutation.toggle-gridlines',
            { subUnitId: 'sheet-7', showGridlines: 0, unitId: 'other' },
          ],
        ],
      },
      'invalid_history',
    ],
    [
      'format-only write',
      {
        id: 'op',
        m: [
          [
            setValues,
            {
              subUnitId: 'sheet-7',
              cellValue: { 0: { 0: { v: 5 } } },
              options: { formatOnly: true },
            },
          ],
        ],
      },
      'unsupported',
    ],
    [
      'indirect view change',
      {
        id: 'op',
        m: [
          [
            'sheet.mutation.toggle-gridlines',
            { subUnitId: 'sheet-7', showGridlines: 0, trigger: 'sheet.command.paste' },
          ],
        ],
      },
      'unsupported',
    ],
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

  it('rejects a write read by a formula on another sheet', async () => {
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

describe('XLSX ops in the Sheets editor wire form', () => {
  it('replays and undoes typed cells whose format object is empty', async () => {
    const base = await workbook()
    const typed = await editorCellOp('alice', 'sheet-7', { 0: { 1: typedCell('Typed') } })
    const peer = await editorCellOp('bob', 'sheet-7', { 0: { 2: typedCell('Peer') } })
    const prefix = await render(base, request(base, [[typed]]))
    expect(await reopened(prefix.bytes)).toEqual([{ A1: 41 }, { A1: 'Old', B1: 'Typed' }])
    const undone = await render(base, request(base, [[typed], [peer]], 0))
    expect(await reopened(undone.bytes)).toEqual([{ A1: 41 }, { A1: 'Old', C1: 'Peer' }])
    const appended = await render(base, request(base, [[typed], [peer], undone.compensation!]))
    expect(await reopened(appended.bytes)).toEqual(await reopened(undone.bytes))
  })

  it('clears a cell with the Delete key op and restores it on undo', async () => {
    const base = await workbook()
    const cleared = await editorClearOp('alice', 'sheet-7', { 0: { 0: null } })
    const prefix = await render(base, request(base, [[cleared]]))
    expect(await reopened(prefix.bytes)).toEqual([{ A1: 41 }, {}])
    const undone = await render(base, request(base, [[cleared]], 0))
    expect(await reopened(undone.bytes)).toEqual([{ A1: 41 }, { A1: 'Old' }])
  })

  it('names the format it cannot write instead of dropping it', async () => {
    const base = await workbook()
    const bold = await editorCellOp('alice', 'sheet-7', {
      0: { 1: { ...typedCell('Bold'), s: { bl: 1 } } },
    })
    await expect(render(base, request(base, [[bold]]))).rejects.toMatchObject({
      code: 'unsupported',
      message: 'Sheets cell format change: bl',
    })
    // A cell Univer deletes outright loses its format too; this row has one.
    const formatted = await workbook(`<c r="A1"><v>1</v></c>`)
    const zip = await JSZip.loadAsync(formatted)
    const sheet = (await zip.file(paths[1]!)!.async('string')).replace(
      '<row r="1">',
      '<row r="1" s="0" customFormat="1">',
    )
    zip.file(paths[1]!, sheet)
    const styled = await zip.generateAsync({ type: 'uint8array', compression: 'DEFLATE' })
    const removed = cell('alice', 'sheet-7', 0, null)
    await expect(render(styled, request(styled, [[removed]]))).rejects.toMatchObject({
      code: 'unsupported',
      message: 'deleting a formatted Sheets cell also clears its format',
    })
    const plain = await render(base, request(base, [[removed]]))
    expect(await reopened(plain.bytes)).toEqual([{ A1: 41 }, {}])
  })
})
