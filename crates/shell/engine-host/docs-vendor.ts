import { encode } from '../bridge/wire'

function base64(bytes: Uint8Array): string {
  let text = ''
  for (const byte of bytes) text += String.fromCharCode(byte)
  return btoa(text)
}

export async function runHwp(input: unknown, signal: AbortSignal): Promise<unknown> {
  if (
    !input ||
    typeof input !== 'object' ||
    !('bytesBase64' in input) ||
    typeof input.bytesBase64 !== 'string' ||
    !('format' in input) ||
    (input.format !== 'hwp' && input.format !== 'hwpx')
  ) {
    throw new TypeError('invalid Hangul conversion request')
  }
  signal.throwIfAborted()
  const docx = Uint8Array.from(atob(input.bytesBase64), (character) => character.charCodeAt(0))
  const worker = new Worker(new URL('./docs-vendor/hwp-worker.js', document.baseURI), {
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
    else resolve(value)
  }
  const abort = () => finish(new DOMException('The job was cancelled', 'AbortError'))
  signal.addEventListener('abort', abort, { once: true })
  worker.onerror = (event) => finish(new Error(event.message || 'Hangul conversion worker failed'))
  worker.onmessageerror = () =>
    finish(new Error('Hangul conversion worker returned an invalid message'))
  worker.onmessage = (event) => {
    const reply: unknown = event.data
    if (!reply || typeof reply !== 'object' || !('ok' in reply)) {
      finish(new Error('Hangul conversion worker returned an invalid reply'))
    } else if (
      reply.ok === false &&
      'code' in reply &&
      typeof reply.code === 'string' &&
      'error' in reply &&
      typeof reply.error === 'string'
    ) {
      finish(undefined, { ok: false, code: reply.code, error: reply.error })
    } else if (
      reply.ok === true &&
      'bytes' in reply &&
      reply.bytes instanceof ArrayBuffer &&
      'losses' in reply &&
      Array.isArray(reply.losses)
    ) {
      const frame = encode({ bytes: new Uint8Array(reply.bytes), losses: reply.losses })
      finish(undefined, { ok: true, header: frame.header, buffers: frame.buffers.map(base64) })
    } else {
      finish(new Error('Hangul conversion worker returned an invalid reply'))
    }
  }
  try {
    worker.postMessage({ docx: docx.buffer, format: input.format }, [docx.buffer])
    if (signal.aborted) abort()
  } catch (error) {
    finish(error instanceof Error ? error : new Error(String(error)))
  }
  return promise
}
