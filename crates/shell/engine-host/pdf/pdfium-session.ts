import type { NativeCallback } from '../transport'

interface WorkerReply { ok?: unknown; error?: { message: string; code?: number } }

export async function runPdfiumSession(_input: unknown, signal: AbortSignal, callback: NativeCallback): Promise<null> {
  signal.throwIfAborted()
  const worker = new Worker(new URL('./pdf/pdfium-session/worker.js', document.baseURI), { type: 'module' })
  let pending: { resolve: (reply: WorkerReply) => void; reject: (error: Error) => void } | undefined
  let stopped = false
  const stop = (error: Error) => {
    if (stopped) return
    stopped = true
    worker.terminate()
    pending?.reject(error)
    pending = undefined
  }
  const abort = () => stop(new DOMException('The PDFium session was cancelled', 'AbortError'))
  signal.addEventListener('abort', abort, { once: true })
  worker.onerror = event => stop(new Error(event.message || 'PDFium worker failed'))
  worker.onmessageerror = () => stop(new Error('PDFium worker returned an invalid message'))
  worker.onmessage = event => {
    if (!pending) { stop(new Error('Unexpected PDFium worker reply')); return }
    const current = pending
    pending = undefined
    current.resolve(event.data as WorkerReply)
  }
  try {
    let response: WorkerReply | undefined
    for (;;) {
      signal.throwIfAborted()
      const command = await callback('pdfium.next', response === undefined ? {} : { response })
      signal.throwIfAborted()
      if (!command || typeof command !== 'object' || !('op' in command) || typeof command.op !== 'string') {
        throw new TypeError('Invalid PDFium command')
      }
      const { promise, resolve, reject } = Promise.withResolvers<WorkerReply>()
      pending = { resolve, reject }
      try { worker.postMessage(command) } catch (error) { pending = undefined; reject(error instanceof Error ? error : new Error(String(error))) }
      response = await promise
      if (command.op === 'close') return null
    }
  } finally {
    signal.removeEventListener('abort', abort)
    stop(new Error('PDFium session closed'))
  }
}
