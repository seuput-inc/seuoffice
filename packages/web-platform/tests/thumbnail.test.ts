import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type * as ChannelModule from '../src/embed-channel'
import type { ThumbnailSource } from '../src/thumbnail'

type Channel = typeof ChannelModule

const HOST = location.origin

let host: Window
let sent: Record<string, unknown>[]
let channel: Channel
/** what the browser's PNG encoder gives for the next image: its bytes, or the error it throws */
let encoded: Uint8Array | Error

function fromHost(data: Record<string, unknown>): void {
  window.dispatchEvent(
    new MessageEvent('message', {
      source: host,
      origin: HOST,
      data: { source: 'seuoffice', version: 1, ...data },
    }),
  )
}

/** A page the app drew, as large as the app chose */
function drawn(width: number, height: number): HTMLCanvasElement {
  const canvas = document.createElement('canvas')
  canvas.width = width
  canvas.height = height
  return canvas
}

async function answer(): Promise<Record<string, unknown>> {
  await vi.waitFor(() => expect(sent.some((m) => m.type === 'thumbnail_done')).toBe(true))
  return sent.find((m) => m.type === 'thumbnail_done')!
}

beforeEach(async () => {
  const frame = document.createElement('iframe')
  document.body.appendChild(frame)
  host = frame.contentWindow!
  sent = []
  vi.spyOn(host, 'postMessage').mockImplementation((message: unknown) => {
    sent.push(message as Record<string, unknown>)
  })
  Object.defineProperty(window, 'parent', { value: host, configurable: true })
  vi.stubGlobal('fetch', async () => new Response('not found', { status: 404 }))
  // jsdom draws nothing: a canvas that takes the calls, and an encoder each test sets
  encoded = new Uint8Array([137, 80, 78, 71])
  vi.spyOn(HTMLCanvasElement.prototype, 'getContext').mockImplementation(
    () => ({ fillRect() {}, drawImage() {} }) as unknown as CanvasRenderingContext2D,
  )
  vi.spyOn(HTMLCanvasElement.prototype, 'toBlob').mockImplementation((done) => {
    if (encoded instanceof Error) throw encoded
    done(new Blob([encoded as BlobPart], { type: 'image/png' }))
  })
  // the channel keeps one host per page load, so each test loads a fresh copy
  vi.resetModules()
  channel = await import('../src/embed-channel')
  const opening = channel.receiveOpen()
  fromHost({ type: 'open', fileName: 'report.pdf', bytes: new ArrayBuffer(1) })
  await opening
  sent = []
})

afterEach(() => {
  vi.useRealTimers()
  vi.restoreAllMocks()
  vi.unstubAllGlobals()
  Object.defineProperty(window, 'parent', { value: window, configurable: true })
  document.body.replaceChildren()
})

