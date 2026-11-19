// @vitest-environment jsdom
import { beforeEach, describe, expect, it, vi } from 'vitest'
import { sheetPictureDocument } from '../src/renderer/page-layout-actions'
import { createEditJournal } from '../src/renderer/edit-journal'
import { setModuleLang } from '../src/renderer/i18n/locale'
import type { LazyWorkbookState, UniverRuntime } from '../src/renderer/univer-state'
import { preloadEntireWorkbook } from '../src/renderer/univer-sync'

vi.mock('../src/renderer/univer-sync', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../src/renderer/univer-sync')>()
  return { ...actual, preloadEntireWorkbook: vi.fn() }
})

const ROW_HEIGHT_PX = 20
const COLUMN_WIDTH_PX = 100

/// A streamed workbook (not fully loaded) whose one sheet has `rows` by `columns` cells.
function openSheet(rows: number, columns: number, hiddenGridLines = false) {
  const worksheet = {
    getSheetId: () => 'sheet-1',
    getSheetName: () => 'Sheet1',
    getLastRow: () => rows - 1,
    getLastColumn: () => columns - 1,
    getRowHeight: () => ROW_HEIGHT_PX,
    getColumnWidth: () => COLUMN_WIDTH_PX,
    getMergedRanges: () => [],
    getRange: (_row: number, _column: number, numRows = 1, numColumns = 1) => {
      const values = Array.from({ length: numRows }, () => Array<string>(numColumns).fill('x'))
      return {
        getValues: () => values,
        getDisplayValues: () => values,
        getCellStyleData: () => null,
      }
    },
    getMaxRows: () => rows,
    getMaxColumns: () => columns,
    hasHiddenGridLines: () => hiddenGridLines,
  }
  const state = {
    file: {
      name: 'test.xlsx',
      sessionId: 'session-1',
      sheets: [{ id: 'sheet-1', name: 'Sheet1', rowCount: rows, columnCount: columns }],
      visuals: [],
    },
    flags: { preloadComplete: false, preloadRunning: false },
    editJournal: createEditJournal(),
    sheetFilePageSetups: new Map(),
    sheetPageBreaks: new Map(),
    sheetProtections: new Map(),
    sheetProtectedRanges: new Map(),
  } as unknown as LazyWorkbookState
  const runtime = {
    univerAPI: { getActiveWorkbook: () => ({ getActiveSheet: () => worksheet }) },
  } as unknown as UniverRuntime
  return { univerRef: { current: runtime }, lazyWorkbookRef: { current: state } }
}

/// The file's page setup of the sheet, as its first range read reports it.
function filePrints(pageSetup: unknown) {
  vi.stubGlobal('window', {
    desktopApi: {
      readWorkbookRange: vi.fn().mockResolvedValue({
        cells: [],
        rows: [],
        indexingComplete: true,
        indexedThroughRow: 1,
        sheetProtection: null,
        rowBreaks: [],
        colBreaks: [],
        pageSetup,
        protectedRanges: [],
      }),
    },
  })
}

function cellsOf(body: string): HTMLElement[] {
  const page = document.createElement('div')
  page.innerHTML = body
  return Array.from(page.querySelectorAll<HTMLElement>('td, th'))
}

describe('the print layout a picture of the sheet is drawn from', () => {
  beforeEach(() => {
    vi.mocked(preloadEntireWorkbook).mockReset()
    setModuleLang('en')
    filePrints(null)
  })

  it('lays out the top-left block only, however large the sheet is', async () => {
    const sheet = await sheetPictureDocument(openSheet(5000, 200))

    expect(sheet).not.toBeNull()
    const pages = sheet!.pages.map(({ page }) => page)
    expect(Math.min(...pages.map((page) => page.rowStart))).toBe(0)
    expect(Math.min(...pages.map((page) => page.colStart))).toBe(0)
    expect(Math.max(...pages.map((page) => page.rowEnd))).toBe(99)
    expect(Math.max(...pages.map((page) => page.colEnd))).toBe(25)
  })

  it('lays out all of a sheet smaller than the block', async () => {
    const sheet = await sheetPictureDocument(openSheet(8, 3))

    expect(sheet!.pages.map(({ page }) => page)).toEqual([
      { area: 0, rowStart: 0, rowEnd: 7, colStart: 0, colEnd: 2 },
    ])
  })

  it('does not load the whole workbook first, unlike a print job', async () => {
    const sheet = await sheetPictureDocument(openSheet(5000, 200))

    expect(sheet).not.toBeNull()
    expect(preloadEntireWorkbook).not.toHaveBeenCalled()
  })

  it('keeps the grid size when the file prints the sheet small on one page', async () => {
    filePrints({ scale: 40, fitToPage: true, fitToWidth: 1, fitToHeight: 1 })

    const sheet = await sheetPictureDocument(openSheet(100, 26))

    expect(sheet!.scale).toBe(1)
    expect(sheet!.pages.length).toBeGreaterThan(1)
  })

  it('shows the row and column headings though the file does not print them', async () => {
    filePrints({ printHeadings: false })

    const sheet = await sheetPictureDocument(openSheet(8, 3))

    const headings = cellsOf(sheet!.pages[0]!.body)
      .filter((cell) => cell.tagName === 'TH')
      .map((cell) => cell.textContent)
    expect(headings).toEqual(expect.arrayContaining(['A', 'B', 'C', '1', '8']))
  })

  it('draws grid lines exactly when the grid shows them', async () => {
    const lined = (body: string): number =>
      cellsOf(body).filter((cell) => cell.tagName === 'TD' && cell.style.boxShadow !== '').length

    const shown = await sheetPictureDocument(openSheet(8, 3))
    const hidden = await sheetPictureDocument(openSheet(8, 3, true))

    expect(lined(shown!.pages[0]!.body)).toBe(24)
    expect(lined(hidden!.pages[0]!.body)).toBe(0)
  })

  it('has no picture to lay out when no sheet is open', async () => {
    const closed = { univerRef: { current: null }, lazyWorkbookRef: { current: null } }

    expect(await sheetPictureDocument(closed)).toBeNull()
  })
})
