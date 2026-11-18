import { readFileSync } from 'node:fs'
import { createContext, runInContext } from 'node:vm'
import { afterEach, expect, it, vi } from 'vitest'

const source = readFileSync(new URL('./invoke-system.js', import.meta.url), 'utf8')
  .replaceAll('__INVOKE_KEY__', JSON.stringify('test-invoke-key'))

afterEach(() => {
  vi.unstubAllGlobals()
  vi.resetModules()
})

function setup(mode = 'preload', bootstrapOverride) {
  const requests = []
  const callbacks = []
  let listener
  let resume
  let nextCallback = 100
  let random = 100000
  const callbackMap = new Map()
  const internals = {
    convertFileSrc: (path, protocol) => `${protocol}://localhost/${path}`,
    callbacks: callbackMap,
    runCallback(id, value) {
      callbacks.push([id, value])
      callbackMap.get(id)?.(value)
    },
    transformCallback(callback, once) {
      const id = ++nextCallback
      callbackMap.set(id, (value) => {
        if (once) callbackMap.delete(id)
        callback(value)
      })
      return id
    },
    unregisterCallback(id) { callbackMap.delete(id) },
    invoke(cmd, payload) {
      return new Promise((resolve, reject) => {
        const callback = internals.transformCallback(resolve, true)
        const error = internals.transformCallback(reject, true)
        internals.postMessage({ cmd, payload, callback, error })
      })
    },
  }
  const native = {
    mode,
    invokeTauri(cmd, callback, error, contentType, key, body, headers) {
      requests.push({ cmd, callback, error, contentType, key, body, headers })
    },
    listenTauri(callback, onResume) { listener = callback; resume = onResume },
    sendSync(channel, body) {
      requests.push({ channel, body })
      return new Uint8Array([0, 255, 65]).buffer
    },
  }
  const window = {
    __TAURI_INTERNALS__: internals,
    Headers,
    crypto: { getRandomValues(array) { array[0] = ++random; return array } },
    fetch: async (url, options) => {
      requests.push({ url, ...options })
      return new Response('null', { headers: { 'Tauri-Response': 'ok', 'Content-Type': 'application/json' } })
    },
  }
  if (mode !== false) window.__seuofficeNativeBridgeBootstrap =
    bootstrapOverride || (() => native)
  const context = createContext({ window, Response, Promise, Uint8Array, ArrayBuffer, Map, Headers, TextEncoder, TextDecoder })
  runInContext(source, context)
  return { window, internals, requests, callbacks, native, deliver: (...args) => listener(...args), resume: () => resume(), context }
}

it('sends native Raw bytes and JSON Channel payload without a bearer header', () => {
  const { window, internals, requests } = setup()
  const frame = new Uint8Array([0, 255, 7])
  internals.postMessage({ cmd: 'ipc_invoke', callback: 1, error: 2, payload: frame })
  internals.postMessage({ cmd: 'ipc_listen', callback: 3, error: 4, payload: { onFrame: { toJSON: () => '__CHANNEL__:42' } }, options: { headers: { Example: 'value' } } })
  expect(requests[0]).toMatchObject({ cmd: 'ipc_invoke', callback: 1, error: 2, key: 'test-invoke-key', contentType: 'application/octet-stream' })
  expect([...requests[0].body]).toEqual([...frame])
  expect(JSON.parse(new TextDecoder().decode(requests[1].body))).toEqual({ onFrame: '__CHANNEL__:42' })
  expect(JSON.parse(requests[1].headers)).toEqual([['example', 'value']])
  expect(window.__seuofficeNativeBridgeBootstrap).toBeUndefined()
  expect(Object.values(window)).not.toContain('test-invoke-key')
})

it('preserves typed-array byte offsets and empty Raw responses', () => {
  const { internals, requests, deliver, callbacks } = setup()
  const body = new Uint8Array([1, 2, 3, 4]).subarray(1, 3)
  internals.postMessage({ cmd: 'ipc_send', callback: 1, error: 2, payload: body })
  expect([...requests[0].body]).toEqual([2, 3])
  const empty = new ArrayBuffer(0)
  deliver(1, true, empty, undefined, false)
  expect(callbacks).toEqual([[1, empty]])
})

it('adapts native Channel indexes without encoding Raw frames as JSON', () => {
  const { deliver, callbacks } = setup()
  const frame = new Uint8Array([0, 255]).buffer
  deliver(7, true, frame, 0n, false)
  deliver(7, false, new TextEncoder().encode('{"ok":true}').buffer, 1n, false)
  deliver(7, false, new ArrayBuffer(0), 2n, true)
  expect(callbacks).toEqual([[7, { message: frame, index: 0 }], [7, { message: { ok: true }, index: 1 }], [7, { end: true, index: 2 }]])
  expect(() => deliver(7, true, frame, 9007199254740992n, false)).toThrow('safe integer')
  expect(callbacks).toHaveLength(3)
})

it('uses captured native methods when authored scripts replace page transports', () => {
  const { window, internals, native, requests } = setup()
  const stolen = vi.fn()
  window.fetch = stolen
  window.Headers = stolen
  window.XMLHttpRequest = stolen
  native.invokeTauri = stolen
  native.sendSync = stolen
  internals.postMessage({ cmd: 'ipc_send', callback: 1, error: 2, payload: new Uint8Array([1]) })
  expect([...window.__seuofficeNativeSendSync('safe', new Uint8Array())]).toEqual([0, 255, 65])
  expect(requests).toHaveLength(2)
  expect(stolen).not.toHaveBeenCalled()
})

