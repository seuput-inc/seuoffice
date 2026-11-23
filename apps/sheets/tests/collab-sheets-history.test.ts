import { type ICellData, type IWorkbookData } from '@univerjs/core'
import { HistoryError, type Json } from '@seuoffice/collab/client'
import { parseAddress } from '@seuoffice/xlsx-gateway/domain/cell-address'
import {
  applyCellEditsToXlsx,
  readBasicWorkbook,
} from '@seuoffice/xlsx-gateway/gateway/xlsx-gateway'
import JSZip from 'jszip'
import { afterEach, expect, it } from 'vitest'
import { createEditJournal, toSaveEdits } from '../src/renderer/edit-journal'
import { SET_RANGE_VALUES } from '../src/web/collab-mutations'
import {
  disposeAll,
  fileOf,
  openBook,
  type OpenedBook,
  workbookData,
} from './collab/sheets-workbook'
import { buildCompatibilityFixture } from './fixture-builder'

afterEach(disposeAll)

async function openXlsx(base: Buffer): Promise<OpenedBook> {
  const { snapshot, sheetNamesById } = await readBasicWorkbook(base)
  const data: IWorkbookData = { ...workbookData(), sheets: {}, sheetOrder: [] }
  for (const sheet of snapshot.sheets) {
    const cellData: Record<number, Record<number, ICellData>> = {}
    for (const [address, cell] of Object.entries(sheet.cells)) {
      const { row, column } = parseAddress(address)
      ;(cellData[row] ??= {})[column] = cell.formula ? { f: cell.formula } : { v: cell.value }
    }
    data.sheetOrder.push(sheet.id)
    data.sheets[sheet.id] = {
      id: sheet.id,
      name: sheet.name,
      rowCount: 40,
      columnCount: 12,
      cellData,
    }
  }
  const journal = createEditJournal()
  return openBook(fileOf(data), {}, journal, async () => {
    const edits = toSaveEdits(journal).map(
      ({ sheetId, row, column, writeValue, value, formula }) => ({
        sheetName: sheetNamesById[sheetId]!,
        row,
        column,
        writeValue,
        cell: { value, formula },
      }),
    )
    return { fileName: 'history.xlsx', bytes: (await applyCellEditsToXlsx(base, edits)).buffer }
  })
}

function write(page: OpenedBook, id: string, column: number, v: string | number): Json {
  expect(
    page.edit(id, 'sheet.command.set-range-values', {
      subUnitId: 'sheet-1',
      range: { startRow: 0, endRow: 0, startColumn: column, endColumn: column },
      value: { v },
    }),
  ).toBe(true)
  return page.adapter.encode(page.emitted.at(-1)!)
}

async function reopened(page: OpenedBook) {
  const file = await page.adapter.produceFile()
  expect(file.fileName).toBe('history.xlsx')
  const archive = await JSZip.loadAsync(file.bytes)
  expect(await archive.file('customXml/item1.xml')!.async('string')).toContain('must-survive')
  const { snapshot } = await readBasicWorkbook(Buffer.from(file.bytes))
  const cells = snapshot.sheets[0]!.cells
  return [cells.A1?.value, cells.B1?.value]
}

it('reopens XLSX version 1 and appends a cell inverse to the two-user head', async () => {
  const base = await buildCompatibilityFixture()
  const alice = await openXlsx(base)
  const bob = await openXlsx(base)
  const first = write(alice, 'alice-1', 0, 'Alice')
  bob.adapter.integrate([bob.adapter.decode(first)], [])
  const second = write(bob, 'bob-2', 1, 22)
  const history = await openXlsx(base)
  const edit = history.adapter.decode(first)
  const plan = history.adapter.captureUndo!(edit)
  history.adapter.integrate([edit], [])
  expect(await reopened(history)).toEqual(['Alice', 10])
  const later = history.adapter.decode(second)
  history.adapter.integrate([later], [])
  const undo = plan([later])
  expect(undo).toHaveLength(1)
  expect(await reopened(history)).toEqual(['Alice', 22])
  bob.adapter.integrate(
    undo!.map((op) => bob.adapter.decode(history.adapter.encode(op))),
    [],
  )
  expect(await reopened(bob)).toEqual(['Old', 22])
  expect(await reopened(alice)).toEqual(['Alice', 10])
})

it('refuses a later write even when it leaves the target value unchanged', async () => {
  const base = await buildCompatibilityFixture()
  const alice = await openXlsx(base)
  const bob = await openXlsx(base)
  const first = write(alice, 'alice', 0, 'Alice')
  bob.adapter.integrate([bob.adapter.decode(first)], [])
  const other = write(bob, 'bob', 0, 'Alice')
  const history = await openXlsx(base)
  const edit = history.adapter.decode(first)
  const plan = history.adapter.captureUndo!(edit)
  const later = history.adapter.decode(other)
  history.adapter.integrate([edit, later], [])
  expect(plan([later])).toBeNull()
  expect(await reopened(history)).toEqual(['Alice', 10])
})

it('rejects formulas and structural writes instead of treating them as independent', async () => {
  const history = await openXlsx(await buildCompatibilityFixture())
  const op = (mutation: string, params: Record<string, Json>) =>
    history.adapter.decode({
      id: 'one',
      m: [[mutation, { subUnitId: 'sheet-1', ...params }]],
    })
  const literal = op(SET_RANGE_VALUES, { cellValue: { 0: { 0: { v: 'Alice' } } } })
  const formula = op(SET_RANGE_VALUES, { cellValue: { 0: { 1: { f: '=A1' } } } })
  const insert = op('sheet.mutation.insert-row', {
    range: { startRow: 0, endRow: 0, startColumn: 0, endColumn: 11, rangeType: 1 },
  })
  const plan = history.adapter.captureUndo!(literal)
  for (const unsafe of [formula, insert]) {
    expect(() => history.adapter.captureUndo!(unsafe)).toThrow(HistoryError)
    expect(() => plan([unsafe])).toThrow(HistoryError)
  }
  history.adapter.integrate([formula], [])
  expect(() => history.adapter.captureUndo!(literal)).toThrow(HistoryError)
})
