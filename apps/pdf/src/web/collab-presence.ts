import type { Json } from '@seuoffice/collab/client'
import { presenceLabel, type PresenceMark } from '@seuoffice/web-platform/presence'
import { pdfRectToCss, viewToPdf, type PageGeom } from '../renderer/annotations'

type Rect = [number, number, number, number]
type Point = [page: number, layer: 'pdf' | 'ocr', run: number, offset: number]
export type PdfObject =
  | { kind: 'annotation'; id: string }
  | { kind: 'markup'; id: string }
  | { kind: 'savedMarkup'; page: number; id: number }
  | { kind: 'fields'; ids: string[] }
type ModelObject = Exclude<PdfObject, { kind: 'annotation' }>
type PdfAt =
  | PdfObject
  | { kind: 'text'; from: Point; to: Point }
  | { kind: 'draft'; page: number; caret: boolean; rects: Rect[] }

export interface PdfPresenceHost {
  presenceLayout: { current: (() => void) | null }
  pageGeom(page: number): PageGeom
  selectedObject(): PdfObject | null
  rectsFor(at: ModelObject): { page: number; rect: Rect }[]
}

const index = (v: Json | undefined): v is number =>
  typeof v === 'number' && Number.isSafeInteger(v) && v >= 0
const id = (v: Json | undefined): v is string => typeof v === 'string' && v.length > 0
const point = (v: Json | undefined): v is Point =>
  Array.isArray(v) &&
  v.length === 4 &&
  index(v[0]) &&
  (v[1] === 'pdf' || v[1] === 'ocr') &&
  index(v[2]) &&
  index(v[3])
const rect = (v: Json): v is Rect =>
  Array.isArray(v) && v.length === 4 && v.every((n) => typeof n === 'number' && Number.isFinite(n))

function parseAt(v: Json): PdfAt | null {
  if (!v || typeof v !== 'object' || Array.isArray(v)) return null
  switch (v.kind) {
    case 'annotation':
    case 'markup':
      return id(v.id) ? { kind: v.kind, id: v.id } : null
    case 'savedMarkup':
      return index(v.page) && index(v.id) ? { kind: v.kind, page: v.page, id: v.id } : null
    case 'fields':
      return Array.isArray(v.ids) && v.ids.every(id) ? { kind: v.kind, ids: v.ids } : null
    case 'text':
      return point(v.from) && point(v.to) ? { kind: v.kind, from: v.from, to: v.to } : null
    case 'draft':
      return index(v.page) &&
        typeof v.caret === 'boolean' &&
        Array.isArray(v.rects) &&
        v.rects.every(rect)
        ? { kind: v.kind, page: v.page, caret: v.caret, rects: v.rects }
        : null
    default:
      return null
  }
}

const pageOf = (el: Element) => el.closest<HTMLElement>('.pdf-page[data-page]')
const scaleOf = (page: HTMLElement) => Number(page.style.getPropertyValue('--scale-factor'))

function textPoint(node: Node, offset: number): Point | null {
  const layer = (node instanceof Element ? node : node.parentElement)?.closest(
    '.textLayer, .pdf-ocr-layer',
  )
  const page = layer && pageOf(layer)
  if (!layer || !page) return null
  const which = layer.matches('.textLayer') ? 'pdf' : 'ocr'
  const walk = document.createTreeWalker(layer, NodeFilter.SHOW_TEXT)
  const boundary = document.createRange()
  boundary.setStart(node, offset)
  boundary.collapse(true)
  let last: Point | null = null
  let run = 0
  for (let n = walk.nextNode(); n; n = walk.nextNode(), run++) {
    if (n === node) return [Number(page.dataset.page), which, run, offset]
    if (boundary.comparePoint(n, 0) > 0) return [Number(page.dataset.page), which, run, 0]
    last = [Number(page.dataset.page), which, run, (n as Text).length]
  }
  return last
}