it('rejects protected calls without native admission while retaining engine nonce transport', async () => {
  const { window, internals, requests, callbacks } = setup(false)
  expect(window.__seuofficeNativeSendSync).toBeUndefined()
  internals.postMessage({ cmd: 'ipc_invoke', callback: 1, error: 2, payload: new Uint8Array() })
  expect(callbacks).toEqual([[2, 'native document context is unavailable']])
  internals.postMessage({ cmd: 'engine_host_ready', callback: 3, error: 4, payload: {} })
  expect(requests).toHaveLength(1)
  expect(requests[0].headers.get('x-seuoffice-native-context')).toBeNull()
  await vi.waitFor(() => expect(callbacks).toHaveLength(2))
})

it('captures and deletes the sync closure before exposing the preload bridge', async () => {
  const { window, internals } = setup()
  internals.transformCallback = () => 1
  internals.unregisterCallback = () => {}
  const send = window.__seuofficeNativeSendSync
  vi.stubGlobal('window', window)
  const { tauriTransport } = await import('./tauri')
  const transport = tauriTransport()
  expect(window.__seuofficeNativeSendSync).toBeUndefined()
  expect(transport.sendSync).toBe(send)
  expect(send.__seuofficeResumeChannel).toBeUndefined()
  expect([...transport.sendSync('safe', new Uint8Array())]).toEqual([0, 255, 65])
})

it('keeps native denial distinct from successful zero-byte sync responses', () => {
  const accepted = setup('preload', () => ({
    mode: 'preload', invokeTauri() {}, listenTauri() {}, sendSync: () => new ArrayBuffer(0),
  }))
  expect(accepted.window.__seuofficeNativeSendSync('safe', new Uint8Array())).toHaveLength(0)
  const denied = setup('preload', () => ({
    mode: 'preload', invokeTauri() {}, listenTauri() {}, sendSync() { throw new Error('Native synchronous request refused') },
  }))
  expect(() => denied.window.__seuofficeNativeSendSync('safe', new Uint8Array())).toThrow('refused')
})

it('fails malformed initialization and never exposes sync in invoke-only mode', () => {
  expect(() => setup('preload', () => { throw new Error('native bootstrap failed') })).toThrow('native bootstrap failed')
  expect(() => setup('preload', () => {})).toThrow('native document context was not initialized')
  expect(() => setup('untrusted')).toThrow('native document context was not initialized')
  expect(setup('invoke').window.__seuofficeNativeSendSync).toBeUndefined()
})

it('does not fall back to page XHR when the native sync closure is missing', async () => {
  const { window, internals } = setup(false)
  internals.transformCallback = () => 1
  internals.unregisterCallback = () => {}
  window.XMLHttpRequest = vi.fn()
  vi.stubGlobal('window', window)
  const { tauriTransport } = await import('./tauri')
  expect(() => tauriTransport()).toThrow('private synchronous transport')
  expect(window.XMLHttpRequest).not.toHaveBeenCalled()
})

it('rebinds only the output Channel before restored page traffic', async () => {
  const { window, internals, requests, deliver, resume } = setup()
  vi.stubGlobal('window', window)
  const { tauriTransport } = await import('./tauri')
  const transport = tauriTransport()
  const frames = []
  transport.listen((frame) => frames.push([...frame]))
  await vi.waitFor(() => expect(requests).toHaveLength(1))
  const firstChannel = JSON.parse(new TextDecoder().decode(requests[0].body)).onFrame
  const firstId = Number(firstChannel.slice('__CHANNEL__:'.length))
  deliver(firstId, true, new Uint8Array([1]).buffer, 0n, false)
  deliver(firstId, true, new Uint8Array([99]).buffer, 2n, false)
  deliver(requests[0].callback, false, new TextEncoder().encode('null').buffer, undefined, false)
  expect(frames).toEqual([[1]])
  internals.postMessage({ cmd: 'ipc_send', callback: 10, error: 11, payload: new Uint8Array([42]) })
  window.crypto.getRandomValues = () => { throw new Error('authored crypto replacement ran') }
  internals.transformCallback = () => { throw new Error('authored callback registrar ran') }
  expect(resume()).toBe(true)
  expect(requests.map((request) => request.cmd)).toEqual(['ipc_listen', 'ipc_send', 'ipc_listen'])
  const restored = requests[2]
  const restoredChannel = JSON.parse(new TextDecoder().decode(restored.body)).onFrame
  const restoredId = Number(restoredChannel.slice('__CHANNEL__:'.length))
  expect(restoredId).not.toBe(firstId)
  expect(internals.callbacks.has(firstId)).toBe(false)
  deliver(restoredId, true, new Uint8Array([2]).buffer, 0n, false)
  deliver(restoredId, true, new Uint8Array([3]).buffer, 1n, false)
  expect(frames).toEqual([[1], [2], [3]])
  expect(window.__seuofficeNativeSendSync).toBeUndefined()
  expect(transport.sendSync.__seuofficeResumeChannel).toBeUndefined()
})

it('does not acknowledge restore when preload initialization lacks its private Channel binding', () => {
  const { resume } = setup()
  expect(() => resume()).toThrow('never initialized')
  expect(setup('invoke').resume()).toBe(true)
})
