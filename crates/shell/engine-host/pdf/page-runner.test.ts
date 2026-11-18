import { afterEach, beforeEach, expect, it, vi } from 'vitest'
import { runPdfPageOperation } from './page-runner'

class FakeWorker {
  static instances: FakeWorker[] = []
  static duringConstruction: (() => void) | undefined
  static postError: Error | undefined
  onmessage: ((event: MessageEvent) => void) | null = null
  onerror: ((event: ErrorEvent) => void) | null = null
  onmessageerror: ((event: MessageEvent) => void) | null = null
  postMessage = vi.fn(() => { if (FakeWorker.postError) throw FakeWorker.postError })
  terminate = vi.fn()
  constructor(readonly url: URL, readonly options: WorkerOptions) {
    FakeWorker.instances.push(this)
    FakeWorker.duringConstruction?.()
  }
  receive(data: unknown) { this.onmessage?.(new MessageEvent('message', { data })) }
}

let lifecycle: EventTarget
beforeEach(() => {
  FakeWorker.instances = []
  FakeWorker.duringConstruction = undefined
  FakeWorker.postError = undefined
  lifecycle = new EventTarget()
  vi.stubGlobal('document', { baseURI: 'https://engine-host/' })
  vi.stubGlobal('Worker', FakeWorker)
  vi.stubGlobal('addEventListener', lifecycle.addEventListener.bind(lifecycle))
  vi.stubGlobal('removeEventListener', lifecycle.removeEventListener.bind(lifecycle))
})
afterEach(() => vi.unstubAllGlobals())

it('creates one worker per request and preserves independent results', async () => {
  const first = runPdfPageOperation('pdf.pageCount', { bytesBase64: 'YQ==' }, new AbortController().signal)
  const second = runPdfPageOperation('pdf.applyPageEdits', { bytesBase64: 'Yg==', rotations: [], deletedPages: [] }, new AbortController().signal)
  expect(FakeWorker.instances).toHaveLength(2)
  const [a, b] = FakeWorker.instances
  expect(a!.url.href).toBe('https://engine-host/pdf/page-worker.js')
  expect(a!.options).toEqual({ type: 'module' })
  expect(b!.postMessage).toHaveBeenCalledWith({ name: 'pdf.applyPageEdits', input: { bytesBase64: 'Yg==', rotations: [], deletedPages: [] } })
  b!.receive({ type: 'result', result: 'YmFzZTY0' })
  a!.receive({ type: 'result', result: 3 })
  await expect(first).resolves.toBe(3)
  await expect(second).resolves.toBe('YmFzZTY0')
  expect(a!.terminate).toHaveBeenCalledOnce()
  expect(b!.terminate).toHaveBeenCalledOnce()
})

it('aborts a running worker and ignores a late result', async () => {
  const controller = new AbortController()
  const job = runPdfPageOperation('pdf.applyPageEdits', {}, controller.signal)
  const worker = FakeWorker.instances[0]!
  const late = worker.onmessage!
  controller.abort()
  await expect(job).rejects.toMatchObject({ name: 'AbortError' })
  late(new MessageEvent('message', { data: { type: 'result', result: 'late' } }))
  expect(worker.terminate).toHaveBeenCalledOnce()
  expect(worker.onmessage).toBeNull()
})

it('handles a signal aborted during worker construction before posting', async () => {
  const controller = new AbortController()
  FakeWorker.duringConstruction = () => controller.abort()
  const job = runPdfPageOperation('pdf.pageCount', {}, controller.signal)
  await expect(job).rejects.toMatchObject({ name: 'AbortError' })
  expect(FakeWorker.instances[0]!.postMessage).not.toHaveBeenCalled()
  expect(FakeWorker.instances[0]!.terminate).toHaveBeenCalledOnce()
})

it('does not create a worker when already cancelled', async () => {
  const controller = new AbortController()
  controller.abort()
  await expect(runPdfPageOperation('pdf.pageCount', {}, controller.signal)).rejects.toMatchObject({ name: 'AbortError' })
  expect(FakeWorker.instances).toHaveLength(0)
})

it('terminates pending work on pagehide', async () => {
  const job = runPdfPageOperation('pdf.pageCount', {}, new AbortController().signal)
  lifecycle.dispatchEvent(new Event('pagehide'))
  await expect(job).rejects.toMatchObject({ name: 'AbortError' })
  expect(FakeWorker.instances[0]!.terminate).toHaveBeenCalledOnce()
})

