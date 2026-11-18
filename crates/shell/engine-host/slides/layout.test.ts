import { afterEach, describe, expect, it, vi } from 'vitest'
import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import { openPptx, type Slide, type TextBody } from '@seuoffice/pptx-engine'
import { buildRenderSlide, EMU_PER_PX_96, HeuristicMetrics, layoutText, makeViewport, type RunStyle } from '@seuoffice/pptx-render'
import { measureAutofit, measurePageText } from '../slides-layout'

const fixture = fileURLToPath(new URL('../../../../packages/pptx-engine/tests/fixtures/01_standard_business.pptx', import.meta.url))

afterEach(() => vi.unstubAllGlobals())

describe('page text measurement boundary', () => {
  it('returns the original renderer measurements at native scale without mutating the slide', async () => {
    vi.stubGlobal('document', { baseURI: 'file:///' })
    const { deck } = await openPptx(readFileSync(fixture))
    const slide: Slide = { ...deck.slides[0]!, decorations: [], elements: (['top', 'middle', 'bottom'] as const).map((anchor, i) => ({
      id: `text-${i}`,
      type: 'text' as const,
      anchor: { spIndex: -1, originalXml: '', range: [0, 0] as [number, number] },
      transform: { offset: { x: 0, y: i * 300000, cx: 914400, cy: 100000 }, rot: 0, flipH: false, flipV: false },
      text: { anchor, paragraphs: [{ runs: [{ text: 'A long text box line '.repeat(5), fontSize: 18 }] }], insets: { l: 0, t: 10000, r: 0, b: 20000 } },
    })) }
    const before = structuredClone(slide)
    const metrics = new HeuristicMetrics()
    const callback = vi.fn(async (operation: string, input: unknown) => {
      expect(operation).toBe('slides.fontMetrics')
      const { items } = input as { items: Array<{ kind: string; style: RunStyle; text?: string }> }
      return items.map(item => {
        switch (item.kind) {
          case 'metrics': return metrics.metrics(item.style)
          case 'measure': return metrics.measure(item.text ?? '', item.style)
          case 'displayFamily': return item.style.fontFamily
          case 'substituted': return false
          default: throw new Error(`unexpected metric ${item.kind}`)
        }
      })
    })
    const expected = buildRenderSlide(slide, deck.size, { fitWidthPx: deck.size.cx / EMU_PER_PX_96, metrics }).nodes.flatMap(node => {
      if (node.type !== 'text' || !node.text) return []
      return [{ sourceId: node.sourceId, boxHeight: node.box.h, contentHeight: node.text.contentHeight,
        ...(node.text.inkBottom !== undefined ? { inkBottom: node.text.inkBottom } : {}),
        insets: { t: node.text.insets.t, b: node.text.insets.b }, anchor: node.text.anchor }]
    })
    expect(expected.length).toBeGreaterThan(0)
    expect(await measurePageText({ slide, size: deck.size }, callback, new AbortController().signal)).toEqual(expected)
    expect(slide).toEqual(before)
    expect(callback).toHaveBeenCalled()
  })

  it('refits a stored shrink scale using the original text layout', async () => {
    vi.stubGlobal('document', { baseURI: 'file:///' })
    const metrics = new HeuristicMetrics()
    const body: TextBody = {
      autofit: 'shrink', fontScale: 0.9,
      paragraphs: [{ runs: [{ text: 'autofit '.repeat(40), fontSize: 24 }] }],
    }
    const size = { cx: 9144000, cy: 5143500 }
    const fitWidthPx = 960
    const boxWidthPx = 160
    const boxHeightPx = 50
    const callback = vi.fn(async (_operation: string, input: unknown) => {
      const { items } = input as { items: Array<{ kind: string; style: RunStyle; text?: string }> }
      return items.map(item => {
        switch (item.kind) {
          case 'metrics': return metrics.metrics(item.style)
          case 'measure': return metrics.measure(item.text ?? '', item.style)
          case 'displayFamily': return item.style.fontFamily
          case 'substituted': return false
          default: throw new Error(`unexpected metric ${item.kind}`)
        }
      })
    })
    const refit = layoutText({ body, boxWidthPx, boxHeightPx, metrics, vp: makeViewport(size, fitWidthPx), refitAutofit: true })
    expect(await measureAutofit({ body, size, fitWidthPx, boxWidthPx, boxHeightPx }, callback, new AbortController().signal))
      .toEqual({ fontScale: refit.fontScale, lnSpcReduction: refit.lnSpcReduction ?? 0 })
  })

  it('propagates cancellation and rejects malformed requests', async () => {
    const callback = vi.fn()
    await expect(measurePageText({}, callback, new AbortController().signal)).rejects.toThrow('invalid slide text measurement input')
    const controller = new AbortController()
    controller.abort(new Error('cancelled'))
    await expect(measurePageText({ slide: {}, size: {} }, callback, controller.signal)).rejects.toThrow('cancelled')
    expect(callback).not.toHaveBeenCalled()
  })
})
