import { describe, expect, it } from 'vitest'

import {
  assemblePrintHtml,
  layoutSheetPrint,
  type PrintWorksheet,
} from '../src/renderer/print-html'
import type { EffectivePageSetup } from '../src/renderer/print-settings'

const LONG = 'SEU-E2E-sheets'

interface FakeStyle {
  readonly ht?: number
  readonly tb?: number
}

/// 100px (75pt) columns, 20px (15pt) rows.
function worksheet(
  grid: readonly (readonly (string | number)[])[],
  styles: Readonly<Record<string, FakeStyle>> = {},
  merges: readonly { row: number; column: number; width: number }[] = [],
): PrintWorksheet {
  const slice = (row: number, column: number, numRows = 1, numColumns = 1) =>
    grid.slice(row, row + numRows).map((cells) => cells.slice(column, column + numColumns))
  return {
    getLastRow: () => grid.length - 1,
    getLastColumn: () => Math.max(...grid.map((cells) => cells.length)) - 1,
    getRowHeight: () => 20,
    getColumnWidth: () => 100,
    getMergedRanges: () =>
      merges.map((merge) => ({
        getRow: () => merge.row,
        getColumn: () => merge.column,
        getWidth: () => merge.width,
        getHeight: () => 1,
      })),
    getRange: ((row: number, column: number, numRows?: number, numColumns?: number) => ({
      getDisplayValues: () =>
        slice(row, column, numRows, numColumns).map((cells) => cells.map(String)),
      getValues: () => slice(row, column, numRows, numColumns),
      getCellStyleData: () => styles[`${row}:${column}`] ?? null,
    })) as PrintWorksheet['getRange'],
  }
}

function setup(overrides: Partial<EffectivePageSetup> = {}): EffectivePageSetup {
  return {
    orientation: 'portrait',
    paperSize: 9,
    scale: 100,
    fitToWidth: 0,
    fitToHeight: 0,
    fitToPage: false,
    margins: { left: 0.7, right: 0.7, top: 0.75, bottom: 0.75, header: 0.3, footer: 0.3 },
    printGridlines: false,
    printHeadings: false,
    printAreas: [],
    printTitles: null,
    printTitleColumns: null,
    header: null,
    footer: null,
    firstPage: null,
    evenPages: null,
    headerFooterScaleWithDoc: true,
    headerFooterPictures: [],
    ...overrides,
  }
}

interface TextBox {
  /// Left edge of the box the text is clipped to, from its cell's left edge.
  readonly leftPt: number
  readonly widthPt: number
  readonly escapesCell: boolean
}

/// The clip box of each printed cell's text, keyed by the text.
function textBoxes(sheet: PrintWorksheet, pageSetup = setup()): Map<string, TextBox> {
  const html = assemblePrintHtml([layoutSheetPrint(sheet, pageSetup, 'Book', 'S1')]).html
  const boxes = new Map<string, TextBox>()
  const cells = html.matchAll(
    /<td[^>]*? style="([^"]*)"><div class="c" style="([^"]*)"><span>([^<]*)<\/span>/g,
  )
  for (const [, cellStyle = '', boxStyle = '', text = ''] of cells) {
    const pt = (property: string): number | undefined => {
      const match = new RegExp(`(?:^|;)${property}:(-?[\\d.]+)pt`).exec(boxStyle)
      return match ? Number(match[1]) : undefined
    }
    boxes.set(text, {
      leftPt: pt('margin-left') ?? 0,
      widthPt: pt('width') ?? 75,
      escapesCell: /(?:^|;)overflow:visible/.test(cellStyle),
    })
  }
  return boxes
}

const CLIPPED: TextBox = { leftPt: 0, widthPt: 75, escapesCell: false }
/// A4 less the 0.7in side margins.
const PAGE_WIDTH_PT = (8.27 - 1.4) * 72

describe('print layout: text running past its cell', () => {
  it('runs unwrapped text over the empty cells up to the next value', () => {
    const boxes = textBoxes(worksheet([[LONG, '', '', 'stop', 'after']]))
    expect(boxes.get(LONG)).toEqual({ leftPt: 0, widthPt: 225, escapesCell: true })
    expect(boxes.get('stop')).toEqual(CLIPPED)
  })

  it('gives text in the last used column the rest of the page', () => {
    const box = textBoxes(worksheet([[LONG]])).get(LONG)
    expect(box?.escapesCell).toBe(true)
    expect(box?.leftPt).toBe(0)
    expect(box?.widthPt).toBeCloseTo(75 + PAGE_WIDTH_PT, 1)
  })

  it('ends the text at the edge of an explicit print area', () => {
    const sheet = worksheet([[LONG, '', 'outside']])
    expect(textBoxes(sheet, setup({ printAreas: ['A1:A1'] })).get(LONG)).toEqual(CLIPPED)
    expect(textBoxes(sheet, setup({ printAreas: ['A1:B1'] })).get(LONG)).toEqual({
      leftPt: 0,
      widthPt: 150,
      escapesCell: true,
    })
  })

  it('ends the text at the last column of a page that continues on the next one', () => {
    // 75pt columns on a 494pt page: six per page, so F ends the first page
    const row = Array.from({ length: 12 }, (_, column) =>
      column === 5 ? LONG : column === 11 ? 'last' : '',
    )
    const boxes = textBoxes(worksheet([row]))
    expect(boxes.get(LONG)).toEqual(CLIPPED)
    expect(boxes.get('last')?.escapesCell).toBe(true)
  })

  it('runs right-aligned text to the left and centred text to both sides', () => {
    const right = textBoxes(worksheet([['stop', '', '', LONG, 'x']], { '0:3': { ht: 3 } }))
    expect(right.get(LONG)).toEqual({ leftPt: -150, widthPt: 225, escapesCell: true })
    const centre = textBoxes(worksheet([['', LONG, '', '', 'x']], { '0:1': { ht: 2 } }))
    expect(centre.get(LONG)).toEqual({ leftPt: -75, widthPt: 300, escapesCell: true })
  })

  it('crosses a hidden column and stops where the printed columns leave sheet order', () => {
    const hidden: PrintWorksheet = {
      ...worksheet([[LONG, '', '', 'x']]),
      getSheet: () => ({ getRowVisible: () => true, getColVisible: (column) => column !== 1 }),
    }
    expect(textBoxes(hidden).get(LONG)).toEqual({ leftPt: 0, widthPt: 150, escapesCell: true })
    // B repeats at the left of the page, so the empty A prints to its right
    const titled = textBoxes(worksheet([['', LONG, '', 'x']]), setup({ printTitleColumns: 'B:B' }))
    expect(titled.get(LONG)).toEqual(CLIPPED)
  })

  it('keeps numbers, wrapped text and merged cells inside their own cell', () => {
    expect(textBoxes(worksheet([[1234567890123, '', 'x']])).get('1234567890123')).toEqual(CLIPPED)
    const wrapped = textBoxes(worksheet([[LONG, '', 'x']], { '0:0': { tb: 3 } }))
    expect(wrapped.get(LONG)).toEqual(CLIPPED)
    const merge = [{ row: 0, column: 1, width: 2 }]
    const blocked = textBoxes(worksheet([[LONG, '', '', 'x']], {}, merge))
    expect(blocked.get(LONG)).toEqual(CLIPPED)
  })
})
