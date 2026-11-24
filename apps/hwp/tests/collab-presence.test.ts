import { readFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import { dirname, join } from 'node:path'
import init, { HwpDocument } from '@rhwp/core'
import { JSDOM } from 'jsdom'
import { afterEach, beforeAll, describe, expect, it, vi } from 'vitest'
import { bindHwpPresence, type HwpPresenceHost } from '../studio-plugin/collab-presence.mjs'
import { readOnlyAllows, readOnlyDocument } from '../studio-plugin/read-only.mjs'

const require = createRequire(import.meta.url)
beforeAll(async () => {
  const wasm = join(dirname(require.resolve('@rhwp/core')), 'rhwp_bg.wasm')
  await init({ module_or_path: readFileSync(wasm) })
})

const cleanup: (() => void)[] = []
afterEach(() => {
  while (cleanup.length) cleanup.pop()!()
  vi.unstubAllGlobals()
})

const body = (paragraph: number, offset: number) => ({ section: 0, paragraph, path: [], offset })
const path = [
  { controlIndex: 0, cellIndex: 9, cellParaIndex: 3 },
  { controlIndex: 0, cellIndex: 1, cellParaIndex: 0 },
]
const nested = (offset: number) => ({ section: 0, paragraph: 2, path, offset })
const peer = {
  client: 'A" data-spoof="1',
  name: '<img src=x onerror=alert(1)>',
  color: '#1261a0',
  at: { anchor: body(0, 2), head: body(0, 2) },
}

function studio(nestedFixture = false) {
  const doc = nestedFixture
    ? new HwpDocument(readFileSync(join(__dirname, 'fixtures', 'nested-form.hwp')))
    : HwpDocument.createEmpty()
  if (!nestedFixture) {
    doc.createBlankDocument()
    doc.insertText(0, 0, 0, '가😀나다')
    doc.splitParagraph(0, 0, 4)
    doc.insertText(0, 1, 0, '둘째 문단')
  }
  const page = new JSDOM('<div id="scroll-container"><div id="scroll-content"></div></div>', {
    pretendToBeVisual: true,
  }).window
  vi.stubGlobal('document', page.document)
  const content = page.document.getElementById('scroll-content')!
  const events = new EventTarget()
  const state = {
    head: {} as Record<string, unknown>,
    anchor: null as Record<string, unknown> | null,
    elsewhere: false,
    cellBlock: false,
    picture: false,
    table: false,
    zoom: 1,
    left: 18,
    top: 34,
    pageReady: true,
    bodyTables: [] as { pageIndex: number; x: number; y: number; width: number; height: number }[],
  }
  state.head = {
    sectionIndex: 0,
    paragraphIndex: 0,
    charOffset: 2,
    cursorRect: { x: 9000, y: 9999 },
  }
  const services = {
    getInputHandler: () => ({
      isInSeparateCaretSpace: () => state.elsewhere,
      bodyTableRectsInRange: () => state.bodyTables,
      cursor: {
        getPosition: () => state.head,
        getSelection: () => state.anchor && { anchor: state.anchor, focus: state.head },
        isInCellSelectionMode: () => state.cellBlock,
        isInPictureObjectSelection: () => state.picture,
        isInTableObjectSelection: () => state.table,
      },
      virtualScroll: {
        getPageWidth: (i: number) => (state.pageReady && i < doc.pageCount() ? 500 : 0),
        getPageOffset: (i: number) => state.top + i * 800,
        getPageLeftResolved: () => state.left,
      },
    }),
    getViewportManager: () => ({ getZoom: () => state.zoom }),
  }
  const commands = new Map<string, (services: unknown) => void>()
  const host: HwpPresenceHost = {
    read: (fn) => fn(readOnlyDocument(doc)),
    events: {
      on: (name, cb) => {
        events.addEventListener(name, cb)
        return () => events.removeEventListener(name, cb)
      },
    },
    automation: {
      registerCommand: (command) => {
        commands.set(command.id, command.execute)
      },
      unregisterCommand: (id) => {
        commands.delete(id)
      },
      execute: (id) => {
        if (readOnlyAllows(id)) commands.get(id)?.(services)
      },
    },
  }
  const publish = vi.fn()
  const bridge = bindHwpPresence(host, publish)
  cleanup.push(() => {
    bridge.dispose()
    page.close()
    doc.free()
  })
  return {
    doc,
    page,
    state,
    bridge,
    content,
    publish,
    emit: (event: string) => events.dispatchEvent(new Event(event)),
    frame: () => {
      const { promise, resolve } = Promise.withResolvers<void>()
      page.requestAnimationFrame(() => resolve())
      return promise
    },
    caret: () => content.querySelector<HTMLElement>('.hwp-presence-caret'),
  }
}

describe('HWP peer presence on a read-only Studio host', () => {
  it('publishes the logical caret or directed range, never hit-test coordinates or selected text', () => {
    const { bridge, state, emit, publish } = studio()
    expect(bridge.read()).toEqual({ anchor: body(0, 2), head: body(0, 2) })
    state.anchor = { sectionIndex: 0, paragraphIndex: 1, charOffset: 4, text: 'private' }
    emit('cursor-rect-updated')
    expect(publish).toHaveBeenCalledOnce()
    expect(bridge.read()).toEqual({ anchor: body(1, 4), head: body(0, 2) })
    for (const mode of ['elsewhere', 'cellBlock', 'picture', 'table'] as const) {
      state[mode] = true
      expect(bridge.read()).toBeNull()
      state[mode] = false
    }
    state.head.charOffset = 5 // Four engine characters, five UTF-16 units.
    expect(bridge.read()).toBeNull()
  })

  it('reads and paints a backward selection in the inner table of a Hancom document', () => {
    const { bridge, state, content, doc, caret } = studio(true)
    state.head = {
      sectionIndex: 0,
      paragraphIndex: 0,
      parentParaIndex: 2,
      cellPath: path,
      charOffset: 1,
    }
    state.anchor = { ...state.head, charOffset: 4 }
    const selection = { anchor: nested(4), head: nested(1) }
    expect(bridge.read()).toEqual(selection)
    bridge.draw([{ ...peer, at: selection }])
    const rect = JSON.parse(doc.getCursorRectByPath(0, 2, JSON.stringify(path), 1))
    expect(parseFloat(caret()!.style.left)).toBeCloseTo(state.left + rect.x)
    expect(parseFloat(caret()!.style.top)).toBeCloseTo(state.top + rect.pageIndex * 800 + rect.y)
    const ranges = JSON.parse(
      doc.getSelectionRectsInCellByPath(0, 2, JSON.stringify(path), 0, 1, 0, 4),
    )
    expect(ranges.length).toBeGreaterThan(0)
    const boxes = content.querySelectorAll<HTMLElement>('.hwp-presence-range')
    expect(boxes).toHaveLength(ranges.length)
    expect(boxes[0].style.width).toBe(`${ranges[0].width}px`)
    expect(boxes[0].style.height).toBe(`${ranges[0].height}px`)
    expect(caret()?.querySelector('span')?.textContent).toBe(peer.name)
    expect(caret()?.querySelector('span')?.dataset.presenceClient).toBe(peer.client)
    expect(content.querySelector('img, [data-spoof]')).toBeNull()
    expect(caret()?.style.backgroundColor).toBe('rgb(18, 97, 160)')
    expect(caret()?.querySelector('span')?.style.backgroundColor).toBe('rgb(18, 97, 160)')
    expect(bridge.read()).toEqual(selection)
    state.anchor = null
    state.head = {
      ...state.head,
      cellPath: undefined,
      controlIndex: 0,
      cellIndex: 9,
      cellParaIndex: 3,
      charOffset: 0,
    }
    expect(bridge.read()?.head).toEqual({ ...nested(0), path: path.slice(0, 1) })
    state.head.isTextBox = true
    expect(bridge.read()).toBeNull()
  })

  it('refreshes a multi-paragraph body range on zoom, scroll and reflow without moving the local caret', async () => {
    const { bridge, state, content, doc, caret, page, frame, emit } = studio()
    const local = bridge.read()
    bridge.draw([{ ...peer, at: { anchor: body(0, 1), head: body(1, 2) } }])
    const rect = JSON.parse(doc.getCursorRect(0, 1, 2))
    const assertBounds = () => {
      expect(parseFloat(caret()!.style.left)).toBeCloseTo(state.left + rect.x * state.zoom)
      expect(parseFloat(caret()!.style.top)).toBeCloseTo(state.top + rect.y * state.zoom)
      expect(parseFloat(caret()!.style.height)).toBeCloseTo(rect.height * state.zoom)
      expect(content.querySelectorAll('.hwp-presence-range').length).toBeGreaterThanOrEqual(2)
    }
    assertBounds()
    state.zoom = 1.5
    emit('zoom-changed')
    await frame()
    assertBounds()
    state.left += 31
    content.parentElement!.dispatchEvent(new page.Event('scroll'))
    await frame()
    assertBounds()
    state.top += 177
    emit('document-layout-refreshed')
    await frame()
    assertBounds()
    expect(bridge.read()).toEqual(local)
  })

  it('projects the two sides of a backward body selection in their respective sections', () => {
    const { bridge, state, doc, content } = studio()
    doc.splitSection(0, 1, 0)
    state.anchor = { sectionIndex: 1, paragraphIndex: 0, charOffset: 3 }
    const selection = { anchor: { ...body(0, 3), section: 1 }, head: body(0, 2) }
    expect(bridge.read()).toEqual(selection)
    bridge.draw([{ ...peer, at: selection }])
    const boxes = [...content.querySelectorAll<HTMLElement>('.hwp-presence-range')]
    const rects = [
      ...JSON.parse(doc.getSelectionRects(0, 0, 2, 0, 4)),
      ...JSON.parse(doc.getSelectionRects(1, 0, 0, 0, 3)),
    ]
    expect(rects.length).toBeGreaterThanOrEqual(2)
    expect(boxes).toHaveLength(rects.length)
    rects.forEach((rect, i) => {
      expect(parseFloat(boxes[i].style.top)).toBeCloseTo(state.top + rect.pageIndex * 800 + rect.y)
    })
  })

  it('includes Studio’s table boxes in body ranges, but never paints a partly unresolved range', () => {
    const { bridge, state, content } = studio()
    const range = { ...peer, at: { anchor: body(0, 0), head: body(1, 2) } }
    state.bodyTables = [{ pageIndex: 0, x: 60, y: 80, width: 175, height: 45 }]
    bridge.draw([range])
    const table = [...content.querySelectorAll<HTMLElement>('.hwp-presence-range')].find(
      (el) => el.style.width === '175px',
    )
    expect(table?.style.left).toBe(`${state.left + 60}px`)
    state.bodyTables[0].x = NaN
    bridge.draw([range])
    expect(content.childElementCount).toBe(0)
  })

  it('removes only unresolved, malformed or unsupported peers and never falls back to a body caret', async () => {
    const { bridge, content, caret, emit, frame, state } = studio()
    const valid = { ...peer, client: 'valid', name: '별' }
    bridge.draw([valid, peer])
    expect(content.querySelectorAll('[data-presence-client]')).toHaveLength(2)
    for (const invalid of [
      { ...peer, color: '#1261a0;background-image:url(https://invalid.test/x)' },
      { ...peer, at: null },
      { ...peer, at: { anchor: body(0, 1), head: body(0, 4294967296) } },
      { ...peer, at: { anchor: body(4, 0), head: body(4, 0) } },
      { ...peer, at: { anchor: body(0, 0), head: nested(1) } },
      { ...peer, at: { anchor: nested(1), head: nested(2) } },
    ]) {
      bridge.draw([valid, invalid])
      expect(content.textContent).toBe(valid.name)
      expect(content.querySelectorAll('[data-presence-client]')).toHaveLength(1)
    }
    state.pageReady = false
    emit('document-layout-refreshed')
    await frame()
    expect(caret()).toBeNull()
    state.pageReady = true
    emit('document-layout-refreshed')
    await frame()
    expect(content.textContent).toBe(valid.name)
    emit('zoom-changed')
    bridge.draw([]) // A leave or joinPageCollab's buffer/unclean-version gate.
    expect(content.childElementCount).toBe(0)
    await frame()
    emit('document-layout-refreshed')
    expect(content.childElementCount).toBe(0)
  })

  it('unsubscribes and cannot revive a queued mark after disposal', async () => {
    const { bridge, content, frame, emit, publish } = studio()
    bridge.draw([peer])
    expect(content.textContent).toBe(peer.name)
    emit('viewport-resize')
    bridge.dispose()
    bridge.dispose()
    bridge.draw([peer])
    emit('cursor-rect-updated')
    emit('document-layout-refreshed')
    await frame()
    expect(bridge.read()).toBeNull()
    expect(content.childElementCount).toBe(0)
    expect(publish).not.toHaveBeenCalled()
  })
})
