import { readFileSync } from 'node:fs'
import { createContext, runInContext } from 'node:vm'
import { afterEach, expect, it, vi } from 'vitest'

const source = readFileSync(new URL('./invoke-system.js', import.meta.url), 'utf8').replaceAll(
  '__INVOKE_KEY__',
  JSON.stringify('test-invoke-key'),
)

const NOT_COMMITTED = 'Renderer document is not committed yet.'
const NOT_READY = 'Renderer document authentication is not ready.'

afterEach(() => {
  vi.unstubAllGlobals()
  vi.resetModules()
})

const answer = (data, ok = true) =>
  new Response(JSON.stringify(data), {
    headers: { 'Tauri-Response': ok ? 'ok' : 'error', 'Content-Type': 'application/json' },
  })

function frame(callback, flags, index, body) {
  const bytes = new TextEncoder().encode(body)
  const buffer = new ArrayBuffer(17 + bytes.length)
  const view = new DataView(buffer)
  view.setUint32(0, callback, true)
  view.setUint8(4, flags)
  view.setBigUint64(5, BigInt(index), true)
  view.setUint32(13, bytes.length, true)
  new Uint8Array(buffer, 17).set(bytes)
  return buffer
}

function setup({
  bridge = 'preload',
  protocol = 'https:',
  ready = 'complete',
  route,
  subframe = false,
} = {}) {
  const requests = []
  const callbacks = []
  const listeners = new Map()
  const callbackMap = new Map()
  let nextCallback = 100
  let random = 0
  let registers = 0
  const internals = {
    convertFileSrc: (path, scheme) => `${scheme}://localhost/${encodeURIComponent(path)}`,
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
    unregisterCallback(id) {
      callbackMap.delete(id)
    },
    invoke(cmd, payload) {
      return new Promise((resolve, reject) => {
        const callback = internals.transformCallback(resolve, true)
        const error = internals.transformCallback(reject, true)
        internals.postMessage({ cmd, payload, callback, error })
      })
    },
  }
  const xhrs = []
  class FakeXhr {
    open(method, url, asynchronous) {
      this.call = { method, url, asynchronous, headers: {} }
      xhrs.push(this.call)
    }
    overrideMimeType() {}
    setRequestHeader(name, value) {
      this.call.headers[name] = value
    }
    send(body) {
      this.call.body = body
      const result = this.call.url.includes('seuoffice%3Aregister')
        ? { status: 200, text: JSON.stringify({ document: 9, bridge }) }
        : (route?.xhr?.(this.call) ?? { status: 200, text: '\u0000\u00ff\u0041' })
      this.result = result
    }
    get status() {
      return this.result.status
    }
    get responseText() {
      return this.result.text
    }
  }
  const window = {
    __TAURI_INTERNALS__: internals,
    Headers,
    XMLHttpRequest: FakeXhr,
    location: { protocol },
    document: { readyState: ready, fonts: { ready: Promise.resolve() } },
    crypto: {
      getRandomValues(array) {
        for (let index = 0; index < array.length; index += 1) array[index] = ++random & 255
        return array
      },
    },
    setTimeout: (callback, delay) => setTimeout(callback, delay),
    addEventListener: (type, listener) => listeners.set(type, listener),
    fetch: async (url, options) => {
      requests.push({ url, ...options })
      const routed = route?.fetch?.(url, options)
      if (routed) return routed
      if (url.endsWith('ipc_register')) {
        registers += 1
        return answer({ document: 10 + registers, bridge })
      }
      return answer(null)
    },
  }
  window.top = subframe ? {} : window
  const context = createContext({
    window,
    Response,
    Promise,
    Uint8Array,
    ArrayBuffer,
    DataView,
    Map,
    Headers,
    TextEncoder,
    TextDecoder,
  })
  runInContext(source, context)
  return { window, internals, requests, callbacks, listeners, xhrs, context }
}

const nonceOf = (request) => request.headers.get('x-seuoffice-doc')
const commands = (requests) => requests.map((request) => request.url.split('/').pop())

it('does nothing in a subframe, whose nonce would revoke the top document', async () => {
  const { requests } = setup({ subframe: true })
  await new Promise((resolve) => setTimeout(resolve, 50))
  expect(requests).toEqual([])
})

