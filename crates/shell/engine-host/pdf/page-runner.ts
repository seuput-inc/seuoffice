export async function runPdfPageOperation(
  name: string,
  input: unknown,
  signal: AbortSignal,
): Promise<unknown> {
  if (signal.aborted) throw new DOMException('The job was cancelled', 'AbortError')
  if (
    name === 'pdf.qpdfEncrypt' ||
    name === 'pdf.qpdfDecrypt' ||
    name === 'pdf.repair' ||
    name === 'pdf.structureOperation'
  ) {
    throw new TypeError(`PDF operation ${name} requires its dedicated worker`)
  }
  const worker = new Worker(new URL('./pdf/page-worker.js', document.baseURI), { type: 'module' })
  const { promise, resolve, reject } = Promise.withResolvers<unknown>()
  let settled = false
  const finish = (outcome: { result: unknown } | { error: Error }) => {
    if (settled) return
    settled = true
    signal.removeEventListener('abort', abort)
    globalThis.removeEventListener('pagehide', abort)
    worker.onmessage = null
    worker.onerror = null
    worker.onmessageerror = null
    worker.terminate()
    if ('error' in outcome) reject(outcome.error)
    else resolve(outcome.result)
  }
  const abort = () => finish({ error: new DOMException('The job was cancelled', 'AbortError') })
  signal.addEventListener('abort', abort, { once: true })
  globalThis.addEventListener('pagehide', abort, { once: true })
  worker.onerror = (event) =>
    finish({ error: new Error(event.message || 'PDF page worker failed') })
  worker.onmessageerror = () =>
    finish({ error: new Error('PDF page worker returned an invalid message') })
  worker.onmessage = (event) => {
    const message: unknown = event.data
    if (!message || typeof message !== 'object' || !('type' in message)) {
      finish({ error: new Error('PDF page worker returned an invalid message') })
      return
    }
    if (message.type === 'result' && 'result' in message && message.result !== undefined) {
      finish({ result: message.result })
      return
    }
    if (
      message.type === 'error' &&
      'error' in message &&
      message.error &&
      typeof message.error === 'object' &&
      'name' in message.error &&
      typeof message.error.name === 'string' &&
      'message' in message.error &&
      typeof message.error.message === 'string'
    ) {
      const error = new Error(message.error.message) as Error & {
        code?: unknown
        reason?: unknown
        validRange?: unknown
      }
      error.name = message.error.name
      if ('code' in message.error) error.code = message.error.code
      if ('reason' in message.error) error.reason = message.error.reason
      if ('validRange' in message.error) error.validRange = message.error.validRange
      finish({ error })
      return
    }
    finish({ error: new Error('PDF page worker returned an invalid message') })
  }
  if (signal.aborted) abort()
  if (!settled) {
    try {
      worker.postMessage({ name, input })
    } catch (thrown) {
      finish({ error: thrown instanceof Error ? thrown : new Error(String(thrown)) })
    }
  }
  return promise
}
