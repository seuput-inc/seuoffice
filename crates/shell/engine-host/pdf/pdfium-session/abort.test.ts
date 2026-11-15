import { afterEach, expect, it, vi } from 'vitest'
import { runPdfiumSession } from '../pdfium-session'

afterEach(() => { vi.unstubAllGlobals() })

it('rejects an outstanding worker exchange when the request is aborted', async () => {
  const { promise: posted, resolve: postedResolve } = Promise.withResolvers<void>()
  let terminated = false
  class PendingWorker {
    onerror: unknown
    onmessageerror: unknown
    onmessage: unknown
    postMessage(): void { postedResolve() }
    terminate(): void { terminated = true }
  }
  vi.stubGlobal('Worker', PendingWorker)
  vi.stubGlobal('document', { baseURI: 'https://service/' })
  const controller = new AbortController()
  const session = runPdfiumSession({}, controller.signal, async () => ({ op: 'alloc', size: 4 }))
  await posted
  controller.abort()
  await expect(session).rejects.toMatchObject({ name: 'AbortError' })
  expect(terminated).toBe(true)
})
