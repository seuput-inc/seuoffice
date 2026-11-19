import { paintHtml, type WebThumbnailRequest } from '@seuoffice/ui'

import { assemblePrintHtml, type SheetPrintDocument } from './print-html'

const CSS_PX_PER_INCH = 96

/** One flat inset shadow of a computed `box-shadow`: its colour, then its x and y offsets */
const INSET_EDGE = /(rgba?\([^)]*\)) (-?[\d.]+)px (-?[\d.]+)px 0px 0px inset/g

/**
 * The print layout draws every gridline, cell border and heading edge as a flat inset shadow,
 * and WebKit paints no box shadow inside an SVG drawn as an image. Each one becomes a
 * background stripe on the same edge, which both engines paint.
 */
function edgesAsStripes(body: HTMLElement, css: string): void {
  // a cell's shadow is known only where the print rules apply to it, and they must not reach the app
  const host = document.createElement('div')
  host.style.display = 'none'
  const style = document.createElement('style')
  style.textContent = css
  host.attachShadow({ mode: 'open' }).append(style, body)
  document.body.append(host)
  try {
    for (const cell of Array.from(body.querySelectorAll<HTMLElement>('td, th'))) {
      const edges = Array.from(getComputedStyle(cell).boxShadow.matchAll(INSET_EDGE))
      if (edges.length === 0) continue
      const stripes = edges.map(([, colour, x, y]) => {
        const across = Number(x)
        const down = Number(y)
        return across !== 0
          ? {
              colour,
              size: `${Math.abs(across)}px 100%`,
              at: across > 0 ? 'left top' : 'right top',
            }
          : { colour, size: `100% ${Math.abs(down)}px`, at: down > 0 ? 'left top' : 'left bottom' }
      })
      cell.style.backgroundImage = stripes
        .map(({ colour }) => `linear-gradient(${colour},${colour})`)
        .join(',')
      cell.style.backgroundSize = stripes.map(({ size }) => size).join(',')
      cell.style.backgroundPosition = stripes.map(({ at }) => at).join(',')
      // one value per stripe: the cell's style is written out as a `background` shorthand when
      // it has a fill, and a single value is then written for the first stripe only
      cell.style.backgroundRepeat = stripes.map(() => 'no-repeat').join(',')
      cell.style.boxShadow = 'none'
    }
  } finally {
    host.remove()
  }
}

/**
 * The first page of a sheet laid out for print, on a canvas of its own: the page the Print
 * dialog previews in a frame, painted through an SVG. It shows what the print layout carries
 * (cell text, styles, merges, gridlines, headings, and the charts, pictures and shapes over
 * the cells), on white paper whatever the theme, and not the grid's own drawing: conditional
 * formats, sparklines and in-cell pictures are not in it.
 */
export async function drawFirstPage(
  sheet: SheetPrintDocument,
  { width, maxHeight, signal }: WebThumbnailRequest,
): Promise<HTMLCanvasElement> {
  const { html } = assemblePrintHtml([sheet], (page) => page === 1)
  const printed = new DOMParser().parseFromString(html, 'text/html')
  const css = Array.from(printed.querySelectorAll('style'), (style) => style.textContent).join('\n')
  // the print document's own body: its rules name it
  const body = document.adoptNode(printed.body)
  edgesAsStripes(body, css)
  const paperWidth = sheet.paper.widthIn * CSS_PX_PER_INCH
  const scale = width / paperWidth
  const height = Math.min(sheet.paper.heightIn * CSS_PX_PER_INCH, maxHeight / scale)
  return paintHtml({
    tree: body,
    css,
    width: paperWidth,
    height,
    canvasWidth: width,
    canvasHeight: Math.max(1, Math.round(height * scale)),
    signal,
  })
}
