import { createRequire } from 'node:module'
import { dirname, join } from 'node:path'
import { pathToFileURL } from 'node:url'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { STUDIO_HANDSHAKE_MS } from '../src/renderer/studio-settings'

const require = createRequire(import.meta.url)

interface Transport {
  connect(): Promise<void>
  request(method: string): Promise<unknown>
  destroy(): void
}
type TransportClass = new (
  iframe: { contentWindow: unknown },
  studioUrl: string,
  options: Record<string, unknown>,
) => Transport

interface Envelope {
  type: string
  version: number
  sessionId: string
  id?: number
  capabilities?: string[]
}

/**
 * The SDK's transport, the module the app's `createEditor` connects through. It is loaded by
 * its file: the package declares no types and no export for it, so a static import of
 * `@rhwp/editor/transport.js` does not typecheck (the scheme patch test reads it the same way).
 */
async function transportClass(): Promise<TransportClass> {
  const file = join(dirname(require.resolve('@rhwp/editor')), 'transport.js')
  const module = (await import(pathToFileURL(file).href)) as { EditorTransport: TransportClass }
  return module.EditorTransport
}

/**
 * The studio's side of the embed protocol (rhwp-studio `embed/runtime.ts`), as far as the
 * handshake goes: a connect message binds its port once and is answered on it, and once a port
 * is bound every message that comes without one (the legacy transport) is dropped. `busyMs` is
 * how long the studio takes to get to its messages, on the test's fake clock.
 */
function studioFrame(busyMs: number) {
  let bound = false
  return {
    contentWindow: {
      postMessage(message: Envelope, _origin: string, transfer: MessagePort[] = []) {
        setTimeout(() => {
          const port = transfer[0]
          if (message.type !== 'rhwp-connect' || !port || bound) return
          bound = true
          port.onmessage = (event: MessageEvent<Envelope>) => {
            const request = event.data
            port.postMessage({
              type: 'rhwp-response',
              version: request.version,
              sessionId: request.sessionId,
              id: request.id,
              result: true,
            })
          }
          port.postMessage({
            type: 'rhwp-connected',
            version: message.version,
            sessionId: message.sessionId,
            capabilities: message.capabilities,
          })
        }, busyMs)
      },
    },
  }
}

const hostWindow = { addEventListener() {}, removeEventListener() {} }
const STUDIO_URL = 'http://studio.test/index.html'
/** the SDK's own handshake time */
const SDK_HANDSHAKE_MS = 1000
/** a studio busy for longer than that */
const BUSY_MS = 1300
const REQUEST_TIMEOUT_MS = 600

describe('the studio handshake', () => {
  beforeEach(() => {
    vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] })
  })
  afterEach(() => {
    vi.useRealTimers()
  })

  it("is lost for good when the studio is busy past the SDK's own second", async () => {
    const EditorTransport = await transportClass()
    const transport = new EditorTransport(studioFrame(BUSY_MS), STUDIO_URL, {
      window: hostWindow,
      requestTimeoutMs: REQUEST_TIMEOUT_MS,
    })
    try {
      const connected = transport.connect()
      // the SDK gives up on the port and resolves through the legacy fallback
      await vi.advanceTimersByTimeAsync(SDK_HANDSHAKE_MS)
      await connected
      const answered = expect(transport.request('ready')).rejects.toThrow(/Request timeout/)
      // the studio binds the closed port, then drops the legacy request; nothing ever answers
      await vi.advanceTimersByTimeAsync(BUSY_MS + REQUEST_TIMEOUT_MS)
      await answered
    } finally {
      transport.destroy()
    }
  })

  it('completes with the time the app gives it', async () => {
    expect(STUDIO_HANDSHAKE_MS).toBeGreaterThan(BUSY_MS)
    const EditorTransport = await transportClass()
    const transport = new EditorTransport(studioFrame(BUSY_MS), STUDIO_URL, {
      window: hostWindow,
      requestTimeoutMs: REQUEST_TIMEOUT_MS,
      handshakeTimeoutMs: STUDIO_HANDSHAKE_MS,
    })
    try {
      const connected = transport.connect()
      await vi.advanceTimersByTimeAsync(BUSY_MS)
      // the studio's answer arrives over the real port: no clock is involved from here on
      await connected
      await expect(transport.request('ready')).resolves.toBe(true)
    } finally {
      transport.destroy()
    }
  })
})