it('preserves structured operation errors and typed byte results', async () => {
  const first = runPdfPageOperation('pdf.pageCount', {}, new AbortController().signal)
  FakeWorker.instances[0]!.receive({ type: 'error', error: { name: 'PdfLoadError', message: 'password', code: 'password-required', reason: { attempt: 2 }, validRange: [1, 3] } })
  await expect(first).rejects.toMatchObject({ name: 'PdfLoadError', message: 'password', code: 'password-required', reason: { attempt: 2 }, validRange: [1, 3] })
  const second = runPdfPageOperation('pdf.inspectPdf', {}, new AbortController().signal)
  const result = { bytes: new Uint8Array([1, 2, 255]) }
  FakeWorker.instances[1]!.receive({ type: 'result', result })
  await expect(second).resolves.toEqual(result)
})

it('rejects malformed and empty messages and postMessage failures', async () => {
  const first = runPdfPageOperation('pdf.pageCount', {}, new AbortController().signal)
  FakeWorker.instances[0]!.receive({ type: 'result' })
  await expect(first).rejects.toThrow('PDF page worker returned an invalid message')
  expect(FakeWorker.instances[0]!.terminate).toHaveBeenCalledOnce()
  const second = runPdfPageOperation('pdf.pageCount', {}, new AbortController().signal)
  FakeWorker.instances[1]!.onmessageerror?.(new MessageEvent('messageerror'))
  await expect(second).rejects.toThrow('PDF page worker returned an invalid message')
  expect(FakeWorker.instances[1]!.terminate).toHaveBeenCalledOnce()
  FakeWorker.postError = new Error('clone failed')
  const third = runPdfPageOperation('pdf.pageCount', {}, new AbortController().signal)
  await expect(third).rejects.toThrow('clone failed')
  expect(FakeWorker.instances[2]!.terminate).toHaveBeenCalledOnce()
})

it('keeps qpdf and structure operations on their dedicated workers', async () => {
  for (const name of ['pdf.qpdfEncrypt', 'pdf.qpdfDecrypt', 'pdf.repair', 'pdf.structureOperation']) {
    await expect(runPdfPageOperation(name, {}, new AbortController().signal)).rejects.toThrow('requires its dedicated worker')
  }
  expect(FakeWorker.instances).toHaveLength(0)
})

it('worker rejects a duplicate request without sending a late result', async () => {
  vi.resetModules()
  const scope: { onmessage: ((event: MessageEvent) => Promise<void>) | null; postMessage: (message: unknown) => void } = {
    onmessage: null,
    postMessage: vi.fn(),
  }
  vi.stubGlobal('self', scope)
  // Each worker module must bind its handler to a fresh worker global.
  await import('./page-worker')
  expect(scope.onmessage).not.toBeNull()
  const event = new MessageEvent('message', { data: { name: 'pdf.isImage', input: { bytesBase64: 'AA==' } } })
  const first = scope.onmessage!(event)
  await scope.onmessage!(event)
  await first
  expect(scope.postMessage).toHaveBeenCalledWith({ type: 'error', error: { name: 'Error', message: 'PDF page worker received a duplicate request' } })
  expect(scope.postMessage).toHaveBeenCalledTimes(1)
})

it('worker delegates to the real page operation', async () => {
  vi.resetModules()
  const scope: { onmessage: ((event: MessageEvent) => Promise<void>) | null; postMessage: (message: unknown) => void } = {
    onmessage: null,
    postMessage: vi.fn(),
  }
  vi.stubGlobal('self', scope)
  // Each worker module must bind its handler to a fresh worker global.
  await import('./page-worker')
  await scope.onmessage!(new MessageEvent('message', { data: { name: 'pdf.isImage', input: { bytesBase64: 'AA==' } } }))
  expect(scope.postMessage).toHaveBeenCalledWith({ type: 'result', result: false })
})

it('worker refuses malformed and dedicated-worker requests', async () => {
  vi.resetModules()
  const scope: { onmessage: ((event: MessageEvent) => Promise<void>) | null; postMessage: (message: unknown) => void } = {
    onmessage: null,
    postMessage: vi.fn(),
  }
  vi.stubGlobal('self', scope)
  // Each worker module must bind its handler to a fresh worker global.
  await import('./page-worker')
  await scope.onmessage!(new MessageEvent('message', { data: { name: 'pdf.repair', input: {} } }))
  expect(scope.postMessage).toHaveBeenCalledWith({ type: 'error', error: { name: 'TypeError', message: 'invalid PDF page worker request' } })
})