describe("answering the host's thumbnail", () => {
  it('sends the first page as a PNG of the asked width, as tall as the page is', async () => {
    const source = vi.fn<ThumbnailSource>(async ({ width }) => drawn(width, width * 1.5))
    channel.onHostThumbnailRequest(source)
    channel.reportOpened()
    fromHost({ type: 'thumbnail', requestId: 'a', width: 320 })
    const done = await answer()
    expect(done).toMatchObject({ requestId: 'a', ok: true, width: 320, height: 480 })
    expect(Array.from(new Uint8Array(done.bytes as ArrayBuffer))).toEqual([137, 80, 78, 71])
    expect(source.mock.calls[0]![0]).toMatchObject({ width: 320, maxHeight: 1280 })
  })

  it('scales a page the app drew at another width', async () => {
    channel.onHostThumbnailRequest(async () => drawn(200, 100))
    channel.reportOpened()
    fromHost({ type: 'thumbnail', requestId: 'a', width: 100 })
    expect(await answer()).toMatchObject({ ok: true, width: 100, height: 50 })
  })

  it.each([
    [undefined, 480],
    ['wide', 480],
    [Number.NaN, 480],
    [10, 64],
    [5000, 1024],
    [300.6, 301],
    [-1, 64],
  ])('reads a width of %s as %d pixels', async (asked, width) => {
    channel.onHostThumbnailRequest(async (request) => drawn(request.width, request.width))
    channel.reportOpened()
    fromHost({ type: 'thumbnail', requestId: 'a', width: asked })
    expect(await answer()).toMatchObject({ ok: true, width, height: width })
  })

  it('cuts a page taller than four widths off below', async () => {
    channel.onHostThumbnailRequest(async ({ width }) => drawn(width, width * 9))
    channel.reportOpened()
    fromHost({ type: 'thumbnail', requestId: 'a', width: 100 })
    expect(await answer()).toMatchObject({ ok: true, width: 100, height: 400 })
  })

  it('refuses with thumbnail_unsupported when the app draws no first page', async () => {
    channel.reportOpened()
    fromHost({ type: 'thumbnail', requestId: 'a' })
    expect(await answer()).toMatchObject({
      requestId: 'a',
      ok: false,
      code: 'thumbnail_unsupported',
    })
  })

  it('refuses with thumbnail_unsupported before the document is on screen', async () => {
    const source = vi.fn<ThumbnailSource>(async ({ width }) => drawn(width, width))
    channel.onHostThumbnailRequest(source)
    fromHost({ type: 'thumbnail', requestId: 'a' })
    expect(await answer()).toMatchObject({ ok: false, code: 'thumbnail_unsupported' })
    expect(source).not.toHaveBeenCalled()
  })

  it('refuses again once the app took its first page back', async () => {
    const takeBack = channel.onHostThumbnailRequest(async ({ width }) => drawn(width, width))
    channel.reportOpened()
    takeBack()
    fromHost({ type: 'thumbnail', requestId: 'a' })
    expect(await answer()).toMatchObject({ ok: false, code: 'thumbnail_unsupported' })
  })

  it('refuses an image over 2 MiB with thumbnail_too_large', async () => {
    channel.onHostThumbnailRequest(async ({ width }) => drawn(width, width))
    channel.reportOpened()
    encoded = new Uint8Array(2 * 1024 * 1024 + 1)
    fromHost({ type: 'thumbnail', requestId: 'big' })
    expect(await answer()).toMatchObject({ ok: false, code: 'thumbnail_too_large' })
    sent = []
    encoded = new Uint8Array(2 * 1024 * 1024)
    fromHost({ type: 'thumbnail', requestId: 'fits' })
    expect(await answer()).toMatchObject({ requestId: 'fits', ok: true })
  })

  it('refuses with thumbnail_tainted when the browser will not read the canvas', async () => {
    channel.onHostThumbnailRequest(async ({ width }) => drawn(width, width))
    channel.reportOpened()
    // what a browser throws; jsdom's own DOMException is no Error of this realm
    encoded = Object.assign(new Error('Tainted canvases may not be exported.'), {
      name: 'SecurityError',
    })
    fromHost({ type: 'thumbnail', requestId: 'a' })
    expect(await answer()).toMatchObject({ ok: false, code: 'thumbnail_tainted' })
  })

  it('refuses with thumbnail_timeout after 10 s and tells the app to stop', async () => {
    vi.useFakeTimers()
    let stopped: AbortSignal | undefined
    channel.onHostThumbnailRequest(({ signal }) => {
      stopped = signal
      return new Promise(() => {})
    })
    channel.reportOpened()
    fromHost({ type: 'thumbnail', requestId: 'a' })
    await vi.advanceTimersByTimeAsync(9_999)
    expect(sent.filter((m) => m.type === 'thumbnail_done')).toEqual([])
    expect(stopped!.aborted).toBe(false)
    await vi.advanceTimersByTimeAsync(1)
    expect(sent.find((m) => m.type === 'thumbnail_done')).toMatchObject({
      requestId: 'a',
      ok: false,
      code: 'thumbnail_timeout',
    })
    expect(stopped!.aborted).toBe(true)
  })

  it("refuses with thumbnail_unsupported and the app's reason when the drawing fails", async () => {
    channel.onHostThumbnailRequest(async () => {
      throw new Error('the page is damaged')
    })
    channel.reportOpened()
    fromHost({ type: 'thumbnail', requestId: 'a' })
    expect(await answer()).toMatchObject({
      ok: false,
      code: 'thumbnail_unsupported',
      message: 'the page is damaged',
    })
  })

  it('answers two requests each under its own id', async () => {
    channel.onHostThumbnailRequest(async ({ width }) => drawn(width, width))
    channel.reportOpened()
    fromHost({ type: 'thumbnail', requestId: 'small', width: 64 })
    fromHost({ type: 'thumbnail', requestId: 'large', width: 1024 })
    await vi.waitFor(() => expect(sent.filter((m) => m.type === 'thumbnail_done')).toHaveLength(2))
    const byId = Object.fromEntries(sent.map((m) => [m.requestId, m.width]))
    expect(byId).toEqual({ small: 64, large: 1024 })
  })
})
