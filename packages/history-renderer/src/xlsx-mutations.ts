import { HistoryError, type Json } from '@seuoffice/collab/client'
import { formatAddress, parseAddress } from '@seuoffice/xlsx-gateway/domain/cell-address'
import { checkWireMutation } from '../../../apps/sheets/src/web/collab-wire-check'

const setValues = 'sheet.mutation.set-range-values'
const gridlines = 'sheet.mutation.toggle-gridlines'
const direction = 'sheet.mutation.set-worksheet-right-to-left'

export function invalid(detail: string): never {
  throw new HistoryError('invalid_history', detail)
}
export function unsupported(detail: string): never {
  throw new HistoryError('unsupported', detail)
}
export type Literal = string | number | boolean | null
// Keys are canonical A1 addresses or a named sheet-view property (neither is an address).
// `whole`: Univer deletes the cell outright, its format included.
export type Write = { sheet: string; key: string; value: Literal; whole?: true }
export type SheetOp = { id: string; writes: Write[] }

export function checkText(text: string) {
  if (text.length > 32767 || /\r|\p{Cs}/u.test(text)) unsupported('unsupported XLSX literal text')
  return text
}

function record(value: Json | undefined): value is Record<string, Json> {
  return !!value && typeof value === 'object' && !Array.isArray(value)
}

function literal(cell: Json): Pick<Write, 'value' | 'whole'> {
  if (cell === null) return { value: null, whole: true }
  if (!record(cell)) return unsupported('not a Sheets cell')
  for (const [key, value] of Object.entries(cell)) {
    if (key === 'v' || key === 't') continue
    if (key === 'f' || key === 'si' || key === 'p' || key === 'custom') {
      if (value !== null)
        unsupported(
          key === 'p'
            ? 'Sheets rich text is not supported'
            : key === 'custom'
              ? 'Sheets custom cell data is not supported'
              : 'Sheets formula write',
        )
    } else if (key !== 's') unsupported(`unknown Sheets cell field ${key}`)
    else if (!record(value)) {
      // null removes the cell's format; a style id never travels (toWire sends the object).
      unsupported('Sheets cell format removal or style id')
    } else if (Object.keys(value).length) {
      // Univer merges `s` into the cell's format. Writing a format into styles.xml is not modelled.
      unsupported(`Sheets cell format change: ${Object.keys(value).sort().join(' ')}`)
    }
  }
  const { v, t } = cell
  if (v === null) {
    if (t !== undefined) unsupported('typed empty Sheets value')
    return { value: null }
  }
  // Univer's CellValueType: what the editor applied (toWire sends it) decides the value.
  if (t === 1 || t === 4) {
    if (typeof v === 'string') return { value: checkText(v) }
    if (typeof v === 'number') return { value: String(v) }
  } else if (t === 2) {
    const value = typeof v === 'number' ? v : typeof v === 'string' && v.trim() ? Number(v) : NaN
    if (Number.isFinite(value)) return { value }
  } else if (t === 3) {
    if (v === true || v === 1 || (typeof v === 'string' && /^(?:true|1)$/i.test(v)))
      return { value: true }
    if (v === false || v === 0 || (typeof v === 'string' && /^(?:false|0)$/i.test(v)))
      return { value: false }
  } else if (t === undefined) {
    if (typeof v === 'number') return { value: v }
    // Older editors sent no type; Univer then infers one from state this log does not hold.
    if (
      typeof v === 'string' &&
      (v.trim() === '' || Number.isNaN(Number(v))) &&
      !/^(true|false)$/i.test(v)
    )
      return { value: checkText(v) }
    if (typeof v === 'string') return unsupported('ambiguous Sheets text type')
  }
  return unsupported('unsupported Sheets value')
}

export function decode(json: Json): SheetOp {
  if (!record(json) || typeof json.id !== 'string' || !Array.isArray(json.m))
    return invalid('invalid Sheets op')
  if (Object.keys(json).some((key) => key !== 'id' && key !== 'm'))
    return unsupported('unexpected Sheets op field')
  const writes: Write[] = []
  for (const mutation of json.m) {
    if (!Array.isArray(mutation) || mutation.length !== 2) return invalid('invalid Sheets mutation')
    const [kind, params] = mutation
    if (typeof kind !== 'string' || !record(params)) return invalid('invalid Sheets mutation')
    if (kind !== setValues && kind !== gridlines && kind !== direction)
      return unsupported('unsupported Sheets mutation')
    checkWireMutation(kind, params)
    const { subUnitId: sheet, trigger, cellValue } = params
    if (typeof sheet !== 'string') return invalid('missing Sheets subUnitId')
    // A typed value or the Delete key (whose cells keep their format).
    if (
      trigger !== undefined &&
      trigger !== kind.replace('.mutation.', '.command.') &&
      !(kind === setValues && trigger === 'sheet.command.clear-selection-content')
    )
      return unsupported(`Sheets mutation from ${String(trigger).slice(0, 80)}`)
    if (kind !== setValues) {
      const key = kind === gridlines ? 'showGridlines' : 'rightToLeft'
      const value = params[key]
      if (value !== 0 && value !== 1) return unsupported('unspecified Sheets view setting')
      writes.push({ sheet, key, value })
      continue
    }
    if (params.options !== undefined || !record(cellValue))
      return unsupported('not a direct Sheets write')
    const start = writes.length
    for (const [row, columns] of Object.entries(cellValue)) {
      if (!record(columns)) return unsupported('not a Sheets row of literals')
      for (const [column, cell] of Object.entries(columns)) {
        if (+row >= 1000 || +column >= 26) return unsupported('outside the XLSX history grid')
        writes.push({ sheet, key: formatAddress(+row, +column), ...literal(cell) })
      }
    }
    if (writes.length === start) return unsupported('empty Sheets write')
  }
  if (!writes.length) return unsupported('empty Sheets mutation')
  return { id: json.id, writes }
}

export function encode({ id, writes }: SheetOp): Json {
  const m: Json[] = []
  const matrices = new Map<string, Record<string, Record<string, Json>>>()
  // Compensation has one write per distinct key. A sparse matrix per sheet fits the peer wire budget.
  for (const { sheet, key, value } of writes) {
    if (key === 'showGridlines' || key === 'rightToLeft') {
      m.push([key === 'showGridlines' ? gridlines : direction, { subUnitId: sheet, [key]: value }])
      continue
    }
    let matrix = matrices.get(sheet)
    if (!matrix) {
      matrix = {}
      matrices.set(sheet, matrix)
      m.push([setValues, { subUnitId: sheet, cellValue: matrix }])
    }
    const { row, column } = parseAddress(key)
    const cells = (matrix[row] ??= {})
    cells[column] =
      value === null
        ? { v: null }
        : typeof value === 'boolean'
          ? { v: value ? 1 : 0, t: 3 }
          : { v: value, t: typeof value === 'string' ? 1 : 2 }
  }
  return { id, m }
}
