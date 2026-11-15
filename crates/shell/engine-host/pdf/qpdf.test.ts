import { afterEach, describe, expect, it, vi } from 'vitest'
import { decryptPdf, encryptPdf, repairPdf } from './qpdf'

class FakeWorker {
  static latest: FakeWorker
  onerror: ((event: { message: string }) => void) | null = null
  onmessageerror: (() => void) | null = null
  onmessage: ((event: { data: unknown }) => void) | null = null
  terminate = vi.fn()
  postMessage = vi.fn()
  constructor(readonly url: URL) {
    FakeWorker.latest = this
  }
}

afterEach(() => vi.unstubAllGlobals())

describe('qpdf worker lifecycle', () => {
  it('terminates a running worker on cancellation and transfers source bytes', async () => {
    vi.stubGlobal('document', { baseURI: 'https://engine-host/' })
    vi.stubGlobal('Worker', FakeWorker)
    const controller = new AbortController()
    const input = new Uint8Array([1, 2, 3])
    const result = encryptPdf(input, 'password', undefined, controller.signal)
    expect(FakeWorker.latest.url.href).toBe('https://engine-host/pdf/qpdf-worker.js')
    expect(FakeWorker.latest.postMessage).toHaveBeenCalledWith(
      expect.objectContaining({ operation: 'encrypt', bytes: input.buffer }),
      [input.buffer],
    )
    controller.abort()
    await expect(result).rejects.toMatchObject({ name: 'AbortError' })
    expect(FakeWorker.latest.terminate).toHaveBeenCalledOnce()
  })

  it('terminates the worker after an error response', async () => {
    vi.stubGlobal('document', { baseURI: 'https://engine-host/' })
    vi.stubGlobal('Worker', FakeWorker)
    const result = encryptPdf(new Uint8Array([1]), 'password')
    FakeWorker.latest.onmessage?.({
      data: { error: { name: 'Error', message: 'qpdf could not write the file' } },
    })
    await expect(result).rejects.toThrow('qpdf could not write the file')
    expect(FakeWorker.latest.terminate).toHaveBeenCalledOnce()
  })

  it('preserves typed password and repair failures from the worker', async () => {
    vi.stubGlobal('document', { baseURI: 'https://engine-host/' })
    vi.stubGlobal('Worker', FakeWorker)
    const decrypt = decryptPdf(new Uint8Array([1]), 'wrong')
    FakeWorker.latest.onmessage?.({
      data: { error: { name: 'PdfLoadError', message: 'password', code: 'password-required' } },
    })
    await expect(decrypt).rejects.toMatchObject({ name: 'PdfLoadError', code: 'password-required' })
    const repair = repairPdf(new Uint8Array([1]))
    FakeWorker.latest.onmessage?.({
      data: { error: { name: 'RepairFailedError', message: 'unreadable', reason: 'unreadable' } },
    })
    await expect(repair).rejects.toMatchObject({ name: 'RepairFailedError', reason: 'unreadable' })
  })
})
