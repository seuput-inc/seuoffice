import { fontFamilyKey, pageCss, paintHtml, type WebThumbnailRequest } from '@seuoffice/ui'
import { THUMB_PAGE_CLASS, fillThumbPage, snapshotPages } from './components/NavPageThumbs'
import { canvasScaleOf } from './editor/canvas-scale'
import type { PageCut } from './pagination-cuts'

/** What a box above the page may keep: it stands in the tree only so the page's rules still match */
const SHELL_STYLE = [
  'display:block',
  'position:static',
  'float:none',
  'margin:0',
  'padding:0',
  'border:0',
  'width:auto',
  'height:auto',
  'min-width:0',
  'min-height:0',
  'max-width:none',
  'max-height:none',
  'overflow:visible',
  'transform:none',
  'zoom:1',
  'columns:auto',
  'background:none',
  'box-shadow:none',
  'opacity:1',
  'filter:none',
]
  .map((declaration) => `${declaration}!important`)
  .join(';')

/** The dark page is a way of looking at the document on screen, not the document */
const DARK_PAGE_CLASS = 'page-dark'

/** The custom properties an element sets on itself: the page's geometry is handed down in them */
function ownVariables(element: HTMLElement): string {
  return (element.getAttribute('style') ?? '')
    .split(';')
    .filter((declaration) => declaration.trim().startsWith('--'))
    .join(';')
}

/**
 * `page` inside boxes named like the ones the editor's page sits in, so a rule that names one
 * of them finds the page as it does on screen. The boxes themselves take no room.
 */
function insideEditorBoxes(pm: HTMLElement, page: HTMLElement): HTMLElement {
  let tree = page
  for (let box = pm.parentElement; box && box !== document.body; box = box.parentElement) {
    const shell = document.createElement('div')
    shell.className = Array.from(box.classList)
      .filter((name) => name !== DARK_PAGE_CLASS)
      .join(' ')
    shell.setAttribute('style', `${SHELL_STYLE};${ownVariables(box)}`)
    shell.appendChild(tree)
    tree = shell
  }
  // what the page inherits from the document's root, which no box above stands for
  const root = document.createElement('div')
  const inherited = getComputedStyle(document.body)
  root.setAttribute('style', `font:${inherited.font};color:${inherited.color}`)
  root.lang = document.documentElement.lang
  root.dir = document.documentElement.dir
  root.appendChild(tree)
  return root
}

/** The families the text of `blocks` is set in, as the canvas computes them, in reading order */
function familiesOf(blocks: readonly HTMLElement[]): Set<string> {
  const lists = new Set<string>()
  for (const block of blocks) {
    for (const element of [block, ...Array.from(block.querySelectorAll('*'))]) {
      lists.add(getComputedStyle(element).fontFamily)
    }
  }
  return new Set(Array.from(lists).flatMap((list) => list.split(',').map(fontFamilyKey)))
}

/**
 * The first page as the editor lays it out, edits included, on a canvas of its own: the blocks
 * of that page are copied the way the Pages tab copies them and painted through an SVG. `pm` is
 * the editor's page element, `cuts` the page cuts of the last pagination pass.
 */
export async function drawFirstPage(
  pm: HTMLElement,
  cuts: readonly PageCut[] | undefined,
  { width, maxHeight, signal }: WebThumbnailRequest,
): Promise<HTMLCanvasElement> {
  // the scale the canvas is shown at now: a request waits for the document, and the zoom
  // the App had when it came may have been refitted since
  const zoom = canvasScaleOf(pm.closest<HTMLElement>('.doc-zoom'), 1)
  const snap = snapshotPages(pm, zoom, cuts)
  const first = snap?.pages[0]
  if (!snap || !first) throw new Error('the document has no page laid out')
  const scale = width / snap.pageW
  const height = Math.min(first.height, maxHeight / scale)
  const page = document.createElement('div')
  page.className = THUMB_PAGE_CLASS
  page.setAttribute('style', `width:${snap.pageW}px;height:${first.height}px;${snap.vars}`)
  const blocks = fillThumbPage(page, snap, first)
  const css = await pageCss({ families: familiesOf(blocks), text: page.textContent, signal })
  return paintHtml({
    tree: insideEditorBoxes(pm, page),
    css,
    width: snap.pageW,
    height,
    canvasWidth: width,
    canvasHeight: Math.max(1, Math.round(height * scale)),
    signal,
  })
}
