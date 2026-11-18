import { CefHtmlDriver } from './driver'
import { convertHtml } from './html'

const worker = self as unknown as {
  onmessage: ((event: MessageEvent) => void) | null
  postMessage(message: unknown): void
}
const pending = new Map<number, { resolve(value: unknown): void; reject(error: Error): void }>()
let next = 0
worker.onmessage = async event => {
  const message = event.data
  if (message.type === 'callbackResult') {
    const waiter = pending.get(message.id)
    if (!waiter) return
    pending.delete(message.id)
    if (message.error) waiter.reject(new Error(message.error))
    else waiter.resolve(message.value)
    return
  }
  if (message.type !== 'start') return
  try {
    const driver = new CefHtmlDriver((operation, input) => {
      const id = ++next
      const { promise, resolve, reject } = Promise.withResolvers<unknown>()
      pending.set(id, { resolve, reject })
      worker.postMessage({ type: 'callback', id, operation, input })
      return promise
    })
    const result = await convertHtml(message.url, driver, new AbortController().signal, message.naturalTableWidth)
    worker.postMessage({ type: 'result', result })
  } catch (error) {
    worker.postMessage({ type: 'error', error: error instanceof Error ? error.message : String(error) })
  }
}