it('registers the document nonce once and sends it with every protected call', async () => {
  const { window, internals, requests, callbacks } = setup()
  await vi.waitFor(() => expect(commands(requests)).toContain('ipc_register'))
  const register = requests.find((request) => request.url.endsWith('ipc_register'))
  expect(nonceOf(register)).toMatch(/^[0-9a-f]{32}$/)
  expect(register.headers.get('Tauri-Invoke-Key')).toBe('test-invoke-key')
  const frameBytes = new Uint8Array([0, 255, 7])
  internals.postMessage({ cmd: 'ipc_invoke', callback: 1, error: 2, payload: frameBytes })
  await vi.waitFor(() => expect(commands(requests)).toContain('ipc_invoke'))
  const invoke = requests.find((request) => request.url.endsWith('ipc_invoke'))
  expect(nonceOf(invoke)).toBe(nonceOf(register))
  expect(invoke.headers.get('Tauri-Callback')).toBe('1')
  expect(invoke.headers.get('Content-Type')).toBe('application/octet-stream')
  expect([...invoke.body]).toEqual([...frameBytes])
  await vi.waitFor(() => expect(callbacks).toContainEqual([1, null]))
  expect(Object.values(window)).not.toContain(nonceOf(register))
  expect(Object.values(window)).not.toContain('test-invoke-key')
  expect(window.__seuofficeNativeBridgeBootstrap).toBeUndefined()
})

it('holds protected calls until registration and keeps them in order', async () => {
  let release
  const gate = new Promise((resolve) => {
    release = resolve
  })
  const { internals, requests } = setup({
    ready: 'loading',
    route: {
      fetch: (url) =>
        url.endsWith('ipc_register')
          ? gate.then(() => answer({ document: 5, bridge: 'invoke' }))
          : undefined,
    },
  })
  internals.postMessage({ cmd: 'ipc_send', callback: 1, error: 2, payload: new Uint8Array([1]) })
  internals.postMessage({ cmd: 'ipc_send', callback: 3, error: 4, payload: new Uint8Array([2]) })
  await vi.waitFor(() => expect(commands(requests)).toEqual(['ipc_register']))
  release()
  await vi.waitFor(() =>
    expect(commands(requests)).toEqual(['ipc_register', 'ipc_send', 'ipc_send']),
  )
  expect(requests.map((request) => request.headers.get('Tauri-Callback'))).toEqual(['0', '1', '3'])
})

it('retries registration while the document is not committed and then gives up on refusal', async () => {
  let attempts = 0
  const { internals, requests, callbacks } = setup({
    route: {
      fetch: (url) => {
        if (!url.endsWith('ipc_register')) return undefined
        attempts += 1
        return attempts < 3
          ? answer(NOT_COMMITTED, false)
          : answer('Renderer document has no bridge.', false)
      },
    },
  })
  internals.postMessage({ cmd: 'ipc_send', callback: 1, error: 2, payload: new Uint8Array() })
  await vi.waitFor(() => expect(callbacks).toEqual([[2, 'native document context is unavailable']]))
  expect(commands(requests)).toEqual(['ipc_register', 'ipc_register', 'ipc_register'])
})

it('retries a protected call only while Rust says authentication is not ready', async () => {
  let calls = 0
  const { internals, requests, callbacks } = setup({
    route: {
      fetch: (url) => {
        if (!url.endsWith('ipc_invoke')) return undefined
        calls += 1
        return calls < 3 ? answer(NOT_READY, false) : answer('refused', false)
      },
    },
  })
  internals.postMessage({ cmd: 'ipc_invoke', callback: 1, error: 2, payload: new Uint8Array() })
  await vi.waitFor(() => expect(callbacks).toEqual([[2, 'refused']]))
  expect(commands(requests).filter((name) => name === 'ipc_invoke')).toHaveLength(3)
})

it('answers the document id check and drops frames of another document', async () => {
  const { window, callbacks, internals, requests } = setup()
  await vi.waitFor(() => expect(commands(requests)).toContain('ipc_register'))
  internals.postMessage({ cmd: 'ipc_send', callback: 1, error: 2, payload: new Uint8Array() })
  await vi.waitFor(() => expect(callbacks).toHaveLength(1))
  expect(window.__seuofficeCheck(11)).toBe(true)
  expect(window.__seuofficeCheck(12)).toBe(false)
  window.__seuofficeDeliver(12, 70, true, new ArrayBuffer(0), undefined, false)
  expect(callbacks).toHaveLength(1)
  window.__seuofficeDeliver(11, 70, true, new ArrayBuffer(0), undefined, false)
  expect(callbacks).toHaveLength(2)
  expect(Object.getOwnPropertyDescriptor(window, '__seuofficeCheck')).toMatchObject({
    configurable: false,
    writable: false,
  })
})

