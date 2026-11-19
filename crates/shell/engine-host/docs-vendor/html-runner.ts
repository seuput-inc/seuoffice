import type { PageCall } from './driver'

export async function runHtml(
  naturalTableWidth: boolean,
  signal: AbortSignal,
  call: PageCall,
): Promise<unknown> {
  signal.throwIfAborted()
  const opened = await call('html.open', null)
  if (signal.aborted) {
    await call('html.close', null)
    signal.throwIfAborted()
  }
  if (
    !opened ||
    typeof opened !== 'object' ||
    !('url' in opened) ||
    typeof opened.url !== 'string'
  ) {
    await call('html.close', null)
    throw new Error('HTML conversion page returned no URL')
  }
  let worker: Worker
  try {
    worker = new Worker(new URL('./docs-vendor/html-worker.js', document.baseURI), {
      type: 'module',
    })
  } catch (error) {
    await call('html.close', null)
    throw error
  }
  const { promise, resolve, reject } = Promise.withResolvers<unknown>()
  let settled = false
  const finish = (error?: Error, value?: unknown) => {
    if (settled) return
    settled = true
    signal.removeEventListener('abort', abort)
    worker.terminate()
    void call('html.close', null).catch(() => {})
    if (error) reject(error)
    else resolve(value)
  }
  const abort = () => finish(new DOMException('The job was cancelled', 'AbortError'))
  signal.addEventListener('abort', abort, { once: true })
  worker.onerror = (event) => finish(new Error(event.message || 'HTML conversion worker failed'))
  worker.onmessageerror = () =>
    finish(new Error('HTML conversion worker returned an invalid message'))
  worker.onmessage = (event) => {
    const message: unknown = event.data
    if (!message || typeof message !== 'object' || !('type' in message))
      return finish(new Error('invalid HTML worker reply'))
    if (message.type === 'result' && 'result' in message) return finish(undefined, message.result)
    if (message.type === 'error' && 'error' in message && typeof message.error === 'string')
      return finish(new Error(message.error))
    if (
      message.type !== 'callback' ||
      !('id' in message) ||
      typeof message.id !== 'number' ||
      !('operation' in message) ||
      typeof message.operation !== 'string' ||
      !('input' in message)
    ) {
      return finish(new Error('invalid HTML worker callback'))
    }
    const id = message.id
    void call(message.operation, message.input).then(
      (value) => {
        if (!settled) worker.postMessage({ type: 'callbackResult', id, value })
      },
      (error) => {
        if (!settled)
          worker.postMessage({
            type: 'callbackResult',
            id,
            error: error instanceof Error ? error.message : String(error),
          })
      },
    )
  }
  try {
    worker.postMessage({ type: 'start', url: opened.url, naturalTableWidth })
    if (signal.aborted) abort()
  } catch (error) {
    finish(error instanceof Error ? error : new Error(String(error)))
  }
  return promise
}
