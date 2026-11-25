/**
 * Cell writes in the wire form the Sheets editor sends, made by the editor's own code path: a
 * real Univer workbook runs the command, and every mutation co-editing sends goes through
 * toWire as the co-editing adapter sends it. History tests build their ops here instead of
 * writing the JSON by hand, so a change to that path reaches them.
 */
import {
  ICommandService,
  type IWorkbookData,
  LocaleType,
  LogLevel,
  Univer,
  UniverInstanceType,
  type Workbook,
} from '@univerjs/core'
import { UniverFormulaEnginePlugin } from '@univerjs/engine-formula'
import { UniverSheetsPlugin } from '@univerjs/sheets'
import type { Json } from '@seuoffice/collab/client'
import { classify, type Params, toWire } from '../../src/web/collab-mutations'

/**
 * What the cell editor hands that command when a person types `text` into a cell without a
 * format of its own (sheets-ui `getCellDataByInput`, then `_submitEdit` with the style it took
 * from the editor document, stripped of the sheet's defaults).
 */
export const typedCell = (text: string) => ({ v: text, f: null, si: null, p: null, s: {} })

type Cells = Record<number, Record<number, object | null>>

/** The op the Sheets editor sends for one command run on a blank sheet `sheet` */
async function editorOp(id: string, sheet: string, command: string, params: object) {
  const univer = new Univer({
    logLevel: LogLevel.SILENT,
    locale: LocaleType.EN_US,
    locales: { [LocaleType.EN_US]: {} },
  })
  try {
    univer.registerPlugin(UniverFormulaEnginePlugin)
    univer.registerPlugin(UniverSheetsPlugin)
    const workbook = univer.createUnit<IWorkbookData, Workbook>(UniverInstanceType.UNIVER_SHEET, {
      id: 'history-book',
      name: 'book',
      appVersion: '',
      locale: LocaleType.EN_US,
      styles: {},
      sheetOrder: [sheet],
      sheets: {
        [sheet]: { id: sheet, name: 'Sheet1', rowCount: 1000, columnCount: 26, cellData: {} },
      },
    })
    const commands = univer.__getInjector().get(ICommandService)
    const m: Json[] = []
    commands.onMutationExecutedForCollab((info, options) => {
      if (classify(info.id, options) !== 'synced') return
      const wire = toWire(workbook, { id: info.id, params: info.params as Params })
      m.push([wire.id, wire.params as Json])
    })
    if (
      !(await commands.executeCommand(command, {
        unitId: 'history-book',
        subUnitId: sheet,
        ...params,
      }))
    )
      throw new Error(`the editor ran no ${command}`)
    return { id, m } satisfies Json
  } finally {
    univer.dispose()
  }
}

function bounds(cells: Cells) {
  const rows = Object.keys(cells).map(Number)
  const columns = rows.flatMap((row) => Object.keys(cells[row]!).map(Number))
  return {
    startRow: Math.min(...rows),
    endRow: Math.max(...rows),
    startColumn: Math.min(...columns),
    endColumn: Math.max(...columns),
  }
}

/** The op that writes `cells` (keyed by row, then column) */
export const editorCellOp = (id: string, sheet: string, cells: Cells) =>
  editorOp(id, sheet, 'sheet.command.set-range-values', { range: bounds(cells), value: cells })

/** The op the Delete key sends for the cells' range */
export const editorClearOp = (id: string, sheet: string, cells: Cells) =>
  editorOp(id, sheet, 'sheet.command.clear-selection-content', { ranges: [bounds(cells)] })
