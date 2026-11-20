type Request =
  | { operation: 'encrypt'; userPassword: string; ownerPassword?: string }
  | { operation: 'decrypt'; password: string }
  | { operation: 'repair' }

async function run(bytes: Uint8Array, request: Request, signal?: AbortSignal): Promise<Uint8Array> {
  if (signal?.aborted) throw new DOMException('The job was cancelled', 'AbortError')
  const worker = new Worker(new URL('./pdf/qpdf-worker.js', document.baseURI), { type: 'module' })
  const { promise, resolve, reject } = Promise.withResolvers<Uint8Array>()
  let settled = false
  const finish = (error?: Error, value?: ArrayBuffer) => {
    if (settled) return
    settled = true
    signal?.removeEventListener('abort', abort)
    worker.terminate()
    if (error) reject(error)
    else if (value) resolve(new Uint8Array(value))
    else reject(new Error('qpdf worker returned no output'))
  }
  const abort = () => finish(new DOMException('The job was cancelled', 'AbortError'))
  signal?.addEventListener('abort', abort, { once: true })
  worker.onerror = (event) => finish(new Error(event.message || 'qpdf worker failed'))
  worker.onmessageerror = () => finish(new Error('qpdf worker returned an invalid message'))
  worker.onmessage = (event) => {
    const result: unknown = event.data
    if (
      result &&
      typeof result === 'object' &&
      'ok' in result &&
      result.ok instanceof ArrayBuffer
    ) {
      finish(undefined, result.ok)
    } else if (
      result &&
      typeof result === 'object' &&
      'error' in result &&
      result.error &&
      typeof result.error === 'object' &&
      'message' in result.error &&
      typeof result.error.message === 'string'
    ) {
      const error = new Error(result.error.message) as Error & { code?: unknown; reason?: unknown }
      if ('name' in result.error && typeof result.error.name === 'string')
        error.name = result.error.name
      if ('code' in result.error) error.code = result.error.code
      if ('reason' in result.error) error.reason = result.error.reason
      finish(error)
    } else finish(new Error('qpdf worker returned an invalid message'))
  }
  const data =
    bytes.byteOffset === 0 &&
    bytes.byteLength === bytes.buffer.byteLength &&
    bytes.buffer instanceof ArrayBuffer
      ? bytes.buffer
      : bytes.slice().buffer
  try {
    worker.postMessage({ ...request, bytes: data }, [data])
  } catch (error) {
    finish(error instanceof Error ? error : new Error(String(error)))
  }
  if (signal?.aborted) abort()
  return promise
}

export function encryptPdf(
  bytes: Uint8Array,
  userPassword: string,
  ownerPassword?: string,
  signal?: AbortSignal,
): Promise<Uint8Array> {
  return run(
    bytes,
    {
      operation: 'encrypt',
      userPassword,
      ...(ownerPassword === undefined ? {} : { ownerPassword }),
    },
    signal,
  )
}

export function decryptPdf(
  bytes: Uint8Array,
  password: string,
  signal?: AbortSignal,
): Promise<Uint8Array> {
  return run(bytes, { operation: 'decrypt', password }, signal)
}

export function repairPdf(bytes: Uint8Array, signal?: AbortSignal): Promise<Uint8Array> {
  return run(bytes, { operation: 'repair' }, signal)
}
