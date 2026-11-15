import { readFileSync } from 'node:fs'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { createSubsetter } from '../../../../packages/pdf-structure/src/writeback/font'
import { fontCoversText } from '../../../../apps/pdf/src/main/font-cmap'

const wasm = readFileSync(new URL('../../../../apps/pdf/node_modules/harfbuzzjs/hb-subset.wasm', import.meta.url))
const font = readFileSync(new URL('../../../../packages/ui/src/fonts/Carlito-Regular.ttf', import.meta.url))

afterEach(() => { vi.unstubAllGlobals(); vi.resetModules() })

describe('HarfBuzz font subset worker', () => {
  it('matches the original subsetter for a real font and preserves requested glyphs', async () => {
    const { promise, resolve } = Promise.withResolvers<unknown>()
    const worker: { location: { href: string }; onmessage?: (event: { data: unknown }) => void; postMessage: (value: unknown) => void } = {
      location: { href: 'https://engine-host/pdf/font-subset-worker.js' },
      postMessage: value => resolve(value),
    }
    vi.stubGlobal('self', worker)
    vi.stubGlobal('fetch', async (url: URL) => {
      expect(url.href).toBe('https://engine-host/pdf/hb-subset.wasm')
      return { ok: true, arrayBuffer: async () => Uint8Array.from(wasm).buffer }
    })
    // The worker installs its handler against self when its module loads.
    await import('./font-subset-worker')
    worker.onmessage!({ data: { fontBase64: font.toString('base64'), text: 'Hello 세계 \u{1F600} ' } })
    const reply = await promise as { ok?: string; error?: { message: string } }
    expect(reply.error).toBeUndefined()
    expect(typeof reply.ok).toBe('string')
    const result = Buffer.from(reply.ok!, 'base64')
    const oracle = await createSubsetter(Uint8Array.from(wasm))
    expect(result).toEqual(oracle(font, 'Hello 세계 \u{1F600} '))
    expect(fontCoversText(result, 'Hello ')).toBe(true)
    expect(result.byteLength).toBeLessThan(font.byteLength)
  })

  it('reports malformed base64 rather than returning an empty font', async () => {
    const { promise, resolve } = Promise.withResolvers<unknown>()
    const worker: { location: { href: string }; onmessage?: (event: { data: unknown }) => void; postMessage: (value: unknown) => void } = {
      location: { href: 'https://engine-host/pdf/font-subset-worker.js' },
      postMessage: value => resolve(value),
    }
    vi.stubGlobal('self', worker)
    // The worker installs its handler against self when its module loads.
    await import('./font-subset-worker')
    worker.onmessage!({ data: { fontBase64: '!!', text: 'A' } })
    expect(await promise).toMatchObject({ error: { name: 'InvalidCharacterError' } })
  })
})
