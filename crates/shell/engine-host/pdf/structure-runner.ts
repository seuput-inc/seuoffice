import type { OcrRecognition } from '../../../../packages/pdf2docx/src/ocr'

export interface NativeOcrRequest {
  requestId: number
  png: Uint8Array
  page: { widthPt: number; heightPt: number }
  signal: AbortSignal
}

export interface StructureRunRequest {
  operation: 'tree' | 'fingerprints' | 'blankPages' | 'images'
  bytes: Uint8Array
  password?: string
  pages?: number[]
  minImagePx?: number
  ocr: ((request: NativeOcrRequest) => Promise<OcrRecognition | null>) | null
  onProgress: (done: number, total: number) => void
  signal: AbortSignal
}

const MAX_RESPONSE = 64 * 1024 * 1024

export async function runStructureTree(request: StructureRunRequest): Promise<unknown> {
  if (request.signal.aborted) throw new DOMException('The job was cancelled', 'AbortError')
  const worker = new Worker(new URL('./pdf/structure-worker.js', document.baseURI), {
    type: 'module',
  })
  const { promise, resolve, reject } = Promise.withResolvers<unknown>()
  let settled = false
  const lifetime = new AbortController()
  let lastOcr = 0
  const waiters = new Set<SharedArrayBuffer>()
  const ocrTimers = new Map<SharedArrayBuffer, () => void>()
  const wakeFailed = (response: SharedArrayBuffer) => {
    const control = new Int32Array(response, 0, 2)
    Atomics.compareExchange(control, 0, 0, 2)
    Atomics.notify(control, 0)
  }
  const finish = (error?: Error, result?: unknown) => {
    if (settled) return
    settled = true
    request.signal.removeEventListener('abort', abort)
    for (const waiter of waiters) wakeFailed(waiter)
    for (const clear of ocrTimers.values()) clear()
    ocrTimers.clear()
    waiters.clear()
    lifetime.abort()
    worker.terminate()
    if (error) reject(error)
    else resolve(result)
  }
  const abort = () => finish(new DOMException('The job was cancelled', 'AbortError'))
  request.signal.addEventListener('abort', abort, { once: true })
  worker.onerror = (event) => finish(new Error(event.message || 'structure worker failed'))
  worker.onmessageerror = () => finish(new Error('structure worker returned an invalid message'))
  worker.onmessage = (event) => {
    const message: unknown = event.data
    if (!message || typeof message !== 'object' || !('type' in message)) {
      finish(new Error('structure worker returned an invalid message'))
      return
    }
    if (
      message.type === 'progress' &&
      'done' in message &&
      'total' in message &&
      typeof message.done === 'number' &&
      typeof message.total === 'number'
    ) {
      request.onProgress(message.done, message.total)
      return
    }
    if (message.type === 'result' && 'result' in message) {
      finish(undefined, message.result)
      return
    }
    if (
      message.type === 'error' &&
      'error' in message &&
      message.error &&
      typeof message.error === 'object' &&
      'message' in message.error &&
      typeof message.error.message === 'string'
    ) {
      const error = new Error(message.error.message) as Error & {
        code?: unknown
        validRange?: unknown
      }
      if ('name' in message.error && typeof message.error.name === 'string')
        error.name = message.error.name
      if ('code' in message.error) error.code = message.error.code
      if ('validRange' in message.error) error.validRange = message.error.validRange
      finish(error)
      return
    }
    if (
      message.type !== 'ocr' ||
      !('response' in message) ||
      !(message.response instanceof SharedArrayBuffer) ||
      !('requestId' in message) ||
      typeof message.requestId !== 'number' ||
      !Number.isSafeInteger(message.requestId) ||
      message.requestId <= lastOcr ||
      !('png' in message) ||
      !(message.png instanceof ArrayBuffer) ||
      !('page' in message) ||
      !message.page ||
      typeof message.page !== 'object' ||
      !('widthPt' in message.page) ||
      !('heightPt' in message.page) ||
      typeof message.page.widthPt !== 'number' ||
      typeof message.page.heightPt !== 'number' ||
      message.response.byteLength !== 8 ||
      request.ocr === null
    ) {
      finish(new Error('structure worker returned an invalid OCR request'))
      return
    }
    lastOcr = message.requestId
    const response = message.response
    const nativeOcr = request.ocr
    waiters.add(response)
    const timer = setTimeout(() => finish(new Error('native OCR request timed out')), 30_000)
    ocrTimers.set(response, () => clearTimeout(timer))
    const input: NativeOcrRequest = {
      requestId: message.requestId,
      png: new Uint8Array(message.png),
      page: { widthPt: message.page.widthPt, heightPt: message.page.heightPt },
      signal: lifetime.signal,
    }
    void Promise.resolve()
      .then(() => nativeOcr(input))
      .then(
        (result) => {
          ocrTimers.get(response)?.()
          ocrTimers.delete(response)
          if (settled || !waiters.delete(response)) return
          try {
            const data = new TextEncoder().encode(JSON.stringify(result))
            if (!Number.isSafeInteger(data.length) || data.length < 0 || data.length > MAX_RESPONSE)
              throw new Error('OCR response is too large')
            const control = new Int32Array(response, 0, 2)
            if (Atomics.compareExchange(control, 0, 0, 3) !== 0) return
            try {
              const growable = response as SharedArrayBuffer & { grow(length: number): void }
              growable.grow(8 + data.length)
              new Uint8Array(response, 8, data.length).set(data)
              Atomics.store(control, 1, data.length)
              Atomics.store(control, 0, 1)
            } catch {
              Atomics.store(control, 0, 2)
            }
            Atomics.notify(control, 0)
          } catch {
            wakeFailed(response)
          }
        },
        () => {
          ocrTimers.get(response)?.()
          ocrTimers.delete(response)
          if (!settled && waiters.delete(response)) wakeFailed(response)
        },
      )
  }
  const bytes =
    request.bytes.byteOffset === 0 &&
    request.bytes.byteLength === request.bytes.buffer.byteLength &&
    request.bytes.buffer instanceof ArrayBuffer
      ? request.bytes.buffer
      : request.bytes.slice().buffer
  try {
    worker.postMessage(
      {
        operation: request.operation,
        bytes,
        password: request.password,
        pages: request.pages,
        minImagePx: request.minImagePx,
        ocrAvailable: request.ocr !== null,
      },
      [bytes],
    )
  } catch (error) {
    finish(error instanceof Error ? error : new Error(String(error)))
  }
  if (request.signal.aborted) abort()
  return promise
}
