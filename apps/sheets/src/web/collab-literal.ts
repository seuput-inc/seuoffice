/**
 * One cell of a `set-range-values` that writes a single literal, read the way
 * Univer stores it. The page's selective undo (collab-history.ts) and the
 * history renderer (packages/history-renderer) both decide with this, so the
 * two accept exactly the same wire cells.
 */
import { HistoryError, type Json } from '@seuoffice/collab/client'

/** A literal as Univer stores it: its value, and its CellValueType when the wire names one */
export interface LiteralCell {
  /** null: the value is cleared */
  v: string | number | null
  /** 1 STRING, 2 NUMBER, 3 BOOLEAN (stored as 1 or 0), 4 FORCE_STRING */
  t?: 1 | 2 | 3 | 4
  /** a null cell: Univer deletes it outright, its format included */
  whole?: true
}

function unsupported(detail: string): never {
  throw new HistoryError('unsupported', detail)
}

function record(value: Json | undefined): value is Record<string, Json> {
  return !!value && typeof value === 'object' && !Array.isArray(value)
}

/**
 * The cell the editor sends for typed input (toWire: the value and type Univer
 * stored, an empty style object, nulled formula and rich text), or refuses it
 * with the reason. A cell without a type is a literal only when Univer cannot
 * infer a number or boolean from it: that inference depends on the cell's
 * format, which the wire does not carry.
 */
export function literalCell(cell: Json | undefined): LiteralCell {
  if (cell === null) return { v: null, whole: true }
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
      // Univer merges `s` into the cell's format; an empty object changes nothing.
      unsupported(`Sheets cell format change: ${Object.keys(value).sort().join(' ')}`)
    }
  }
  const { v, t } = cell
  if (v === null) {
    if (t !== undefined) unsupported('typed empty Sheets value')
    return { v: null }
  }
  if (t === 1 || t === 4) {
    if (typeof v === 'string' || typeof v === 'number') return { v: String(v), t }
  } else if (t === 2) {
    const value = typeof v === 'number' ? v : typeof v === 'string' && v.trim() ? Number(v) : NaN
    if (Number.isFinite(value)) return { v: value, t }
  } else if (t === 3) {
    if (v === true || v === 1 || (typeof v === 'string' && /^(?:true|1)$/i.test(v)))
      return { v: 1, t }
    if (v === false || v === 0 || (typeof v === 'string' && /^(?:false|0)$/i.test(v)))
      return { v: 0, t }
  } else if (t === undefined) {
    if (typeof v === 'number') return { v }
    if (typeof v === 'string') {
      if ((v.trim() === '' || Number.isNaN(Number(v))) && !/^(true|false)$/i.test(v)) return { v }
      return unsupported('ambiguous Sheets text type')
    }
  }
  return unsupported('unsupported Sheets value')
}
