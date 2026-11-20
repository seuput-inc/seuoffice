import { afterEach, describe, expect, it, vi } from 'vitest'
import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import { openPptx, type Slide } from '@seuoffice/pptx-engine'
import {
  buildRenderSlide,
  HeuristicMetrics,
  type RenderSlide,
  type RunStyle,
} from '@seuoffice/pptx-render'
import { decode } from '../../bridge/wire'
import { renderAll, renderSlide } from '../slides'

const fixture = fileURLToPath(
  new URL(
    '../../../../packages/pptx-engine/tests/fixtures/01_standard_business.pptx',
    import.meta.url,
  ),
)
const signal = () => new AbortController().signal
const url = 'seuoffice-slide-media://archive/test?mime=image%2Fpng'

function callback() {
  const metrics = new HeuristicMetrics()
  return vi.fn(async (operation: string, input: unknown) => {
    if (!input || typeof input !== 'object' || !('items' in input) || !Array.isArray(input.items))
      throw new TypeError('missing callback items')
    const items: Array<{ kind: string; style: RunStyle; text?: string }> = input.items
    if (operation === 'slides.mediaResolve') return items.map(() => url)
    if (operation !== 'slides.fontMetrics') throw new Error(`unexpected callback ${operation}`)
    return items.map((item) => {
      switch (item.kind) {
        case 'metrics':
          return metrics.metrics(item.style)
        case 'measure':
          return metrics.measure(item.text ?? '', item.style)
        case 'displayFamily':
          return item.style.fontFamily
        case 'substituted':
          return false
        default:
          throw new Error(`unexpected metric ${item.kind}`)
      }
    })
  })
}

function decoded(frame: { header: unknown; buffers: string[] }): unknown {
  return decode(
    frame.header,
    frame.buffers.map((value) => Uint8Array.from(atob(value), (c) => c.charCodeAt(0))),
  )
}

afterEach(() => vi.unstubAllGlobals())

describe('native slide render boundary', () => {
  it('renders the current unsaved slide model, not the archive copy', async () => {
    vi.stubGlobal('document', { baseURI: 'file:///' })
    const opened = await openPptx(readFileSync(fixture))
    const first = opened.deck.slides[0]!
    const edited: Slide = { ...first, background: { type: 'solid', color: '#123456' }, bgOwn: true }
    const deck = { ...opened.deck, slides: [edited] }
    const entries = [...opened.archive.entries].map(([path, bytes]) => ({
      path,
      bytes: [...bytes],
    }))
    const invoke = callback()
    const input = { deck, entries, originalHash: opened.archive.originalHash, fitWidthPx: 960 }
    const result = decoded(await renderAll(input, invoke, signal())) as RenderSlide[]
    const expected = buildRenderSlide(edited, deck.size, {
      fitWidthPx: 960,
      metrics: new HeuristicMetrics(),
      slideNo: 1,
      media: (ref) => (opened.archive.readBytes(ref) ? url : undefined),
    })
    expect(result[0]).toEqual(expected)
    expect(result[0]?.background).toEqual(expected.background)
    expect(invoke.mock.calls.some(([name]) => name === 'slides.fontMetrics')).toBe(true)
  })

  it('returns the supplied live master and preserves missing slide behavior', async () => {
    vi.stubGlobal('document', { baseURI: 'file:///' })
    const opened = await openPptx(readFileSync(fixture))
    const master = {
      ...opened.deck.slides[0]!,
      elements: [],
      decorations: [],
      background: { type: 'solid' as const, color: '#AA2200' },
    }
    const input = {
      deck: opened.deck,
      entries: [],
      originalHash: opened.archive.originalHash,
      fitWidthPx: 800,
    }
    const result = decoded(await renderSlide({ ...input, master }, callback(), signal()))
    expect(result).toEqual(
      buildRenderSlide(master, opened.deck.size, {
        fitWidthPx: 800,
        metrics: new HeuristicMetrics(),
      }),
    )
    expect(
      decoded(await renderSlide({ ...input, slideIndex: 999 }, callback(), signal())),
    ).toBeNull()
  })

  it('resolves archive media in the addressed slide context', async () => {
    vi.stubGlobal('document', { baseURI: 'file:///' })
    const opened = await openPptx(readFileSync(fixture))
    const slide = opened.deck.slides[0]!
    const picture = {
      id: 'pic_live',
      type: 'picture',
      anchor: { spIndex: -1, originalXml: '', range: [0, 0] },
      transform: {
        offset: { x: 0, y: 0, cx: 914400, cy: 914400 },
        rot: 0,
        flipH: false,
        flipV: false,
      },
      mediaRef: 'ppt/media/image-test.png',
    } as Slide['elements'][number]
    const deck = { ...opened.deck, slides: [{ ...slide, elements: [picture], decorations: [] }] }
    const input = {
      deck,
      entries: [{ path: 'ppt/media/image-test.png', bytes: [137, 80, 78, 71] }],
      originalHash: 'source',
      fitWidthPx: 800,
      slideIndex: 0,
    }
    const invoke = callback()
    const result = decoded(await renderSlide(input, invoke, signal())) as RenderSlide
    expect(result.nodes[0]).toMatchObject({ type: 'picture', dataUrl: url })
    expect(invoke.mock.calls).toContainEqual([
      'slides.mediaResolve',
      { items: [{ mediaRef: 'ppt/media/image-test.png', slidePath: slide.path }] },
    ])
  })
})
