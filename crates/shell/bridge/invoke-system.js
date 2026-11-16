// Tauri Builder::invoke_system substitutes this token before document start.
//
// The document bridge of the shell. One closure, invisible to page code:
//  - a per-document secret nonce, registered once with `ipc_register` and sent in the
//    `x-seuoffice-doc` header of every authenticated call (fetch and sync XHR);
//  - `__seuofficeCheck(id)`, `__seuofficeWake(id)`, `__seuofficeDeliver(...)`: the document id
//    check of Rust evaluations, the outbox wake, and frame delivery;
//  - `pageshow(persisted)` re-registration with a new nonce, and `ipc_render_ready` once the
//    document has loaded and its fonts are ready.
// A document whose nonce is not registered is refused by Rust before any state is read.
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
  const NativePromise = Promise
  const apply = Reflect.apply
  const deleteProperty = Reflect.deleteProperty
  const stringify = JSON.stringify
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
  const View = DataView
  const getUint8 = DataView.prototype.getUint8
  const getUint32 = DataView.prototype.getUint32
  const getBigUint64 = DataView.prototype.getBigUint64
  const bufferSlice = ArrayBuffer.prototype.slice
  const Xhr = window.XMLHttpRequest
  const xhrOpen = Xhr.prototype.open
  const xhrSend = Xhr.prototype.send
  const xhrSetHeader = Xhr.prototype.setRequestHeader
  const xhrMime = Xhr.prototype.overrideMimeType
  const xhrStatus = Object.getOwnPropertyDescriptor(Xhr.prototype, 'status').get
  const xhrText = Object.getOwnPropertyDescriptor(Xhr.prototype, 'responseText').get
  const later = window.setTimeout.bind(window)
  const listen = window.addEventListener.bind(window)
  const crypto = window.crypto
  const random = crypto.getRandomValues

  const DOC_HEADER = 'x-seuoffice-doc'
  const SYNC_NAME = '__seuofficeNativeSendSync'
  const RESUME_NAME = '__seuofficeResumeChannel'
  const UNAVAILABLE = 'native document context is unavailable'
  // Messages of ipc/mod.rs: the only two refusals a retry can cure.
  const NOT_COMMITTED = 'Renderer document is not committed yet.'
  const NOT_READY = 'Renderer document authentication is not ready.'
  const RETRY_MS = 25
  const ATTEMPTS = 100
  const FRAME_HEADER = 17

  const hex = []
  for (let byte = 0; byte < 256; byte += 1) {
    hex.push((byte < 16 ? '0' : '') + byte.toString(16))
  }
  const freshNonce = () => {
    const bytes = new Bytes(16)
    apply(random, crypto, [bytes])
    let text = ''
    for (let index = 0; index < 16; index += 1) text += hex[bytes[index]]
    return text
  }

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
    apply(mapSet, callbacks, [
      id,
      (value) => {
        if (once) unregister(id)
        return callback(value)
      },
    ])
    return id
  }
  const then = (promise, done, failed) => apply(promiseThen, promise, [done, failed])
  const ignore = () => {}

  // The current document registration. `document` is the public id Rust minted for it.
  let state = { nonce: '', document: undefined, bridge: undefined }
  let registration

  /** One call of Tauri's `ipc` protocol with the document nonce. `done(ok, data)` runs once. */
  const exchange = (current, cmd, callback, error, contentType, body, optionHeaders, done) => {
    const headers = new HeadersNative(optionHeaders)
    apply(headersSet, headers, ['Content-Type', contentType])
    apply(headersSet, headers, ['Tauri-Callback', String(callback)])
    apply(headersSet, headers, ['Tauri-Error', String(error)])
    apply(headersSet, headers, ['Tauri-Invoke-Key', invokeKey])
    apply(headersSet, headers, [DOC_HEADER, current.nonce])
    const request = fetchNative(convertFileSrc(cmd, 'ipc'), { method: 'POST', body, headers })
    then(
      request,
      (response) => {
        const ok = apply(headersGet, response.headers, ['Tauri-Response']) === 'ok'
        const content = (apply(headersGet, response.headers, ['content-type']) || '').split(',')[0]
        const read =
          content === 'application/json'
            ? responseJson
            : content === 'text/plain'
              ? responseText
              : responseBuffer
        then(
          apply(read, response, []),
          (data) => done(ok, data),
          (reason) => done(false, String(reason)),
        )
      },
      (reason) => done(false, String(reason)),
    )
  }

  const startRegistration = () => {
    const current = { nonce: freshNonce(), document: undefined, bridge: undefined }
    state = current
    return new NativePromise((resolve, reject) => {
      if (window.location.protocol === 'about:') {
        reject(new NativeError(UNAVAILABLE))
        return
      }
      let attempts = 0
      const attempt = () => {
        exchange(current, 'ipc_register', 0, 0, 'application/json', '{}', undefined, (ok, data) => {
          if (current !== state) {
            reject(new NativeError('native document registration was superseded'))
            return
          }
          if (ok && data && typeof data.document === 'number') {
            current.document = data.document
            current.bridge = data.bridge
            if (data.bridge !== 'preload') deleteProperty(window, SYNC_NAME)
            resolve(current)
            return
          }
          attempts += 1
          if (data === NOT_COMMITTED && attempts < ATTEMPTS) {
            later(attempt, RETRY_MS)
            return
          }
          reject(new NativeError(typeof data === 'string' ? data : UNAVAILABLE))
        })
      }
      // after every initialization script of this document has run
      later(attempt, 0)
    })
  }

  /** `ipc_render_ready`: loaded, fonts ready, one macrotask (no requestAnimationFrame). */
  const reportReady = (current) => {
    const send = () =>
      later(() => {
        if (current === state)
          exchange(current, 'ipc_render_ready', 0, 0, 'application/json', '{}', undefined, ignore)
      }, 0)
    const afterLoad = () => {
      const fonts = window.document.fonts && window.document.fonts.ready
      if (fonts) then(fonts, send, send)
      else send()
    }
    if (window.document.readyState === 'complete') afterLoad()
    else listen('load', afterLoad, { once: true })
  }

  const deliver = (callback, raw, body, index, end) => {
    const data = end ? undefined : raw ? body : parse(apply(decodeText, textDecoder, [body]))
    if (index === undefined) {
      runCallback(callback, data)
      return
    }
    if (typeof index !== 'bigint' || index < 0n || index > 9007199254740991n) {
      throw new NativeError('native channel index is not a safe integer')
    }
    runCallback(
      callback,
      end ? { end: true, index: number(index) } : { message: data, index: number(index) },
    )
  }

  // Frames of the outbox (ipc/sync.rs): u32 callback, u8 flags (1 channel, 2 raw, 4 end),
  // u64 index, u32 length, body; all little endian.
  const deliverFrames = (buffer) => {
    const view = new View(buffer)
    let at = 0
    while (at < buffer.byteLength) {
      if (at + FRAME_HEADER > buffer.byteLength)
        throw new NativeError('native outbox frame is truncated')
      const callback = apply(getUint32, view, [at, true])
      const flags = apply(getUint8, view, [at + 4])
      const index = apply(getBigUint64, view, [at + 5, true])
      const length = apply(getUint32, view, [at + 13, true])
      const start = at + FRAME_HEADER
      if (start + length > buffer.byteLength)
        throw new NativeError('native outbox frame is truncated')
      const body = apply(bufferSlice, buffer, [start, start + length])
      deliver(callback, (flags & 2) !== 0, body, (flags & 1) !== 0 ? index : undefined, (flags & 4) !== 0)
      at = start + length
    }
  }

  let pulling = false
  let pullAgain = false
  const pull = (current) => {
    if (pulling) {
      pullAgain = true
      return
    }
    pulling = true
    const step = () => {
      pullAgain = false
      const headers = new HeadersNative()
      apply(headersSet, headers, [DOC_HEADER, current.nonce])
      const request = fetchNative(
        convertFileSrc(`outbox/${current.document}`, 'seuoffice-ipc'),
        { method: 'GET', headers },
      )
      const finish = () => {
        if (pullAgain && current === state) step()
        else pulling = false
      }
      then(
        request,
        (response) => {
          if (response.status !== 200 || current !== state) {
            pulling = false
            return
          }
          then(
            apply(responseBuffer, response, []),
            (buffer) => {
              try {
                if (current === state) deliverFrames(buffer)
              } finally {
                finish()
              }
            },
            () => {
              pulling = false
            },
          )
        },
        () => {
          pulling = false
        },
      )
    }
    step()
  }

  const define = (name, value) => Object.defineProperty(window, name, { value })
  define('__seuofficeCheck', (id) => state.document !== undefined && id === state.document)
  define('__seuofficeWake', (id) => {
    if (state.document !== undefined && id === state.document) pull(state)
  })
  define('__seuofficeDeliver', (id, callback, raw, body, index, end) => {
    if (state.document !== undefined && id === state.document) deliver(callback, raw, body, index, end)
  })

  /** Re-send `ipc_listen` on a fresh output Channel after the document was restored. */
  const resume = (current) => {
    if (!listenRegistration) return
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
    send(
      current,
      'ipc_listen',
      success,
      failure,
      'application/json',
      `{"onFrame":${stringify(channel)}}`,
      listenRegistration,
    )
  }

  /** A protected call: after registration, with the nonce, retried only while Rust is not ready. */
  const send = (current, cmd, callback, error, contentType, body, optionHeaders) => {
    let attempts = 0
    const go = () =>
      exchange(current, cmd, callback, error, contentType, body, optionHeaders, (ok, data) => {
        attempts += 1
        if (!ok && data === NOT_READY && attempts < ATTEMPTS && current === state) {
          later(go, RETRY_MS)
          return
        }
        runCallback(ok ? callback : error, data)
      })
    go()
  }

  // Synchronous transport: an XHR to the `seuoffice-ipc` scheme with the document nonce.
  const syncExchange = (current, channel, body) => {
    const xhr = new Xhr()
    apply(xhrOpen, xhr, ['POST', convertFileSrc(channel, 'seuoffice-ipc'), false])
    apply(xhrMime, xhr, ['text/plain; charset=x-user-defined'])
    apply(xhrSetHeader, xhr, [DOC_HEADER, current.nonce])
    apply(xhrSend, xhr, [body])
    const text = apply(xhrText, xhr, [])
    const bytes = new Bytes(text.length)
    for (let index = 0; index < text.length; index += 1) bytes[index] = text.charCodeAt(index) & 255
    return { status: apply(xhrStatus, xhr, []), bytes }
  }

  const sendSync = (channel, frame) => {
    const current = state
    if (current.document === undefined) {
      // registration is still in flight: register synchronously, same nonce, same document
      const answer = syncExchange(current, 'seuoffice:register', new Bytes(0))
      if (answer.status !== 200) throw new NativeError('Native synchronous request refused')
      const registered = parse(apply(decodeText, textDecoder, [answer.bytes]))
      if (current === state && current.document === undefined) {
        current.document = registered.document
        current.bridge = registered.bridge
      }
    }
    const answer = syncExchange(current, channel, frame)
    if (answer.status !== 200) throw new NativeError('Native synchronous request refused')
    return answer.bytes
  }
  Object.defineProperty(sendSync, RESUME_NAME, {
    configurable: true,
    value(reset) {
      if (resetChannel || typeof reset !== 'function')
        throw new NativeError('native Channel resume was already bound')
      resetChannel = reset
    },
  })
  Object.defineProperty(window, SYNC_NAME, { configurable: true, value: sendSync })

  registration = startRegistration()
  then(registration, reportReady, ignore)
  listen('pageshow', (event) => {
    if (!event.persisted) return
    registration = startRegistration()
    then(
      registration,
      (current) => {
        reportReady(current)
        resume(current)
      },
      ignore,
    )
  })

  // Context menu: forward only what no page handler claimed (no preventDefault), then
  // prevent the default menu ourselves. The window bubble listener runs after the page's.
  const MAX_SELECTION = 1024
  const MAX_SRC = 8192
  const MAX_WORD = 64
  const wordAt = (target, x, y) => {
    try {
      let text
      let offset
      if (typeof target.value === 'string' && typeof target.selectionStart === 'number') {
        text = target.value
        offset = target.selectionStart
      } else {
        let node
        if (window.document.caretPositionFromPoint) {
          const position = window.document.caretPositionFromPoint(x, y)
          node = position && position.offsetNode
          offset = position && position.offset
        } else if (window.document.caretRangeFromPoint) {
          const range = window.document.caretRangeFromPoint(x, y)
          node = range && range.startContainer
          offset = range && range.startOffset
        }
        if (!node || node.nodeType !== 3) return null
        text = node.data
      }
      if (typeof text !== 'string' || typeof offset !== 'number') return null
      const isWord = (ch) => /[\p{L}\p{N}'’_-]/u.test(ch)
      let start = offset
      let end = offset
      while (start > 0 && isWord(text[start - 1])) start -= 1
      while (end < text.length && isWord(text[end])) end += 1
      const word = text.slice(start, end)
      return word.length > 0 && word.length <= MAX_WORD ? word : null
    } catch {
      return null
    }
  }
  listen('contextmenu', (event) => {
    if (event.defaultPrevented || state.document === undefined) return
    const target = event.target
    const element = target && target.nodeType === 1 ? target : target && target.parentElement
    const editable = !!(element && (element.isContentEditable || /^(INPUT|TEXTAREA)$/.test(element.tagName)))
    const selection = String(window.getSelection ? window.getSelection() : '').slice(0, MAX_SELECTION)
    let srcUrl = null
    let mediaType = 'none'
    if (element && element.tagName === 'IMG') {
      const src = element.currentSrc || element.src || ''
      if (src.length > 0 && src.length <= MAX_SRC) {
        srcUrl = src
        mediaType = 'image'
      }
    }
    const x = event.clientX
    const y = event.clientY
    if (!Number.isFinite(x) || !Number.isFinite(y)) return
    event.preventDefault()
    const message = stringify({
      isEditable: editable,
      selectionText: selection,
      srcUrl,
      mediaType,
      x,
      y,
      editFlags: {
        canCut: editable && selection.length > 0,
        canCopy: selection.length > 0,
        canPaste: editable,
        canSelectAll: true,
      },
      word: editable ? wordAt(element, x, y) : null,
    })
    exchange(state, 'ipc_context_menu', 0, 0, 'application/json', message, undefined, ignore)
  })

  const encode = (payload) => {
    if (payload instanceof ArrayBuffer || ArrayBuffer.isView(payload) || Array.isArray(payload)) {
      return ['application/octet-stream', payload]
    }
    return [
      'application/json',
      stringify(payload, (_key, value) => {
        if (value instanceof Map) return Object.fromEntries(value.entries())
        if (value instanceof Uint8Array) return Array.from(value)
        if (value instanceof ArrayBuffer) return Array.from(new Uint8Array(value))
        if (typeof value === 'object' && value !== null && '__TAURI_TO_IPC_KEY__' in value) {
          return value.__TAURI_TO_IPC_KEY__()
        }
        return value
      }),
    ]
  }

  Object.defineProperty(internals, 'postMessage', {
    value(message) {
      const { cmd, callback, error, payload, options } = message
      const [contentType, body] = encode(payload)
      const bytes =
        contentType === 'application/json'
          ? body
          : body instanceof ArrayBuffer
            ? body
            : ArrayBuffer.isView(body)
              ? new Bytes(body.buffer, body.byteOffset, body.byteLength)
              : new Bytes(body)
      const optionHeaders = options && options.headers
      if (cmd === 'ipc_listen' && listenRegistration === undefined) {
        if (!(internals.callbacks instanceof Callbacks))
          throw new NativeError('the Tauri callback map is unavailable')
        callbacks = internals.callbacks
        listenRegistration = optionHeaders ? Array.from(new HeadersNative(optionHeaders)) : []
      }
      then(
        registration,
        (current) => send(current, cmd, callback, error, contentType, bytes, optionHeaders),
        () => runCallback(error, UNAVAILABLE),
      )
    },
  })
})()
