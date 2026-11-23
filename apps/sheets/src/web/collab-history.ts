import { HistoryError } from '@seuoffice/collab/client'
import { CellValueType, type ICellData, type Workbook } from '@univerjs/core'
import type { SheetOp } from './collab-adapter'
import { SET_RANGE_VALUES } from './collab-mutations'

const unsupported = (): never => {
  throw new HistoryError('unsupported')
}

function literalType(value: unknown): CellValueType.STRING | CellValueType.NUMBER | null {
  if (typeof value === 'string') return CellValueType.STRING
  if (typeof value === 'number' && Number.isFinite(value)) return CellValueType.NUMBER
  return null
}

/** Only direct writes of one text or numeric literal have a known read/write set. */
function literalWrite(op: SheetOp) {
  const [mutation] = op.mutations
  if (op.mutations.length !== 1 || mutation?.id !== SET_RANGE_VALUES) return unsupported()
  const { subUnitId, trigger, cellValue, options } = mutation.params
  if (
    typeof subUnitId !== 'string' ||
    (trigger !== undefined && trigger !== 'sheet.command.set-range-values') ||
    options !== undefined
  ) {
    return unsupported()
  }
  // decode has already checked the matrix keys, bounds and cell field types.
  const rows = Object.entries(
    (cellValue ?? {}) as Record<string, Record<string, ICellData | null> | null>,
  )
  if (rows.length !== 1) return unsupported()
  const [row, cells] = rows[0]!
  const columns = Object.entries(cells ?? {})
  if (columns.length !== 1) return unsupported()
  const [column, cell] = columns[0]!
  if (
    !cell ||
    Object.entries(cell).some(
      ([k, value]) =>
        k !== 'v' && k !== 't' && !(value === null && (k === 'f' || k === 'si' || k === 'p')),
    )
  ) {
    return unsupported()
  }
  const type = literalType(cell.v)
  if (type === null || (cell.t !== undefined && cell.t !== type)) return unsupported()
  return { subUnitId, row: Number(row), column: Number(column), value: cell.v, type }
}

export function captureCellUndo(
  workbook: Workbook,
  op: SheetOp,
): (later: readonly SheetOp[]) => readonly SheetOp[] | null {
  const target = literalWrite(op)
  const { subUnitId, row, column } = target
  const sheet = workbook.getSheetBySheetId(subUnitId)
  if (
    !sheet ||
    row >= sheet.getRowCount() ||
    column >= sheet.getColumnCount() ||
    sheet.getMergedCell(row, column)
  ) {
    return unsupported()
  }
  const before = sheet.getCellRaw(row, column)
  const value = before?.v ?? null
  const type = literalType(value)
  if (
    (value !== null && type === null) ||
    // The file reader also exposes error and ISO-date cells as strings, without their OOXML type.
    (typeof value === 'string' && /^(?:#|\d{4}-\d{2}-\d{2})/.test(value)) ||
    (before?.t != null && before.t !== type) ||
    before?.p != null ||
    before?.custom != null ||
    before?.xf != null ||
    before?.ref != null ||
    (value === target.value && type === target.type)
  ) {
    return unsupported()
  }
  // An array formula can spill into an apparently empty cell after a later literal edit.
  for (const tab of workbook.getSheets()) {
    tab.getCellMatrix().forValue((_r, _c, cell) => {
      if (cell?.f || cell?.si || cell?.ref || cell?.xf) unsupported()
    })
  }
  // Univer mutates the cell object in place. Capture its scalar value before replay continues.
  const restore = value === null ? { v: null } : { v: value, t: type }
  return (later) => {
    for (const edit of later) {
      const next = literalWrite(edit)
      if (next.subUnitId === subUnitId && next.row === row && next.column === column) return null
    }
    const head = sheet.getCellRaw(row, column)
    if (!head || head.v !== target.value || head.t !== target.type) return null
    const cellValue = { [row]: { [column]: restore } }
    return [
      {
        id: crypto.randomUUID(),
        mutations: [{ id: SET_RANGE_VALUES, params: { subUnitId, cellValue } }],
      },
    ]
  }
}
