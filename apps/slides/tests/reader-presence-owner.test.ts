import type { RenderSlide } from '@seuoffice/pptx-render'
import { act, createElement } from 'react'
import { createRoot } from 'react-dom/client'
import { expect, it, vi } from 'vitest'
import { SlidePresenceLayer } from '../src/renderer/SlidePresenceLayer'
import { SlidePresence } from '../src/renderer/slide-presence'

function slide(partPath: string, durableId: string): RenderSlide {
  return {
    partPath,
    widthPx: 100,
    heightPx: 100,
    scale: 1,
    background: { kind: 'none' },
    nodes: [
      {
        id: 'shape',
        sourceId: 'shape',
        durableId,
        type: 'shape',
        fill: { kind: 'none' },
        box: {
          x: 0,
          y: 0,
          w: 10,
          h: 10,
          centerX: 5,
          centerY: 5,
          rotationDeg: 0,
          flipH: false,
          flipV: false,
        },
      },
    ],
  }
}

it('publishes only the selected reader slide across sibling updates, navigation and deselection', () => {
  vi.stubGlobal('IS_REACT_ACT_ENVIRONMENT', true)
  const container = document.createElement('div')
  const root = createRoot(container)
  const sent: (string | null)[] = []
  const presence = new SlidePresence(() => sent.push(presence.read()?.ids[0] ?? null))
  const a = slide('ppt/slides/slide1.xml', 'e_a')
  const b = slide('ppt/slides/slide2.xml', 'e_b')
  const render = (...slides: RenderSlide[]) =>
    act(() =>
      root.render(
        slides.map((s) =>
          createElement(SlidePresenceLayer, {
            key: s.partPath,
            presence,
            slide: s,
            readerScale: 1,
            selectedIds: [],
          }),
        ),
      ),
    )
  const object = (id: string) =>
    container.querySelector<HTMLElement>(`[data-slide-object="${id}"]`)!
  const select = (id: string) =>
    act(() => {
      object(id).dispatchEvent(new MouseEvent('mousedown', { bubbles: true }))
    })
  try {
    render(a, b)
    select('e_a')
    render(a, { ...b })
    render(a)
    expect(presence.read()).toEqual({ slide: a.partPath, group: null, ids: ['e_a'], text: null })
    render(a, b)
    select('e_b')
    expect(presence.read()).toEqual({ slide: b.partPath, group: null, ids: ['e_b'], text: null })
    expect(object('e_a').style.outline).toBe('')
    select('e_a')
    expect(object('e_b').style.outline).toBe('')
    act(() => object('e_a').parentElement!.click())
    expect(presence.read()).toBeNull()
    select('e_b')
    render(a)
    expect(presence.read()).toBeNull()
    expect(sent).toEqual(['e_a', 'e_b', 'e_a', null, 'e_b', null])
  } finally {
    act(() => root.unmount())
    vi.unstubAllGlobals()
  }
})