/** Textareas expose character offsets, but no Range. Lay out the same text locally; only its PDF coordinates travel. */
function draftAt(input: HTMLTextAreaElement, host: PdfPresenceHost): PdfAt | null {
  const page = pageOf(input)
  if (!page || !input.parentElement) return null
  const mirror = document.createElement('div')
  mirror.className = input.className
  mirror.style.cssText = input.style.cssText
  const css = getComputedStyle(input)
  Object.assign(mirror.style, {
    position: 'absolute',
    left: `${input.offsetLeft}px`,
    top: `${input.offsetTop}px`,
    visibility: 'hidden',
    pointerEvents: 'none',
    fontFamily: css.fontFamily,
    fontSize: css.fontSize,
    fontWeight: css.fontWeight,
    fontStyle: css.fontStyle,
    lineHeight: css.lineHeight,
    letterSpacing: css.letterSpacing,
    textAlign: css.textAlign,
    direction: css.direction,
    overflowWrap: css.overflowWrap,
    tabSize: css.tabSize,
    width: css.width,
    height: css.height,
    boxSizing: css.boxSizing,
    padding: css.padding,
    border: css.border,
    whiteSpace: css.whiteSpace,
    wordBreak: css.wordBreak,
    overflow: css.overflow,
  })
  const node = document.createTextNode(input.value + '\u200b')
  mirror.append(node)
  input.parentElement.append(mirror)
  try {
    mirror.scrollLeft = input.scrollLeft
    mirror.scrollTop = input.scrollTop
    const r = document.createRange()
    r.setStart(node, input.selectionStart)
    r.setEnd(node, input.selectionEnd)
    const origin = page.getBoundingClientRect()
    const field = input.getBoundingClientRect()
    const i = Number(page.dataset.page)
    const g = host.pageGeom(i)
    const scale = scaleOf(page)
    const rects: Rect[] = []
    for (const b of r.getClientRects()) {
      const left = Math.max(b.left, field.left),
        right = Math.min(b.right, field.right)
      const top = Math.max(b.top, field.top),
        bottom = Math.min(b.bottom, field.bottom)
      if (left > right || top >= bottom) continue
      const a = viewToPdf(g, (left - origin.left) / scale, (top - origin.top) / scale)
      const z = viewToPdf(g, (right - origin.left) / scale, (bottom - origin.top) / scale)
      rects.push([+a[0].toFixed(2), +a[1].toFixed(2), +z[0].toFixed(2), +z[1].toFixed(2)])
    }
    return rects.length ? { kind: 'draft', page: i, caret: r.collapsed, rects } : null
  } finally {
    mirror.remove()
  }
}

type Box = { left: number; top: number; width: number; height: number }
type Paint = (page: HTMLElement, box: Box, kind: 'outline' | 'range' | 'caret') => void
const localBox = (b: DOMRect, origin: DOMRect): Box => ({
  left: b.left - origin.left,
  top: b.top - origin.top,
  width: b.width,
  height: b.height,
})

function paintText(from: Point, to: Point, paint: Paint): void {
  // PdfPage unmounts offscreen text. Never guess an endpoint in a missing or partially rendered layer.
  const layers = [
    ...document.querySelectorAll(
      '.pdf-page .textLayer[data-presence-ready], .pdf-page .pdf-ocr-layer',
    ),
  ]
  const resolve = ([page, which, run, offset]: Point) => {
    const i = layers.findIndex(
      (el) =>
        Number(pageOf(el)?.dataset.page) === page &&
        el.matches(which === 'pdf' ? '.textLayer' : '.pdf-ocr-layer'),
    )
    if (i < 0) return -1
    const walk = document.createTreeWalker(layers[i]!, NodeFilter.SHOW_TEXT)
    let j = 0
    for (let n = walk.nextNode(); n; n = walk.nextNode(), j++)
      if (j === run) return offset <= (n as Text).length ? i : -1
    return -1
  }
  const first = resolve(from)
  const last = resolve(to)
  if (first < 0 || last < first) return
  if (first === last && (from[2] > to[2] || (from[2] === to[2] && from[3] > to[3]))) return
  const caret = first === last && from[2] === to[2] && from[3] === to[3]
  for (let i = first; i <= last; i++) {
    const layer = layers[i]!
    const page = pageOf(layer)!
    const start = i === first ? from[2] : 0,
      end = i === last ? to[2] : Infinity
    const walk = document.createTreeWalker(layer, NodeFilter.SHOW_TEXT)
    const origin = page.getBoundingClientRect()
    let run = 0
    for (let n = walk.nextNode(); n && run <= end; n = walk.nextNode(), run++) {
      if (run < start) continue
      const node = n as Text
      const a = i === first && run === start ? from[3] : 0
      const b = i === last && run === end ? to[3] : node.length
      if (a <= node.length && b <= node.length && (a < b || (caret && a === b))) {
        const r = document.createRange()
        r.setStart(node, a)
        r.setEnd(node, b)
        for (const box of r.getClientRects())
          if (box.height) paint(page, localBox(box, origin), caret ? 'caret' : 'range')
        if (caret) break
      }
    }
  }
}

