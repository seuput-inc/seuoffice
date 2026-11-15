import { afterEach, expect, it, vi } from 'vitest'
import { runLegacyDoc } from './legacy-doc-runner'

class FakeWorker {
  static instances: FakeWorker[] = []
  onerror: ((event: { message: string }) => void) | null = null
  onmessageerror: (() => void) | null = null
  onmessage: ((event: { data: unknown }) => void) | null = null
  terminate = vi.fn()
  postMessage = vi.fn()
  constructor() {
    FakeWorker.instances.push(this)
  }
}

afterEach(() => {
  vi.unstubAllGlobals()
  FakeWorker.instances = []
})
function setup() {
  vi.stubGlobal('document', { baseURI: 'https://engine-host/' })
  vi.stubGlobal('Worker', FakeWorker)
}

it('terminates only the cancelled conversion and ignores its late reply', async () => {
  setup()
  const controller = new AbortController()
  const first = runLegacyDoc({ bytesBase64: 'AQ==' }, controller.signal)
  const second = runLegacyDoc({ bytesBase64: 'Ag==' }, new AbortController().signal)
  const [a, b] = FakeWorker.instances
  controller.abort()
  a.onmessage?.({ data: { ok: true, value: { docx: 'late', warnings: [] } } })
  b.onmessage?.({ data: { ok: true, value: { docx: 'Ag==', warnings: ['loss'] } } })
  await expect(first).rejects.toMatchObject({ name: 'AbortError' })
  await expect(second).resolves.toEqual({ docx: 'Ag==', warnings: ['loss'] })
  expect(a.terminate).toHaveBeenCalledOnce()
  expect(b.terminate).toHaveBeenCalledOnce()
  expect(b.postMessage).toHaveBeenCalledWith({ bytesBase64: 'Ag==' })
})

it('does not create a worker for an already cancelled request', () => {
  setup()
  const controller = new AbortController()
  controller.abort()
  expect(() => runLegacyDoc({}, controller.signal)).toThrow()
  expect(FakeWorker.instances).toHaveLength(0)
})

it('preserves conversion error name and terminates the realm', async () => {
  setup()
  const result = runLegacyDoc({}, new AbortController().signal)
  const worker = FakeWorker.instances[0]
  worker.onmessage?.({ data: { ok: false, name: 'TypeError', message: 'bad input' } })
  await expect(result).rejects.toMatchObject({ name: 'TypeError', message: 'bad input' })
  expect(worker.terminate).toHaveBeenCalledOnce()
})

it('rejects a crashed worker and terminates it', async () => {
  setup()
  const result = runLegacyDoc({}, new AbortController().signal)
  const worker = FakeWorker.instances[0]
  worker.onerror?.({ message: 'worker crashed' })
  await expect(result).rejects.toThrow('worker crashed')
  expect(worker.terminate).toHaveBeenCalledOnce()
})
