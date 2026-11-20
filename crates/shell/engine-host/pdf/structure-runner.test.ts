import { afterEach, expect, it, vi } from 'vitest'
import { runStructureTree, type NativeOcrRequest } from './structure-runner'
import type { OcrRecognition } from '../../../../packages/pdf2docx/src/ocr'
import { pdfStructureService } from '../pdf'

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

it('revokes a pending OCR request and ignores a late native result', async () => {
  vi.stubGlobal('document', { baseURI: 'https://engine-host/' })
  vi.stubGlobal('Worker', FakeWorker)
  const pending = Promise.withResolvers<OcrRecognition | null>()
  const controller = new AbortController()
  let nativeRequest: NativeOcrRequest | undefined
  const job = runStructureTree({
    operation: 'tree',
    bytes: new Uint8Array([1]),
    ocr: (request) => {
      nativeRequest = request
      return pending.promise
    },
    onProgress: () => {},
    signal: controller.signal,
  })
  expect(FakeWorker.latest.url.href).toBe('https://engine-host/pdf/structure-worker.js')
  const Growable = SharedArrayBuffer as unknown as new (
    length: number,
    options: { maxByteLength: number },
  ) => SharedArrayBuffer
  const response = new Growable(8, { maxByteLength: 1024 })
  FakeWorker.latest.onmessage?.({
    data: {
      type: 'ocr',
      requestId: 1,
      png: new ArrayBuffer(1),
      page: { widthPt: 100, heightPt: 100 },
      response,
    },
  })
  await Promise.resolve()
  controller.abort()
  await expect(job).rejects.toMatchObject({ name: 'AbortError' })
  expect(nativeRequest?.signal.aborted).toBe(true)
  expect(Atomics.load(new Int32Array(response), 0)).toBe(2)
  pending.resolve({ lines: [] })
  await Promise.resolve()
  await Promise.resolve()
  expect(response.byteLength).toBe(8)
  expect(Atomics.load(new Int32Array(response), 0)).toBe(2)
  expect(FakeWorker.latest.terminate).toHaveBeenCalledOnce()
})

it('wakes the worker with a failure when native OCR rejects', async () => {
  vi.stubGlobal('document', { baseURI: 'https://engine-host/' })
  vi.stubGlobal('Worker', FakeWorker)
  const controller = new AbortController()
  const job = runStructureTree({
    operation: 'tree',
    bytes: new Uint8Array([1]),
    ocr: async () => {
      throw new Error('OCR failed')
    },
    onProgress: () => {},
    signal: controller.signal,
  })
  const Growable = SharedArrayBuffer as unknown as new (
    length: number,
    options: { maxByteLength: number },
  ) => SharedArrayBuffer
  const response = new Growable(8, { maxByteLength: 1024 })
  FakeWorker.latest.onmessage?.({
    data: {
      type: 'ocr',
      requestId: 1,
      png: new ArrayBuffer(1),
      page: { widthPt: 100, heightPt: 100 },
      response,
    },
  })
  await Promise.resolve()
  await Promise.resolve()
  expect(Atomics.load(new Int32Array(response), 0)).toBe(2)
  controller.abort()
  await expect(job).rejects.toMatchObject({ name: 'AbortError' })
})

it('cancels native OCR and terminates the worker on timeout', async () => {
  vi.useFakeTimers()
  try {
    vi.stubGlobal('document', { baseURI: 'https://engine-host/' })
    vi.stubGlobal('Worker', FakeWorker)
    const controller = new AbortController()
    let nativeRequest: NativeOcrRequest | undefined
    const job = runStructureTree({
      operation: 'tree',
      bytes: new Uint8Array([1]),
      ocr: (request) => {
        nativeRequest = request
        return new Promise(() => {})
      },
      onProgress: () => {},
      signal: controller.signal,
    })
    const Growable = SharedArrayBuffer as unknown as new (
      length: number,
      options: { maxByteLength: number },
    ) => SharedArrayBuffer
    const response = new Growable(8, { maxByteLength: 1024 })
    FakeWorker.latest.onmessage?.({
      data: {
        type: 'ocr',
        requestId: 1,
        png: new ArrayBuffer(1),
        page: { widthPt: 100, heightPt: 100 },
        response,
      },
    })
    await Promise.resolve()
    const rejected = expect(job).rejects.toThrow('native OCR request timed out')
    await vi.advanceTimersByTimeAsync(30_000)
    await rejected
    expect(nativeRequest?.signal.aborted).toBe(true)
    expect(FakeWorker.latest.terminate).toHaveBeenCalledOnce()
  } finally {
    vi.useRealTimers()
  }
})

it('serializes result bytes and waits for native progress before replying', async () => {
  vi.stubGlobal('document', { baseURI: 'https://engine-host/' })
  vi.stubGlobal('Worker', FakeWorker)
  const progress = Promise.withResolvers<unknown>()
  const callback = vi.fn(() => progress.promise)
  const result = pdfStructureService(
    { operation: 'images', bytesBase64: 'AQ==', ocrAvailable: false },
    new AbortController().signal,
    callback,
  )
  FakeWorker.latest.onmessage?.({ data: { type: 'progress', done: 1, total: 2 } })
  FakeWorker.latest.onmessage?.({
    data: { type: 'result', result: { png: new Uint8Array([1, 2]) } },
  })
  let settled = false
  void result.then(() => {
    settled = true
  })
  await Promise.resolve()
  expect(settled).toBe(false)
  progress.resolve(null)
  expect(await result).toEqual({
    ok: true,
    header: { png: { $: 'b', k: 'u8', i: 0 } },
    buffersBase64: ['AQI='],
  })
  expect(callback).toHaveBeenCalledWith('progress', { done: 1, total: 2 })
})

it('publishes a normalized OCR result into the page response buffer', async () => {
  vi.stubGlobal('document', { baseURI: 'https://engine-host/' })
  vi.stubGlobal('Worker', FakeWorker)
  const controller = new AbortController()
  const job = runStructureTree({
    operation: 'tree',
    bytes: new Uint8Array([1]),
    ocr: async () => ({ lines: [], paperShare: 0.75 }),
    onProgress: () => {},
    signal: controller.signal,
  })
  const Growable = SharedArrayBuffer as unknown as new (
    length: number,
    options: { maxByteLength: number },
  ) => SharedArrayBuffer
  const response = new Growable(8, { maxByteLength: 1024 })
  FakeWorker.latest.onmessage?.({
    data: {
      type: 'ocr',
      requestId: 1,
      png: new ArrayBuffer(1),
      page: { widthPt: 100, heightPt: 100 },
      response,
    },
  })
  await Promise.resolve()
  await Promise.resolve()
  await Promise.resolve()
  const control = new Int32Array(response, 0, 2)
  expect(Atomics.load(control, 0)).toBe(1)
  expect(
    JSON.parse(new TextDecoder().decode(new Uint8Array(response, 8, Atomics.load(control, 1)))),
  ).toEqual({ lines: [], paperShare: 0.75 })
  controller.abort()
  await expect(job).rejects.toMatchObject({ name: 'AbortError' })
})