it('pulls the outbox on wake and adapts Raw frames and Channel indexes', async () => {
  const outbox = new Uint8Array([
    ...new Uint8Array(frame(7, 1 | 2, 0, 'raw')),
    ...new Uint8Array(frame(7, 1, 1, '{"ok":true}')),
    ...new Uint8Array(frame(7, 1 | 4, 2, '')),
    ...new Uint8Array(frame(8, 0, 0, '"plain"')),
  ])
  const { window, callbacks, requests, internals } = setup({
    route: {
      fetch: (url) =>
        url.includes('outbox') ? new Response(outbox.buffer, { status: 200 }) : undefined,
    },
  })
  internals.postMessage({ cmd: 'ipc_send', callback: 1, error: 2, payload: new Uint8Array() })
  await vi.waitFor(() => expect(callbacks).toHaveLength(1))
  window.__seuofficeWake(12)
  expect(commands(requests)).not.toContain('outbox%2F11')
  window.__seuofficeWake(11)
  await vi.waitFor(() => expect(callbacks).toHaveLength(5))
  const pulled = requests.find((request) => request.url.includes('outbox'))
  expect(pulled.url).toBe('seuoffice-ipc://localhost/outbox%2F11')
  expect(pulled.method).toBe('GET')
  expect(pulled.headers.get('x-seuoffice-doc')).toMatch(/^[0-9a-f]{32}$/)
  const [, raw, message, end, plain] = callbacks
  expect(raw[0]).toBe(7)
  expect(new TextDecoder().decode(raw[1].message)).toBe('raw')
  expect(raw[1].index).toBe(0)
  expect(message).toEqual([7, { message: { ok: true }, index: 1 }])
  expect(end).toEqual([7, { end: true, index: 2 }])
  expect(plain).toEqual([8, 'plain'])
})

it('registers again with a fresh nonce when a page is restored from the cache', async () => {
  const { window, listeners, requests } = setup()
  await vi.waitFor(() => expect(window.__seuofficeCheck(11)).toBe(true))
  const first = nonceOf(requests.find((request) => request.url.endsWith('ipc_register')))
  listeners.get('pageshow')({ persisted: false })
  expect(requests.filter((request) => request.url.endsWith('ipc_register'))).toHaveLength(1)
  listeners.get('pageshow')({ persisted: true })
  await vi.waitFor(() => expect(window.__seuofficeCheck(12)).toBe(true))
  expect(window.__seuofficeCheck(11)).toBe(false)
  const registers = requests.filter((request) => request.url.endsWith('ipc_register'))
  expect(nonceOf(registers[1])).not.toBe(first)
})

it('reports render-ready once the document has loaded', async () => {
  const early = setup({ ready: 'loading' })
  await vi.waitFor(() => expect(early.window.__seuofficeCheck(11)).toBe(true))
  await new Promise((resolve) => setTimeout(resolve, 30))
  expect(commands(early.requests)).not.toContain('ipc_render_ready')
  early.listeners.get('load')()
  await vi.waitFor(() => expect(commands(early.requests)).toContain('ipc_render_ready'))
  const loaded = setup()
  await vi.waitFor(() => expect(commands(loaded.requests)).toContain('ipc_render_ready'))
})

it('never registers a blank document', async () => {
  const { requests, internals, callbacks } = setup({ protocol: 'about:' })
  internals.postMessage({ cmd: 'ipc_send', callback: 1, error: 2, payload: new Uint8Array() })
  await vi.waitFor(() => expect(callbacks).toEqual([[2, 'native document context is unavailable']]))
  expect(requests).toHaveLength(0)
})

