import { afterEach, describe, expect, it, vi } from 'vitest'
import { createRequestCallbacks } from './transport'

afterEach(() => vi.unstubAllGlobals())

describe('request-bound native callbacks', () => {
  it('includes the request identity, nonce and a fresh callback identity', async () => {
    const invoke = vi.fn().mockResolvedValue('ok')
    vi.stubGlobal('window', { __SEUOFFICE_ENGINE_TOKEN__: 'service-token', __TAURI_INTERNALS__: { invoke } })
    const call = createRequestCallbacks(7, 'docs.htmlToDocx', 'request-secret', new AbortController().signal)
    expect(await call('html.open', null)).toBe('ok')
    await call('html.evaluate', { expression: '1' })
    expect(invoke.mock.calls[0]).toEqual(['engine_host_callback', {
      token: 'service-token', id: 7, name: 'docs.htmlToDocx', nonce: 'request-secret', callbackId: 1,
      operation: 'html.open', input: null,
    }])
    expect(invoke.mock.calls[1]?.[1]).toMatchObject({ callbackId: 2, operation: 'html.evaluate' })
  })

  it('does not deliver a callback result after cancellation', async () => {
    const { promise, resolve } = Promise.withResolvers<unknown>()
    const invoke = vi.fn(() => promise)
    vi.stubGlobal('window', { __SEUOFFICE_ENGINE_TOKEN__: 'service-token', __TAURI_INTERNALS__: { invoke } })
    const controller = new AbortController()
    const pending = createRequestCallbacks(7, 'docs.htmlToDocx', 'request-secret', controller.signal)('html.open', null)
    controller.abort()
    resolve('late')
    await expect(pending).rejects.toMatchObject({ name: 'AbortError' })
  })
})
