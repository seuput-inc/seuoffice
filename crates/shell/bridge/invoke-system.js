// Tauri Builder::invoke_system substitutes this token before document start.
;(function () {
  const invokeKey = __INVOKE_KEY__
  const internals = window.__TAURI_INTERNALS__
  // Tauri installs these non-configurable functions after invoke_system.
  const convertFileSrc = (path, protocol) => internals.convertFileSrc(path, protocol)
  const runCallback = (id, value) => internals.runCallback(id, value)
  const fetchNative = window.fetch.bind(window)
  const HeadersNative = window.Headers
  const headersSet = HeadersNative.prototype.set
  const headersGet = HeadersNative.prototype.get
  const responseJson = Response.prototype.json
  const responseText = Response.prototype.text
  const responseBuffer = Response.prototype.arrayBuffer
  const promiseThen = Promise.prototype.then
  const apply = Reflect.apply
  const stringify = JSON.stringify
  const textEncoder = new TextEncoder()
  const encodeText = TextEncoder.prototype.encode
  const textDecoder = new TextDecoder()
  const decodeText = TextDecoder.prototype.decode
  const parse = JSON.parse
  const Bytes = Uint8Array
  const number = Number
  const NativeError = Error
  const Callbacks = Map
  const mapHas = Map.prototype.has
  const mapSet = Map.prototype.set
  const mapDelete = Map.prototype.delete
  const Words = Uint32Array
  const crypto = window.crypto
  const random = crypto.getRandomValues
  let resetChannel
  let listenRegistration
  let callbacks
  const unregister = (id) => apply(mapDelete, callbacks, [id])
  const register = (callback, once = false) => {
    let id
    do {
      const word = new Words(1)
      apply(random, crypto, [word])
      id = word[0]
    } while (apply(mapHas, callbacks, [id]))
    apply(mapSet, callbacks, [id, (value) => {
      if (once) unregister(id)
      return callback(value)
    }])
    return id
  }
  const bootstrap = window.__seuofficeNativeBridgeBootstrap
  let nativeInvoke
  if (typeof bootstrap === 'function') {
    delete window.__seuofficeNativeBridgeBootstrap
    const bridge = bootstrap()
    if (!bridge || (bridge.mode !== 'invoke' && bridge.mode !== 'preload') ||
        typeof bridge.invokeTauri !== 'function' || typeof bridge.listenTauri !== 'function' ||
        (bridge.mode === 'preload' && typeof bridge.sendSync !== 'function')) {
      throw new NativeError('native document context was not initialized')
    }
    nativeInvoke = bridge.invokeTauri
    bridge.listenTauri((callback, raw, body, index, end) => {
      const data = end ? undefined : raw ? body : parse(apply(decodeText, textDecoder, [body]))
      if (index === undefined) {
        runCallback(callback, data)
        return
      }
      if (typeof index !== 'bigint' || index < 0n || index > 9007199254740991n) {
        throw new NativeError('native channel index is not a safe integer')
      }
      runCallback(callback, end ? { end: true, index: number(index) } : { message: data, index: number(index) })
    }, () => {
      if (!listenRegistration) {
        if (bridge.mode === 'invoke') return true
        throw new NativeError('the native output stream was never initialized')
      }
      if (!resetChannel || !callbacks) throw new NativeError('the native output stream cannot resume')
      const channel = resetChannel(register, unregister)
      if (typeof channel !== 'string') throw new NativeError('native Channel reset failed')
      let success
      let failure
      success = register(() => unregister(failure), true)
      failure = register((reason) => {
        unregister(success)
        throw new NativeError(`native output stream restore failed: ${reason}`)
      }, true)
      try {
        nativeInvoke('ipc_listen', success, failure, 'application/json', invokeKey,
          apply(encodeText, textEncoder, [`{"onFrame":${stringify(channel)}}`]), listenRegistration)
      } catch (error) {
        unregister(success)
        unregister(failure)
        throw error
      }
      return true
    })
    if (bridge.mode === 'preload') {
      const sendSync = bridge.sendSync
      const send = (channel, frame) => new Bytes(sendSync(channel, frame))
      Object.defineProperty(send, '__seuofficeResumeChannel', {
        configurable: true,
        value(reset) {
          if (resetChannel || typeof reset !== 'function') throw new NativeError('native Channel resume was already bound')
          resetChannel = reset
        },
      })
      Object.defineProperty(window, '__seuofficeNativeSendSync', {
        configurable: true,
        value: send,
      })
    }
  }

  const encode = (payload) => {
    if (payload instanceof ArrayBuffer || ArrayBuffer.isView(payload) || Array.isArray(payload)) {
      return ['application/octet-stream', payload]
    }
    return ['application/json', stringify(payload, (_key, value) => {
      if (value instanceof Map) return Object.fromEntries(value.entries())
      if (value instanceof Uint8Array) return Array.from(value)
      if (value instanceof ArrayBuffer) return Array.from(new Uint8Array(value))
      if (typeof value === 'object' && value !== null && '__TAURI_TO_IPC_KEY__' in value) {
        return value.__TAURI_TO_IPC_KEY__()
      }
      return value
    })]
  }

  Object.defineProperty(internals, 'postMessage', {
    value(message) {
      const { cmd, callback, error, payload, options } = message
      const [contentType, body] = encode(payload)
      if (nativeInvoke) {
        const bytes = contentType === 'application/json'
          ? apply(encodeText, textEncoder, [body])
          : body instanceof ArrayBuffer ? body
          : ArrayBuffer.isView(body) ? new Uint8Array(body.buffer, body.byteOffset, body.byteLength)
          : new Uint8Array(body)
        const headers = new HeadersNative(options && options.headers)
        const headersJson = stringify(Array.from(headers))
        if (cmd === 'ipc_listen' && listenRegistration === undefined) {
          if (!(internals.callbacks instanceof Callbacks)) throw new NativeError('the Tauri callback map is unavailable')
          callbacks = internals.callbacks
          listenRegistration = headersJson
        }
        nativeInvoke(cmd, callback, error, contentType, invokeKey, bytes, headersJson)
        return
      }
      const headers = new HeadersNative(options && options.headers)
      apply(headersSet, headers, ['Content-Type', contentType])
      apply(headersSet, headers, ['Tauri-Callback', String(callback)])
      apply(headersSet, headers, ['Tauri-Error', String(error)])
      apply(headersSet, headers, ['Tauri-Invoke-Key', invokeKey])
      if (cmd === 'ipc_invoke' || cmd === 'ipc_send' || cmd === 'ipc_listen') {
        runCallback(error, 'native document context is unavailable')
        return
      }
      const request = fetchNative(convertFileSrc(cmd, 'ipc'), { method: 'POST', body, headers })
      apply(promiseThen, request, [
        (response) => {
          const callbackId = apply(headersGet, response.headers, ['Tauri-Response']) === 'ok' ? callback : error
          const content = (apply(headersGet, response.headers, ['content-type']) || '').split(',')[0]
          const read = content === 'application/json' ? responseJson : content === 'text/plain' ? responseText : responseBuffer
          apply(promiseThen, apply(read, response, []), [
            (data) => runCallback(callbackId, data),
            (reason) => runCallback(error, String(reason)),
          ])
        },
        (reason) => runCallback(error, String(reason)),
      ])
    },
  })
})()
