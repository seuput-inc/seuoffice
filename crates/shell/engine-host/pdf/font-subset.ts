interface SubsetInput {
  fontBase64: string
  text: string
}

function parseInput(input: unknown): SubsetInput {
  if (!input || typeof input !== 'object' || !('fontBase64' in input) || typeof input.fontBase64 !== 'string' ||
      !('text' in input) || typeof input.text !== 'string') {
    throw new TypeError('invalid PDF font subset request')
  }
  return { fontBase64: input.fontBase64, text: input.text }
}


/** Run one HarfBuzz subset in a disposable worker, so abort ends the computation. */
export async function subsetFont(input: unknown, signal: AbortSignal): Promise<{ fontBase64: string }> {
  if (signal.aborted) throw new DOMException('The job was cancelled', 'AbortError')
  const request = parseInput(input)
  if (signal.aborted) throw new DOMException('The job was cancelled', 'AbortError')
  const worker = new Worker(new URL('./pdf/font-subset-worker.js', document.baseURI), { type: 'module' })
  const { promise, resolve, reject } = Promise.withResolvers<string>()
  let settled = false
  const finish = (result: { value: string } | { error: Error }) => {
    if (settled) return
    settled = true
    signal.removeEventListener('abort', abort)
    globalThis.removeEventListener('pagehide', abort)
    worker.onmessage = null
    worker.onerror = null
    worker.onmessageerror = null
    worker.terminate()
    if ('error' in result) reject(result.error)
    else resolve(result.value)
  }
  const abort = () => finish({ error: new DOMException('The job was cancelled', 'AbortError') })
  signal.addEventListener('abort', abort, { once: true })
  globalThis.addEventListener('pagehide', abort, { once: true })
  worker.onerror = event => finish({ error: new Error(event.message || 'font subset worker failed') })
  worker.onmessageerror = () => finish({ error: new Error('font subset worker returned an invalid message') })
  worker.onmessage = event => {
    const message: unknown = event.data
    if (message && typeof message === 'object' && 'ok' in message && typeof message.ok === 'string' && message.ok.length > 0) {
      finish({ value: message.ok })
    } else if (message && typeof message === 'object' && 'error' in message && message.error &&
      typeof message.error === 'object' && 'name' in message.error && typeof message.error.name === 'string' &&
      'message' in message.error && typeof message.error.message === 'string') {
      const error = new Error(message.error.message)
      error.name = message.error.name
      finish({ error })
    } else {
      finish({ error: new Error('font subset worker returned an invalid message') })
    }
  }
  if (signal.aborted) abort()
  if (!settled) {
    try { worker.postMessage(request) }
    catch (thrown) { finish({ error: thrown instanceof Error ? thrown : new Error(String(thrown)) }) }
  }
  const fontBase64 = await promise
  if (signal.aborted) throw new DOMException('The job was cancelled', 'AbortError')
  return { fontBase64 }
}
