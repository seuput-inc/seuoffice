import { afterEach, expect, it, vi } from 'vitest'
import { runDiagramOpen } from './diagram-open-runner'

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

it('terminates only the cancelled parser and rejects its late reply', async () => {
  setup()
  const cancel = new AbortController()
  const first = runDiagramOpen({ fileName: 'first.drawio', bytesBase64: 'AQ==' }, cancel.signal)
  const second = runDiagramOpen(
    { fileName: 'second.drawio', bytesBase64: 'Ag==' },
    new AbortController().signal,
  )
  const [a, b] = FakeWorker.instances
  cancel.abort()
  a.onmessage?.({ data: { ok: true, value: { ok: true, xml: 'late' } } })
  const refusal = { ok: false, reason: 'no_diagram', error: 'not a diagram' }
  b.onmessage?.({ data: { ok: true, value: refusal } })
  await expect(first).rejects.toMatchObject({ name: 'AbortError' })
  await expect(second).resolves.toEqual(refusal)
  expect(a.terminate).toHaveBeenCalledOnce()
  expect(b.terminate).toHaveBeenCalledOnce()
  expect(b.postMessage).toHaveBeenCalledWith({ fileName: 'second.drawio', bytesBase64: 'Ag==' })
})

it('preserves a real model result without narrowing it', async () => {
  setup()
  const result = runDiagramOpen({}, new AbortController().signal)
  const model = {
    ok: true,
    xml: '<mxfile/>',
    format: 'drawio',
    root: 'mxfile',
    anyPageCompressed: true,
  }
  FakeWorker.instances[0].onmessage?.({ data: { ok: true, value: model } })
  await expect(result).resolves.toEqual(model)
  expect(FakeWorker.instances[0].terminate).toHaveBeenCalledOnce()
})

it('does not start a worker after cancellation and preserves worker errors', async () => {
  setup()
  const cancel = new AbortController()
  cancel.abort()
  expect(() => runDiagramOpen({}, cancel.signal)).toThrow()
  expect(FakeWorker.instances).toHaveLength(0)
  const result = runDiagramOpen({}, new AbortController().signal)
  FakeWorker.instances[0].onmessage?.({
    data: { ok: false, name: 'TypeError', message: 'bad input' },
  })
  await expect(result).rejects.toMatchObject({ name: 'TypeError', message: 'bad input' })
  expect(FakeWorker.instances[0].terminate).toHaveBeenCalledOnce()
})
