import { afterEach, expect, it, vi } from 'vitest'
import { runHtml } from './html-runner'

class FakeWorker {
  static latest: FakeWorker
  onerror: ((event: { message: string }) => void) | null = null
  onmessageerror: (() => void) | null = null
  onmessage: ((event: { data: unknown }) => void) | null = null
  terminate = vi.fn()
  postMessage = vi.fn()
  constructor() { FakeWorker.latest = this }
}
afterEach(() => vi.unstubAllGlobals())

it('terminates the conversion worker and closes its page when cancelled', async () => {
  vi.stubGlobal('document', { baseURI: 'https://engine-host/' })
  vi.stubGlobal('Worker', FakeWorker)
  const call = vi.fn(async (op: string, _input: unknown) => op === 'html.open' ? { url: 'https://html-asset.localhost/chunk.html' } : null)
  const controller = new AbortController()
  const result = runHtml(true, controller.signal, call)
  await Promise.resolve()
  controller.abort()
  await expect(result).rejects.toMatchObject({ name: 'AbortError' })
  expect(FakeWorker.latest.postMessage).toHaveBeenCalledWith({ type: 'start', url: 'https://html-asset.localhost/chunk.html', naturalTableWidth: true })
  expect(FakeWorker.latest.terminate).toHaveBeenCalledOnce()
  expect(call).toHaveBeenCalledWith('html.close', null)
})

it('passes callback requests only to the owning page and keeps editor tables at default width', async () => {
  vi.stubGlobal('document', { baseURI: 'https://engine-host/' })
  vi.stubGlobal('Worker', FakeWorker)
  const call = vi.fn(async (op: string, _input: unknown) => op === 'html.open' ? { url: 'https://html-asset.localhost/file.html' } : 'answer')
  const result = runHtml(false, new AbortController().signal, call)
  await Promise.resolve()
  FakeWorker.latest.onmessage?.({ data: { type: 'callback', id: 4, operation: 'html.evaluate', input: { expression: '1' } } })
  await Promise.resolve()
  expect(call).toHaveBeenCalledWith('html.evaluate', { expression: '1' })
  expect(FakeWorker.latest.postMessage).toHaveBeenCalledWith({ type: 'callbackResult', id: 4, value: 'answer' })
  FakeWorker.latest.onmessage?.({ data: { type: 'result', result: { header: 1, buffers: [] } } })
  await expect(result).resolves.toEqual({ header: 1, buffers: [] })
  expect(FakeWorker.latest.terminate).toHaveBeenCalledOnce()
})
