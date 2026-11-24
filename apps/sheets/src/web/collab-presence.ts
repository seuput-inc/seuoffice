import type { Json } from '@seuoffice/collab/client'
import { presenceLabel, type PresenceMark } from '@seuoffice/web-platform/presence'
import { RANGE_TYPE } from '@univerjs/core'
import { IRenderManagerService, SHEET_VIEWPORT_KEY } from '@univerjs/engine-render'
import { SheetsSelectionsService } from '@univerjs/sheets'
import { SheetSkeletonManagerService } from '@univerjs/sheets-ui'
import { z } from 'zod'
import type { UniverRuntime } from '../renderer/univer-state'

const row = z.number().int().min(0).max(1048575)
const col = z.number().int().min(0).max(16383)
const area = z.tuple([row, col, row, col]).refine(([r, c, endR, endC]) => r <= endR && c <= endC)
const location = z
  .object({ sheet: z.string(), ranges: z.array(area).min(1), active: z.tuple([row, col]) })
  .refine(({ active: [r, c], ranges }) =>
    ranges.some(([r1, c1, r2, c2]) => r1 <= r && c1 <= c && r2 >= r && c2 >= c),
  )
type Area = z.infer<typeof area>

const PANES = [
  SHEET_VIEWPORT_KEY.VIEW_MAIN_LEFT_TOP,
  SHEET_VIEWPORT_KEY.VIEW_MAIN_TOP,
  SHEET_VIEWPORT_KEY.VIEW_MAIN_LEFT,
  SHEET_VIEWPORT_KEY.VIEW_MAIN,
]

export function sheetsPresence({ runtime }: { runtime: UniverRuntime }, publish: () => void) {
  const workbook = runtime.univerAPI.getActiveWorkbook()!
  const injector = runtime.univer.__getInjector()
  const selections = injector.get(SheetsSelectionsService).getWorkbookSelections(workbook.getId())
  const render = injector.get(IRenderManagerService).getRenderById(workbook.getId())!
  const layer = document.createElement('div')
  // A sibling of the grid canvas shares its CSS origin and any outer zoom or scroll.
  layer.style.cssText = 'position:absolute;inset:0;overflow:hidden;pointer-events:none;z-index:9'
  const canvas = render.engine.getCanvasElement()

  let peers: { peer: PresenceMark; at: z.infer<typeof location> }[] = []
  const paint = (): void => {
    if (!peers.length) {
      layer.remove()
      layer.replaceChildren()
      return
    }
    if (canvas.parentElement !== layer.parentElement) canvas.after(layer)
    const sheet = workbook.getActiveSheet()
    const skeleton = render.with(SheetSkeletonManagerService).getCurrentSkeleton()
    let used = 0
    if (sheet && skeleton?.worksheet.getSheetId() === sheet.getSheetId()) {
      const { scaleX, scaleY } = render.scene.getAncestorScale()
      const draw = (peer: PresenceMark, [r, c, endR, endC]: Area, active: boolean): void => {
        if (endR >= skeleton.getRowCount() || endC >= skeleton.getColumnCount()) return
        const first = skeleton.getNoMergeCellWithCoordByIndex(r, c)
        const last = skeleton.getNoMergeCellWithCoordByIndex(endR, endC)
        let labelled = false
        for (const key of PANES) {
          const pane = render.scene.getViewport(key)
          if (!pane?.isActive) continue
          const x = (Math.min(first.startX, last.startX) - pane.viewportScrollX) * scaleX
          const right = (Math.max(first.endX, last.endX) - pane.viewportScrollX) * scaleX
          const y = (first.startY - pane.viewportScrollY) * scaleY
          const bottom = (last.endY - pane.viewportScrollY) * scaleY
          const left = Math.max(x, pane.left)
          const top = Math.max(y, pane.top)
          const paneRight = pane.left + (pane.width ?? 0)
          const paneBottom = pane.top + (pane.height ?? 0)
          const w = Math.min(right, paneRight) - left
          const h = Math.min(bottom, paneBottom) - top
          if (w <= 0 || h <= 0) continue
          const box =
            (layer.children.item(used++) as HTMLDivElement | null) ?? document.createElement('div')
          if (!box.parentNode) layer.append(box)
          box.className = active ? 'sheet-presence-active' : 'sheet-presence-range'
          const stroke = active ? 1 : 2
          Object.assign(box.style, {
            position: 'absolute',
            boxSizing: 'border-box',
            left: `${left}px`,
            top: `${top}px`,
            width: `${w}px`,
            height: `${h}px`,
            borderStyle: 'solid',
            borderColor: peer.color,
            borderWidth: `${y >= top ? stroke : 0}px ${right <= paneRight ? stroke : 0}px ${bottom <= paneBottom ? stroke : 0}px ${x >= left ? stroke : 0}px`,
          })
          if (!active && !labelled) {
            let label = box.firstElementChild as HTMLElement | null
            if (label?.dataset.presenceClient !== peer.client || label.textContent !== peer.name) {
              label = presenceLabel(peer)
              label.style.inset = '0 auto auto 0'
              box.replaceChildren(label)
            }
            label.style.maxWidth = `min(16em, ${Math.max(0, paneRight - left - stroke)}px)`
            const roomAbove = top - pane.top
            const above = roomAbove >= 18
            label.style.transform = above ? 'translateY(-100%)' : ''
            label.style.maxHeight = `${above ? roomAbove : Math.max(0, paneBottom - top - stroke)}px`
            labelled = true
          } else if (box.firstChild) box.replaceChildren()
        }
      }
      for (const { peer, at } of peers) {
        if (sheet.getSheetId() !== at.sheet) continue
        for (const range of at.ranges) draw(peer, range, false)
        const cell = skeleton.worksheet.getCellInfoInMergeData(...at.active)
        const active: Area = [cell.startRow, cell.startColumn, cell.endRow, cell.endColumn]
        if (!at.ranges.some((range) => range.every((n, i) => n === active[i]))) {
          draw(peer, active, true)
        }
      }
    }
    while (layer.children.length > used) layer.lastElementChild!.remove()
  }

  render.scene.afterRender$.subscribe(() => {
    if (peers.length) paint()
  })
  selections.selectionMoveStart$.subscribe(publish)
  selections.selectionMoving$.subscribe(publish)
  selections.selectionChanged$.subscribe(publish)
  runtime.univerAPI.addEvent(runtime.univerAPI.Event.ActiveSheetChanged, publish)
  return {
    read(): Json {
      const sheet = workbook.getActiveSheet()
      const id = sheet.getSheetId()
      const current = selections.getSelectionsOfWorksheet(id)
      const active = selections.getCurrentLastSelection()?.primary
      if (!active || !current.length) return null
      const ranges = current.map(({ range }) => {
        const allRows = range.rangeType === RANGE_TYPE.COLUMN || range.rangeType === RANGE_TYPE.ALL
        const allCols = range.rangeType === RANGE_TYPE.ROW || range.rangeType === RANGE_TYPE.ALL
        return [
          allRows ? 0 : range.startRow,
          allCols ? 0 : range.startColumn,
          allRows ? sheet.getMaxRows() - 1 : range.endRow,
          allCols ? sheet.getMaxColumns() - 1 : range.endColumn,
        ]
      })
      return {
        sheet: id,
        ranges,
        active: [active.actualRow, active.actualColumn],
      }
    },
    draw(marks: readonly PresenceMark[]): void {
      peers = marks.flatMap((peer) => {
        const parsed = location.safeParse(peer.at)
        return parsed.success ? [{ peer, at: parsed.data }] : []
      })
      paint()
    },
  }
}