export function pdfPresence(host: PdfPresenceHost, publish: () => void) {
  let field: string | null = null
  let peers: { peer: PresenceMark; at: PdfAt }[] = []
  const painted: HTMLElement[] = []
  const read = (): PdfAt | null => {
    const draft = document.activeElement
      ?.closest('.pdf-textedit-editor')
      ?.querySelector<HTMLTextAreaElement>('textarea.pdf-textedit-input')
    if (draft) return draftAt(draft, host)
    const selected = host.selectedObject()
    if (field) {
      const at: Extract<PdfObject, { kind: 'fields' }> = { kind: 'fields', ids: [field] }
      return host.rectsFor(at).length ? at : null
    }
    const sel = document.getSelection()
    if (sel?.rangeCount && (!sel.isCollapsed || !selected)) {
      const r = sel.getRangeAt(0)
      const from = textPoint(r.startContainer, r.startOffset),
        to = textPoint(r.endContainer, r.endOffset)
      if (from && to) return { kind: 'text', from, to }
    }
    return selected
  }
  const repaint = () => {
    for (const el of painted) el.remove()
    painted.length = 0
    for (const { peer, at } of peers) {
      let labelled = false
      const paint: Paint = (page, b, kind) => {
        const x = Math.max(0, b.left),
          y = Math.max(0, b.top)
        const w = Math.min(page.clientWidth, b.left + b.width) - x
        const h = Math.min(page.clientHeight, b.top + b.height) - y
        if (!(w >= 0 && h >= 0)) return
        const el = document.createElement('div')
        el.className = `pdf-presence-${kind}`
        el.style.setProperty('--peer-color', peer.color)
        Object.assign(el.style, {
          left: `${x}px`,
          top: `${y}px`,
          width: `${Math.max(kind === 'caret' ? 2 : 1, w)}px`,
          height: `${Math.max(1, h)}px`,
        })
        if (!labelled) {
          const label = presenceLabel(peer)
          Object.assign(label.style, { left: '0', top: 'auto', bottom: '100%' })
          el.append(label)
          labelled = true
        }
        page.append(el)
        painted.push(el)
      }
      if (at.kind === 'text') paintText(at.from, at.to, paint)
      else if (at.kind === 'annotation') {
        const el = document.querySelector(`.pdf-page [data-pdf-annotation="${CSS.escape(at.id)}"]`)
        const page = el && pageOf(el)
        if (el && page)
          paint(page, localBox(el.getBoundingClientRect(), page.getBoundingClientRect()), 'outline')
      } else {
        const boxes =
          at.kind === 'draft'
            ? at.rects.map((rect) => ({ page: at.page, rect }))
            : host.rectsFor(at)
        const kind = at.kind !== 'draft' ? 'outline' : at.caret ? 'caret' : 'range'
        for (const box of boxes) {
          const page = document.querySelector<HTMLElement>(`.pdf-page[data-page="${box.page}"]`)
          if (page)
            paint(page, pdfRectToCss(host.pageGeom(box.page), box.rect, scaleOf(page)), kind)
        }
      }
    }
  }
  let frame = 0
  let layoutChanged = false
  const update = () => {
    if (frame) return
    frame = requestAnimationFrame(() => {
      frame = 0
      if (layoutChanged) {
        layoutChanged = false
        repaint()
      }
      publish()
    })
  }
  const target = (e: Event) => {
    if (!(e.target instanceof Element)) return
    const box = e.target.closest<HTMLElement>('[data-field-id]')
    if (box && pageOf(box)) {
      const design =
        box.matches('.pdf-formdesign-field') && !box.closest('[data-pdf-collab="view"]')
      field = design ? null : (box.dataset.fieldId ?? null)
    } else if (!e.target.closest('.pdf-formdesign-props')) field = null
    update()
  }
  document.addEventListener('pointerdown', target, true)
  document.addEventListener('focusin', target, true)
  document.addEventListener(
    'keydown',
    (e) => {
      if (e.key === 'Escape') {
        field = null
        update()
      }
    },
    true,
  )
  document.addEventListener('selectionchange', update, true)
  document.addEventListener('select', update, true)
  document.addEventListener('input', update, true)
  host.presenceLayout.current = () => {
    layoutChanged = true
    update()
  }
  document.addEventListener('pdf-text-layer-change', host.presenceLayout.current)
  return {
    read,
    draw(marks: readonly PresenceMark[]) {
      peers = marks.flatMap((peer) => {
        const at = parseAt(peer.at)
        return at ? [{ peer, at }] : []
      })
      repaint()
    },
  }
}
