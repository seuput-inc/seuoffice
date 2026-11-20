import { afterEach, expect, it, vi } from 'vitest'
import { decode } from '../../bridge/wire'
import { runHwp } from '../docs-vendor'

class FakeWorker {
  static latest: FakeWorker
  onerror: ((event: { message: string }) => void) | null = null
  onmessageerror: (() => void) | null = null
  onmessage: ((event: { data: unknown }) => void) | null = null
  terminate = vi.fn()
  postMessage = vi.fn()
  constructor() {
    FakeWorker.latest = this
  }
}

afterEach(() => vi.unstubAllGlobals())

it('terminates the actual job when the request is cancelled', async () => {
  vi.stubGlobal('document', { baseURI: 'https://engine-host/' })
  vi.stubGlobal('Worker', FakeWorker)
  const controller = new AbortController()
  const result = runHwp({ bytesBase64: 'AQI=', format: 'hwp' }, controller.signal)
  controller.abort()
  await expect(result).rejects.toMatchObject({ name: 'AbortError' })
  expect(FakeWorker.latest.terminate).toHaveBeenCalledOnce()
})

it('preserves bytes and typed losses and terminates after success', async () => {
  vi.stubGlobal('document', { baseURI: 'https://engine-host/' })
  vi.stubGlobal('Worker', FakeWorker)
  const result = runHwp({ bytesBase64: 'AQI=', format: 'hwpx' }, new AbortController().signal)
  FakeWorker.latest.onmessage?.({
    data: {
      ok: true,
      bytes: new Uint8Array([9, 8]).buffer,
      losses: [{ feature: 'chart', kind: 'dropped', count: 2 }],
    },
  })
  const frame = (await result) as { ok: boolean; header: unknown; buffers: string[] }
  const buffers = frame.buffers.map((text) => Uint8Array.from(atob(text), (c) => c.charCodeAt(0)))
  expect(decode(frame.header, buffers)).toEqual({
    bytes: new Uint8Array([9, 8]),
    losses: [{ feature: 'chart', kind: 'dropped', count: 2 }],
  })
  expect(FakeWorker.latest.terminate).toHaveBeenCalledOnce()
})

it('preserves package refusal codes', async () => {
  vi.stubGlobal('document', { baseURI: 'https://engine-host/' })
  vi.stubGlobal('Worker', FakeWorker)
  const result = runHwp({ bytesBase64: '', format: 'hwp' }, new AbortController().signal)
  FakeWorker.latest.onmessage?.({ data: { ok: false, code: 'not-docx', error: 'bad document' } })
  await expect(result).resolves.toEqual({ ok: false, code: 'not-docx', error: 'bad document' })
  expect(FakeWorker.latest.terminate).toHaveBeenCalledOnce()
})

it('terminates a worker that crashes before replying', async () => {
  vi.stubGlobal('document', { baseURI: 'https://engine-host/' })
  vi.stubGlobal('Worker', FakeWorker)
  const result = runHwp({ bytesBase64: 'AQI=', format: 'hwp' }, new AbortController().signal)
  FakeWorker.latest.onerror?.({ message: 'worker crashed' })
  await expect(result).rejects.toThrow('worker crashed')
  expect(FakeWorker.latest.terminate).toHaveBeenCalledOnce()
})
