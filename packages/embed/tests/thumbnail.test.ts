import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { openDocument } from '../src/index'

const BASE = 'https://office.example/'
const ORIGIN = 'https://office.example'

function post(source: Window | null, data: Record<string, unknown>): void {
  window.dispatchEvent(
    new MessageEvent('message', {
      source,
      origin: ORIGIN,
      data: { source: 'seuoffice', version: 1, ...data },
    }),
  )
}

/** A document the page has not answered for yet; `asked` are the `thumbnail` messages this side sent */
function embedded(mode: 'view' | 'edit' = 'view') {
  const base = { baseUrl: BASE, fileName: 'report.pdf', bytes: new Uint8Array([1]) }
  const doc =
    mode === 'edit'
      ? openDocument(document.body, { ...base, mode, onSave: async () => {} })
      : openDocument(document.body, base)
  const spy = vi.spyOn(doc.iframe.contentWindow!, 'postMessage').mockImplementation(() => {})
  const page = (data: Record<string, unknown>) => post(doc.iframe.contentWindow, data)
  const asked = () =>
    spy.mock.calls
      .map((call) => call[0] as Record<string, unknown>)
      .filter((message) => message.type === 'thumbnail')
  return { doc, page, asked }
}

function shown(mode: 'view' | 'edit' = 'view') {
  const opened = embedded(mode)
  opened.page({ type: 'ready' })
  opened.page({ type: 'opened' })
  return opened
}

beforeEach(() => {
  vi.useFakeTimers()
})

afterEach(() => {
  vi.useRealTimers()
  document.body.replaceChildren()
})

describe('asking the page for its first page as an image', () => {
  it("resolves with the page's PNG and its size, in view and in edit mode", async () => {
    for (const mode of ['view', 'edit'] as const) {
      const { doc, page, asked } = shown(mode)
      const image = doc.thumbnail({ width: 320 })
      const [request] = asked()
      expect(request).toMatchObject({ width: 320 })
      page({
        type: 'thumbnail_done',
        requestId: request!.requestId,
        ok: true,
        bytes: new Uint8Array([137, 80, 78, 71]).buffer,
        width: 320,
        height: 452,
      })
      await expect(image).resolves.toEqual({
        bytes: new Uint8Array([137, 80, 78, 71]),
        width: 320,
        height: 452,
      })
    }
  })

  it('leaves the width to the page when the host names none', () => {
    const { doc, asked } = shown()
    void doc.thumbnail().catch(() => {})
    expect(asked()[0]).not.toHaveProperty('width')
  })

  it.each([
    'thumbnail_unsupported',
    'thumbnail_timeout',
    'thumbnail_too_large',
    'thumbnail_tainted',
  ] as const)("rejects with the page's %s and its reason", async (code) => {
    const { doc, page, asked } = shown()
    const image = doc.thumbnail()
    page({
      type: 'thumbnail_done',
      requestId: asked()[0]!.requestId,
      ok: false,
      code,
      message: 'why not',
    })
    await expect(image).rejects.toMatchObject({
      name: 'SeuofficeEmbedError',
      code,
      message: 'why not',
    })
  })

  it('reads an answer it cannot use as thumbnail_unsupported', async () => {
    const { doc, page, asked } = shown()
    const unknownCode = doc.thumbnail()
    const noBytes = doc.thumbnail()
    const [first, second] = asked()
    page({ type: 'thumbnail_done', requestId: first!.requestId, ok: false, code: 'made_up' })
    page({ type: 'thumbnail_done', requestId: second!.requestId, ok: true, width: 1, height: 1 })
    await expect(unknownCode).rejects.toMatchObject({ code: 'thumbnail_unsupported' })
    await expect(noBytes).rejects.toMatchObject({ code: 'thumbnail_unsupported' })
  })

  it('rejects without asking the page before the document opened', async () => {
    const { doc, page, asked } = embedded()
    await expect(doc.thumbnail()).rejects.toMatchObject({ code: 'thumbnail_unsupported' })
    page({ type: 'ready' })
    await expect(doc.thumbnail()).rejects.toMatchObject({ code: 'thumbnail_unsupported' })
    expect(asked()).toEqual([])
  })

  it('rejects for a file no app opens', async () => {
    const doc = openDocument(document.body, {
      baseUrl: BASE,
      fileName: 'notes.txt',
      bytes: new Uint8Array([1]),
    })
    await expect(doc.thumbnail()).rejects.toMatchObject({ code: 'thumbnail_unsupported' })
  })

  it('gives each of two requests its own answer', async () => {
    const { doc, page, asked } = shown()
    const small = doc.thumbnail({ width: 64 })
    const large = doc.thumbnail({ width: 1024 })
    const [a, b] = asked()
    expect(a!.requestId).not.toBe(b!.requestId)
    page({
      type: 'thumbnail_done',
      requestId: b!.requestId,
      ok: false,
      code: 'thumbnail_too_large',
    })
    page({
      type: 'thumbnail_done',
      requestId: a!.requestId,
      ok: true,
      bytes: new ArrayBuffer(3),
      width: 64,
      height: 90,
    })
    await expect(small).resolves.toMatchObject({ width: 64, height: 90 })
    await expect(large).rejects.toMatchObject({ code: 'thumbnail_too_large' })
  })

  it('rejects with thumbnail_timeout when the page never answers, and drops a late answer', async () => {
    const { doc, page, asked } = shown()
    const image = doc.thumbnail()
    let settled = false
    void image.catch(() => (settled = true))
    const refused = expect(image).rejects.toMatchObject({ code: 'thumbnail_timeout' })
    await vi.advanceTimersByTimeAsync(14_999)
    expect(settled).toBe(false)
    await vi.advanceTimersByTimeAsync(1)
    await refused
    page({
      type: 'thumbnail_done',
      requestId: asked()[0]!.requestId,
      ok: true,
      bytes: new ArrayBuffer(1),
      width: 480,
      height: 1,
    })
  })

  it('rejects every request in flight on destroy()', async () => {
    const { doc } = shown()
    const image = doc.thumbnail()
    doc.destroy()
    await expect(image).rejects.toMatchObject({ code: 'thumbnail_unsupported' })
  })

  it('ignores an answer from another origin', async () => {
    const { doc, asked } = shown()
    const image = doc.thumbnail()
    let settled = false
    void image.then(
      () => (settled = true),
      () => (settled = true),
    )
    window.dispatchEvent(
      new MessageEvent('message', {
        source: doc.iframe.contentWindow,
        origin: 'https://evil.example',
        data: {
          source: 'seuoffice',
          version: 1,
          type: 'thumbnail_done',
          requestId: asked()[0]!.requestId,
          ok: true,
          bytes: new ArrayBuffer(1),
          width: 480,
          height: 1,
        },
      }),
    )
    await vi.advanceTimersByTimeAsync(0)
    expect(settled).toBe(false)
    doc.destroy()
    await image.catch(() => {})
  })
})
