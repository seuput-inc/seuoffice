import { presenceLabel } from '@seuoffice/web-platform/presence'
import { parseHwpPosition, sameHwpContainer } from '../src/shared/collab-text.ts'

function studioPosition(pos) {
  if (!pos || pos.isTextBox) return null
  const inCell = pos.parentParaIndex !== undefined
  if (
    !inCell &&
    (pos.cellPath?.length ||
      pos.controlIndex !== undefined ||
      pos.cellIndex !== undefined ||
      pos.cellParaIndex !== undefined)
  )
    return null
  const cells = pos.cellPath ?? []
  if (!Array.isArray(cells)) return null
  const path = cells.length || !inCell ? cells : [pos]
  return parseHwpPosition({
    section: pos.sectionIndex,
    paragraph: inCell ? pos.parentParaIndex : pos.paragraphIndex,
    path: path.map(({ controlIndex, cellIndex, cellParaIndex }) => ({
      controlIndex,
      cellIndex,
      cellParaIndex,
    })),
    offset: pos.charOffset,
  })
}

const paraNumber = (pos) => pos.path.at(-1)?.cellParaIndex ?? pos.paragraph

function inDocument(doc, { section, paragraph, path, offset }) {
  // The length lookup alone also accepts captions, text boxes and header paths.
  for (let i = 1; i <= path.length; i++) {
    doc.getTableDimensionsByPath(section, paragraph, JSON.stringify(path.slice(0, i)))
  }
  const length = path.length
    ? doc.getCellParagraphLengthByPath(section, paragraph, JSON.stringify(path))
    : doc.getLogicalLength(section, paragraph)
  return offset <= length
}

function selection(doc, anchor, head) {
  if (!anchor || !head) return null
  if ((anchor.path.length || head.path.length) && !sameHwpContainer(anchor, head)) return null
  if (!inDocument(doc, head) || (anchor !== head && !inDocument(doc, anchor))) return null
  return { anchor, head }
}

export function validateHwpSelection(doc, at) {
  try {
    return selection(doc, parseHwpPosition(at?.anchor), parseHwpPosition(at?.head))
  } catch {
    return null
  }
}

export function readHwpSelection(input, doc) {
  const cursor = input?.cursor
  if (
    !cursor ||
    input.active === false ||
    input.formOverlay ||
    input.isComposing ||
    input.isInSeparateCaretSpace() ||
    cursor.isInCellSelectionMode() ||
    cursor.isInTableObjectSelection() ||
    cursor.isInPictureObjectSelection()
  )
    return null
  const range = cursor.getSelection()
  const head = studioPosition(range ? range.focus : cursor.getPosition())
  return selection(doc, range ? studioPosition(range.anchor) : head, head)
}

function rangeRects(doc, input, anchor, head) {
  const order =
    anchor.section - head.section ||
    paraNumber(anchor) - paraNumber(head) ||
    anchor.offset - head.offset
  if (!order) return []
  const [a, b] = order < 0 ? [anchor, head] : [head, anchor]
  let result
  if (a.path.length) {
    result = JSON.parse(
      doc.getSelectionRectsInCellByPath(
        a.section,
        a.paragraph,
        JSON.stringify(a.path),
        paraNumber(a),
        a.offset,
        paraNumber(b),
        b.offset,
      ),
    )
  } else {
    result = []
    // Each engine query addresses paragraphs in a single section.
    for (let sec = a.section; sec <= b.section; sec++) {
      const first = sec === a.section ? a.paragraph : 0
      const last = sec === b.section ? b.paragraph : doc.getParagraphCount(sec) - 1
      if (last < first) continue
      const start = sec === a.section ? a.offset : 0
      const end = sec === b.section ? b.offset : doc.getLogicalLength(sec, last)
      const rects = JSON.parse(doc.getSelectionRects(sec, first, start, last, end))
      if (!Array.isArray(rects)) return null
      for (const rect of rects) result.push(rect)
    }
    // Text-run rectangles omit tables that Studio highlights as part of a body range.
    const body = (p) => ({
      sectionIndex: p.section,
      paragraphIndex: p.paragraph,
      charOffset: p.offset,
    })
    const tables = input.bodyTableRectsInRange(body(a), body(b))
    result = result.concat(tables)
  }
  return Array.isArray(result) && result.length ? result : null
}

function markBox(rect, scroll, zoom, contentWidth, color, caret) {
  if (
    !rect ||
    !Number.isSafeInteger(rect.pageIndex) ||
    rect.pageIndex < 0 ||
    !Number.isFinite(rect.x) ||
    !Number.isFinite(rect.y) ||
    !Number.isFinite(rect.height) ||
    rect.height <= 0 ||
    (!caret && !Number.isFinite(rect.width)) ||
    !(scroll.getPageWidth(rect.pageIndex) > 0)
  )
    return null
  const left = scroll.getPageLeftResolved(rect.pageIndex, contentWidth) + rect.x * zoom
  const top = scroll.getPageOffset(rect.pageIndex) + rect.y * zoom
  const width = caret ? 2 : rect.width * zoom
  const height = rect.height * zoom
  if (![left, top, width, height].every(Number.isFinite) || width <= 0) return null
  const box = document.createElement('div')
  box.className = caret ? 'hwp-presence-caret' : 'hwp-presence-range'
  Object.assign(box.style, {
    position: 'absolute',
    left: `${left}px`,
    top: `${top}px`,
    width: `${width}px`,
    height: `${height}px`,
    backgroundColor: caret ? color : `${color}44`,
  })
  return box
}

