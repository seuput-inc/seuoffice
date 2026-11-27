import type { FixtureName } from './bench-xlsx-fixtures'

// B2 in the released 1440x900 view at scale 1. A layout change needs new image evidence.
const b2: Partial<Record<FixtureName, string>> = { small: '627.07', large: '720.23', xl: '689.77' }

export interface CellPixels {
  text: string
  x: number
  y: number
  width: number
  height: number
}

/** A word read from screenshot pixels, not the workbook canvas or the DOM. */
export function visibleCell(tsv: string, fixture: FixtureName, drawn: string): CellPixels | null {
  const expected = b2[fixture]
  if (expected !== undefined && drawn !== expected) return null
  for (const line of tsv.split(/\r?\n/)) {
    const [level, , , , , , left, top, w, h, , text] = line.split('\t')
    if (level !== '5' || text !== drawn) continue
    const [x, y, width, height] = [Number(left), Number(top), Number(w), Number(h)] as const
    if (!(width > 0 && height > 0)) continue
    const inCell =
      expected !== undefined
        ? x >= 116 && x + width <= 190 && y >= 69 && y + height <= 89
        : x >= 46 && x + width <= 1440 && y >= 48 && y + height <= 850
    if (inCell) return { text, x, y, width, height }
  }
  return null
}
