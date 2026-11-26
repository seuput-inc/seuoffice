/**
 * One sheet render reads each visible cell through Worksheet.getCell many times: the styles cache,
 * then every render extension (background, border, font, conditional formats, data validation,
 * the overflow lookahead) asks again, and each call runs the whole cell-content interceptor chain
 * (conditional formatting, data validation, protection, number formats). Nothing changes a cell
 * while a render draws, so within one Spreadsheet.render the first answer for a cell stands for
 * the rest. Outside a render every call goes to Univer as before.
 */
import { Worksheet } from '@univerjs/core'
import { Spreadsheet } from '@univerjs/engine-render'

const COLUMN_SPAN = 2 ** 20

let installed = false

export function installRenderCellMemo(): void {
  if (installed) return
  installed = true
  const worksheetProto = Worksheet.prototype as unknown as {
    getCell(this: Worksheet, row: number, col: number): unknown
  }
  const sheetProto = Spreadsheet.prototype as unknown as {
    render(
      this: { getSkeleton(): { worksheet?: Worksheet } | undefined },
      ...args: unknown[]
    ): unknown
  }
  const originalGetCell = worksheetProto.getCell
  const originalRender = sheetProto.render
  if (typeof originalGetCell !== 'function' || typeof originalRender !== 'function') return
  // the worksheet a render is drawing, and what it read so far
  let drawing: Worksheet | undefined
  let read = new Map<number, unknown>()
  worksheetProto.getCell = function (row, col) {
    if (this !== drawing || row < 0 || col < 0) return originalGetCell.call(this, row, col)
    const key = row * COLUMN_SPAN + col
    if (read.has(key)) return read.get(key)
    const cell = originalGetCell.call(this, row, col)
    read.set(key, cell)
    return cell
  }
  sheetProto.render = function (...args) {
    const outer = drawing
    const outerRead = read
    drawing = this.getSkeleton()?.worksheet
    read = new Map()
    try {
      return originalRender.apply(this, args)
    } finally {
      drawing = outer
      read = outerRead
    }
  }
}
