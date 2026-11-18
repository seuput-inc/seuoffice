import { afterEach, describe, expect, it, vi } from 'vitest'
import { subsetFont } from './font-subset'

class FakeWorker {
  static latest: FakeWorker
  onerror: ((event: { message: string }) => void) | null = null
  onmessageerror: (() => void) | null = null
  onmessage: ((event: { data: unknown }) => void) | null = null
  terminate = vi.fn()
  postMessage = vi.fn()
  constructor() { FakeWorker.latest = this }
}

function installWorker(): void {
  vi.stubGlobal('document', { baseURI: 'https://engine-host/' })
  vi.stubGlobal('Worker', FakeWorker)
  vi.stubGlobal('addEventListener', vi.fn())
  vi.stubGlobal('removeEventListener', vi.fn())
}

afterEach(() => vi.unstubAllGlobals())

describe('font subset service', () => {
  it('terminates running HarfBuzz work on cancellation', async () => {
    installWorker()
    const controller = new AbortController()
    const promise = subsetFont({ fontBase64: 'AQID', text: 'A \u{1F600}' }, controller.signal)
    expect(FakeWorker.latest.postMessage).toHaveBeenCalledWith({ fontBase64: 'AQID', text: 'A \u{1F600}' })
    controller.abort()
    await expect(promise).rejects.toMatchObject({ name: 'AbortError' })
    expect(FakeWorker.latest.terminate).toHaveBeenCalledOnce()
  })

  it('returns worker bytes and preserves failure messages', async () => {
    installWorker()
    const controller = new AbortController()
    const result = subsetFont({ fontBase64: 'AQID', text: 'A' }, controller.signal)
    FakeWorker.latest.onmessage?.({ data: { ok: 'BAUG' } })
    await expect(result).resolves.toEqual({ fontBase64: 'BAUG' })
    expect(FakeWorker.latest.terminate).toHaveBeenCalledOnce()
    const failed = subsetFont({ fontBase64: 'AQID', text: 'A' }, controller.signal)
    FakeWorker.latest.onmessage?.({ data: { error: { name: 'Error', message: 'hb_subset_or_fail failed (corrupt font?)' } } })
    await expect(failed).rejects.toThrow('hb_subset_or_fail failed (corrupt font?)')
    expect(FakeWorker.latest.terminate).toHaveBeenCalledOnce()
  })

  it('does not create a worker for an already cancelled request', async () => {
    installWorker()
    const controller = new AbortController()
    controller.abort()
    const before = FakeWorker.latest
    await expect(subsetFont({ fontBase64: 'AQID', text: 'A' }, controller.signal)).rejects.toMatchObject({ name: 'AbortError' })
    expect(FakeWorker.latest).toBe(before)
  })
})
