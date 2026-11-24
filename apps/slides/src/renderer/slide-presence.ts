import type { Json } from '@seuoffice/collab/client'
import type { PresenceMark } from '@seuoffice/web-platform/presence'

type Point = [paragraph: number, offset: number]
export type TextSelection = { start: Point; end: Point; body: number }
type Location = { slide: string; group: string | null; ids: string[]; text: TextSelection | null }
export type SlidePeer = Omit<PresenceMark, 'at'> & { at: Location }

const record = (v: Json): v is Record<string, Json> =>
  v !== null && typeof v === 'object' && !Array.isArray(v)
const id = (v: unknown): v is string => typeof v === 'string' && v.length > 0 && v.length <= 256
const uint = (v: unknown): v is number => typeof v === 'number' && Number.isSafeInteger(v) && v >= 0
const point = (v: Json | undefined): v is Point =>
  Array.isArray(v) && v.length === 2 && v.every(uint)

function parse(at: Json): Location | null {
  if (!record(at)) return null
  const { slide, group, ids, text } = at
  if (
    !id(slide) ||
    !(group === null || id(group)) ||
    !Array.isArray(ids) ||
    !ids.length ||
    !ids.every(id)
  )
    return null
  if (text === null) return { slide, group, ids, text }
  if (
    !text ||
    !record(text) ||
    ids.length !== 1 ||
    !uint(text.body) ||
    !point(text.start) ||
    !point(text.end)
  )
    return null
  const { start, end, body } = text
  if (end[0] < start[0] || (end[0] === start[0] && end[1] < start[1])) return null
  return { slide, group, ids, text: { start, end, body } }
}

/** Only the currently displayed slide owns read(); the page can draw before the slide mounts. */
export class SlidePresence {
  constructor(readonly publish: () => void) {}
  shownSlide?: string
  read: () => Location | null = () => null
  private peers: SlidePeer[] = []
  private listeners = new Set<() => void>()
  snapshot = () => this.peers
  subscribe = (listener: () => void) => {
    this.listeners.add(listener)
    return () => {
      this.listeners.delete(listener)
    }
  }
  draw(marks: readonly PresenceMark[]) {
    this.peers = marks.flatMap((p) => {
      const at = parse(p.at)
      return at ? [{ ...p, at }] : []
    })
    for (const notify of this.listeners) notify()
  }
}

/** The version can advance before React paints the replacement box. Never apply old offsets to new text. */
export function bodyKey(root: HTMLElement): number {
  let h = 2166136261
  for (const p of root.children) {
    const text = p.textContent ?? ''
    for (let i = 0; i < text.length; i++) h = Math.imul(h ^ text.charCodeAt(i), 16777619)
    h = Math.imul(h ^ 0x10000, 16777619)
  }
  return h >>> 0
}

function offsetAt(root: HTMLElement, node: Node, offset: number): Point | null {
  if (node === root) {
    const last = root.children.length - 1
    if (last < 0) return [0, 0]
    return offset <= last ? [offset, 0] : [last, root.children[last]!.textContent?.length ?? 0]
  }
  let p: Node = node
  while (p.parentNode && p.parentNode !== root) p = p.parentNode
  if (p.parentNode !== root) return null
  if (!(p instanceof Element)) return null
  const before = document.createRange()
  before.selectNodeContents(p)
  before.setEnd(node, offset)
  return [Array.prototype.indexOf.call(root.children, p), before.toString().length]
}

export function textSelection(root: HTMLElement | null): TextSelection | null {
  const s = window.getSelection()
  if (!root || !s?.rangeCount || !root.contains(s.anchorNode) || !root.contains(s.focusNode))
    return null
  const body = bodyKey(root)
  if (root.dataset.presenceBody !== String(body)) return null
  const r = s.getRangeAt(0)
  const start = offsetAt(root, r.startContainer, r.startOffset)
  const end = offsetAt(root, r.endContainer, r.endOffset)
  return start && end ? { start, end, body } : null
}

function boundary(root: HTMLElement, [paragraph, offset]: Point): [Node, number] | null {
  const p = root.children[paragraph]
  if (!p) return !root.childNodes.length && paragraph === 0 && offset === 0 ? [root, 0] : null
  const walker = document.createTreeWalker(p, NodeFilter.SHOW_TEXT)
  for (let n = walker.nextNode(); n; n = walker.nextNode()) {
    const len = n.textContent?.length ?? 0
    if (offset <= len) return [n, offset]
    offset -= len
  }
  return offset === 0 ? [p, 0] : null
}

/** Read rects only from text nodes, not from the editor's full-width paragraph divs. */
export function selectedRects(root: HTMLElement, text: TextSelection): DOMRect[] {
  if (bodyKey(root) !== text.body) return []
  const a = boundary(root, text.start),
    b = boundary(root, text.end)
  if (!a || !b) return []
  const range = document.createRange()
  range.setStart(...a)
  range.setEnd(...b)
  if (range.collapsed) {
    if (a[0] === root && !root.childNodes.length) {
      const br = document.createElement('br')
      root.appendChild(br)
      const rect = br.getBoundingClientRect()
      br.remove()
      return [rect]
    }
    const rect =
      range.getClientRects()[0] ??
      (a[0] instanceof Element ? a[0].firstElementChild?.getBoundingClientRect() : undefined)
    return rect ? [rect] : []
  }
  const rects: DOMRect[] = []
  const walker = document.createTreeWalker(range.commonAncestorContainer, NodeFilter.SHOW_TEXT)
  const part = document.createRange()
  for (let n: Node | null = walker.currentNode; n; n = walker.nextNode()) {
    if (n.nodeType !== Node.TEXT_NODE || !range.intersectsNode(n)) continue
    part.selectNodeContents(n)
    if (n === a[0]) part.setStart(...a)
    if (n === b[0]) part.setEnd(...b)
    if (!part.collapsed) rects.push(...part.getClientRects())
  }
  return rects
}