it('sends sync requests with the nonce, and registers synchronously when still pending', () => {
  const { window, xhrs } = setup()
  const send = window.__seuofficeNativeSendSync
  expect([...send('safe', new Uint8Array([1]))]).toEqual([0, 255, 65])
  expect(xhrs[0]).toMatchObject({
    method: 'POST',
    url: 'seuoffice-ipc://localhost/seuoffice%3Aregister',
    asynchronous: false,
  })
  expect(xhrs[1].url).toBe('seuoffice-ipc://localhost/safe')
  expect(xhrs[1].headers['x-seuoffice-doc']).toBe(xhrs[0].headers['x-seuoffice-doc'])
  expect(window.__seuofficeCheck(9)).toBe(true)
})

it('keeps native denial distinct from a zero-byte sync answer', () => {
  const accepted = setup({ route: { xhr: () => ({ status: 200, text: '' }) } })
  expect(accepted.window.__seuofficeNativeSendSync('safe', new Uint8Array())).toHaveLength(0)
  const denied = setup({ route: { xhr: () => ({ status: 403, text: 'no' }) } })
  expect(() => denied.window.__seuofficeNativeSendSync('safe', new Uint8Array())).toThrow('refused')
})

it('exposes sync only to documents Rust registered as preload', async () => {
  const { window, requests } = setup({ bridge: 'invoke' })
  expect(window.__seuofficeNativeSendSync).toBeTypeOf('function')
  await vi.waitFor(() => expect(commands(requests)).toContain('ipc_register'))
  await vi.waitFor(() => expect(window.__seuofficeNativeSendSync).toBeUndefined())
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
})

it('rebinds only the output Channel before restored page traffic', async () => {
  const { window, internals, requests, listeners, callbacks } = setup()
  vi.stubGlobal('window', window)
  const { tauriTransport } = await import('./tauri')
  const transport = tauriTransport()
  transport.listen(() => {})
  await vi.waitFor(() => expect(commands(requests)).toContain('ipc_listen'))
  const first = requests.find((request) => request.url.endsWith('ipc_listen'))
  const firstChannel = JSON.parse(first.body).onFrame
  const firstId = Number(firstChannel.slice('__CHANNEL__:'.length))
  await vi.waitFor(() =>
    expect(callbacks.some(([id]) => id === Number(first.headers.get('Tauri-Callback')))).toBe(true),
  )
  window.crypto.getRandomValues = () => {
    throw new Error('authored crypto replacement ran')
  }
  internals.transformCallback = () => {
    throw new Error('authored callback registrar ran')
  }
  listeners.get('pageshow')({ persisted: true })
  await vi.waitFor(() =>
    expect(requests.filter((request) => request.url.endsWith('ipc_listen'))).toHaveLength(2),
  )
  const restored = requests.filter((request) => request.url.endsWith('ipc_listen'))[1]
  const restoredId = Number(JSON.parse(restored.body).onFrame.slice('__CHANNEL__:'.length))
  expect(restoredId).not.toBe(firstId)
  expect(internals.callbacks.has(firstId)).toBe(false)
  expect(nonceOf(restored)).not.toBe(nonceOf(first))
})

it('forwards an unclaimed contextmenu to ipc_context_menu and leaves claimed ones alone', async () => {
  const { window, listeners, requests } = setup()
  await vi.waitFor(() => expect(window.__seuofficeCheck(11)).toBe(true))
  window.getSelection = () => 'picked'
  const image = {
    nodeType: 1,
    tagName: 'IMG',
    currentSrc: 'genoffice-app-docs://localhost/a.png',
    isContentEditable: false,
  }
  let prevented = 0
  const event = (overrides) => ({
    target: image,
    clientX: 12,
    clientY: 34,
    defaultPrevented: false,
    preventDefault: () => (prevented += 1),
    ...overrides,
  })
  listeners.get('contextmenu')(event({ defaultPrevented: true }))
  expect(prevented).toBe(0)
  expect(commands(requests)).not.toContain('ipc_context_menu')
  listeners.get('contextmenu')(event())
  expect(prevented).toBe(1)
  await vi.waitFor(() => expect(commands(requests)).toContain('ipc_context_menu'))
  const sent = requests.find((request) => request.url.endsWith('ipc_context_menu'))
  expect(nonceOf(sent)).toBe(nonceOf(requests.find((r) => r.url.endsWith('ipc_register'))))
  expect(JSON.parse(sent.body)).toEqual({
    isEditable: false,
    selectionText: 'picked',
    srcUrl: 'genoffice-app-docs://localhost/a.png',
    mediaType: 'image',
    x: 12,
    y: 34,
    word: null,
  })
})
