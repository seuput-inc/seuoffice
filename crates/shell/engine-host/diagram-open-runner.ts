/** A request owns its worker, so abort stops only its parser. */
export function runDiagramOpen(input: unknown, signal: AbortSignal): Promise<unknown> {
  signal.throwIfAborted()
  const worker = new Worker(new URL('./diagram-open-worker.js', document.baseURI), {
    type: 'module',
  })
  const { promise, resolve, reject } = Promise.withResolvers<unknown>()
  let settled = false
  const finish = (error?: Error, value?: unknown) => {
    if (settled) return
    settled = true
    signal.removeEventListener('abort', abort)
    worker.terminate()
    if (error) reject(error)
    else resolve(value!)
  }
  const abort = () => finish(new DOMException('The job was cancelled', 'AbortError'))
  signal.addEventListener('abort', abort, { once: true })
  worker.onerror = (event) => finish(new Error(event.message || 'Diagram conversion worker failed'))
  worker.onmessageerror = () =>
    finish(new Error('Diagram conversion worker returned an invalid message'))
  worker.onmessage = (event) => {
    const reply: unknown = event.data
    if (
      reply &&
      typeof reply === 'object' &&
      'ok' in reply &&
      reply.ok === true &&
      'value' in reply &&
      reply.value &&
      typeof reply.value === 'object' &&
      'ok' in reply.value &&
      typeof reply.value.ok === 'boolean'
    ) {
      finish(undefined, reply.value)
    } else if (
      reply &&
      typeof reply === 'object' &&
      'ok' in reply &&
      reply.ok === false &&
      'name' in reply &&
      typeof reply.name === 'string' &&
      'message' in reply &&
      typeof reply.message === 'string'
    ) {
      const failure = new Error(reply.message)
      failure.name = reply.name
      finish(failure)
    } else {
      finish(new Error('Diagram conversion worker returned an invalid reply'))
    }
  }
  try {
    worker.postMessage(input)
    if (signal.aborted) abort()
  } catch (error) {
    finish(error instanceof Error ? error : new Error(String(error)))
  }
  return promise
}