function peerMarks(doc, peer, input, zoom, contentWidth) {
  if (
    !peer ||
    typeof peer.client !== 'string' ||
    !peer.client ||
    typeof peer.name !== 'string' ||
    typeof peer.color !== 'string' ||
    peer.color.length !== 7 ||
    !/^#[0-9a-f]{6}$/i.test(peer.color)
  )
    return null
  const place = peer.at
  if (
    !place ||
    typeof place !== 'object' ||
    Array.isArray(place) ||
    !Object.hasOwn(place, 'anchor') ||
    !Object.hasOwn(place, 'head') ||
    Object.keys(place).length !== 2
  )
    return null
  const at = validateHwpSelection(doc, place)
  if (!at) return null
  const { anchor, head } = at
  const rects = rangeRects(doc, input, anchor, head)
  if (!rects) return null
  const rect = JSON.parse(
    head.path.length
      ? doc.getCursorRectByPath(
          head.section,
          head.paragraph,
          JSON.stringify(head.path),
          head.offset,
        )
      : doc.getCursorRect(head.section, head.paragraph, head.offset),
  )
  const scroll = input.virtualScroll
  const caret = markBox(rect, scroll, zoom, contentWidth, peer.color, true)
  if (!caret) return null
  const group = document.createDocumentFragment()
  for (const rect of rects) {
    const box = markBox(rect, scroll, zoom, contentWidth, peer.color, false)
    if (!box) return null
    group.append(box)
  }
  const label = presenceLabel(peer)
  label.style.left = '0'
  label.style.bottom = '100%'
  caret.append(label)
  group.append(caret)
  return group
}

/** Bind in the live Studio frame. joinPageCollab owns version, buffer and transport gating. */
export function bindHwpPresence(host, publish) {
  const command = 'ext:ieum-bind-presence'
  let services
  host.automation.registerCommand({
    id: command,
    label: 'Read collaboration selection',
    execute: (s) => {
      services = s
    },
  })
  try {
    host.automation.execute(command)
  } finally {
    host.automation.unregisterCommand(command)
  }

  const content = document.getElementById('scroll-content')
  const scrolling = document.getElementById('scroll-container')
  const win = document.defaultView
  const layer = document.createElement('div')
  layer.className = 'hwp-presence'
  layer.style.cssText =
    'position:fixed;overflow:hidden;pointer-events:none;user-select:none;z-index:4'
  layer.setAttribute('aria-hidden', 'true')
  const plane = document.createElement('div')
  plane.style.position = 'absolute'
  layer.append(plane)
  let peers = []
  let frame = null
  let disposed = false

  function render() {
    layer.remove()
    plane.replaceChildren()
    if (!content?.isConnected || !scrolling?.isConnected || !peers.length || !services) return
    try {
      const input = services.getInputHandler()
      const zoom = services.getViewportManager()?.getZoom()
      if (!input?.virtualScroll || !Number.isFinite(zoom) || zoom <= 0) return
      const width = content.clientWidth
      const viewport = scrolling.getBoundingClientRect()
      const origin = content.getBoundingClientRect()
      const left = viewport.left + scrolling.clientLeft
      const top = viewport.top + scrolling.clientTop
      Object.assign(layer.style, {
        left: `${left}px`,
        top: `${top}px`,
        width: `${scrolling.clientWidth}px`,
        height: `${scrolling.clientHeight}px`,
      })
      plane.style.left = `${origin.left + content.clientLeft - left}px`
      plane.style.top = `${origin.top + content.clientTop - top}px`
      host.read((doc) => {
        for (const peer of peers) {
          try {
            const group = peerMarks(doc, peer, input, zoom, width)
            if (group) plane.append(group)
          } catch {
            // An unresolved path or a page awaiting layout has no mark.
          }
        }
      })
      if (plane.childElementCount) document.body.append(layer)
    } catch {
      // The document or its viewport was released.
    }
  }

  function layoutChanged() {
    if (!peers.length || disposed) return
    layer.remove()
    if (frame === null)
      frame = win.requestAnimationFrame(() => {
        frame = null
        render()
      })
  }

  function draw(next) {
    if (disposed) return
    peers = next
    render()
  }

  const off = []
  for (const event of [
    'cursor-rect-updated',
    'cell-selection-phase-changed',
    'table-object-selection-changed',
    'picture-object-selection-changed',
    'headerFooterModeChanged',
    'footnoteModeChanged',
  ]) {
    off.push(host.events.on(event, publish))
  }
  for (const event of [
    'zoom-changed',
    'viewport-scroll',
    'viewport-resize',
    'document-layout-refreshed',
    'document-page-invalidated',
    'document-view-loaded',
    'document-changed',
  ]) {
    off.push(host.events.on(event, layoutChanged))
  }
  off.push(
    host.events.on('document-swapped', () => {
      draw([])
      publish()
    }),
  )
  // A form input or IME session can replace the text caret without moving it.
  for (const event of ['focusin', 'focusout', 'compositionstart', 'compositionend']) {
    document.addEventListener(event, publish, true)
    off.push(() => document.removeEventListener(event, publish, true))
  }
  const observer = win.ResizeObserver && new win.ResizeObserver(layoutChanged)
  if (content) observer?.observe(content)
  scrolling?.addEventListener('scroll', layoutChanged, { passive: true })
  win.addEventListener('pagehide', dispose)

  function dispose() {
    if (disposed) return
    disposed = true
    if (frame !== null) win.cancelAnimationFrame(frame)
    for (const unsubscribe of off) unsubscribe()
    observer?.disconnect()
    scrolling?.removeEventListener('scroll', layoutChanged)
    win.removeEventListener('pagehide', dispose)
    peers = []
    layer.replaceChildren()
    layer.remove()
  }

  return {
    read() {
      if (disposed || !services) return null
      try {
        return host.read((doc) => readHwpSelection(services.getInputHandler(), doc))
      } catch {
        return null
      }
    },
    draw,
    dispose,
  }
}
