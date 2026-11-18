import { PdfDocumentPrimitives } from '../document-edit-primitives'

const worker = self as unknown as {
  onmessage: ((event: MessageEvent<unknown>) => void) | null
  postMessage(value: unknown): void
}
let document: PdfDocumentPrimitives | undefined
let active = false
worker.onmessage = async event => {
  if (active) { worker.postMessage({ error: { message: 'PDF document codec received concurrent commands' } }); return }
  active = true
  try {
    const command = event.data
    if (!command || typeof command !== 'object' || !('op' in command) || typeof command.op !== 'string') throw new TypeError('invalid PDF document codec command')
    if (command.op === 'open') {
      if (document || !('bytesBase64' in command) || typeof command.bytesBase64 !== 'string') throw new TypeError('invalid PDF document open')
      document = await PdfDocumentPrimitives.open(command.bytesBase64)
      worker.postMessage({ ok: null })
    } else if (command.op === 'close') {
      document = undefined
      worker.postMessage({ ok: null })
    } else {
      if (!document) throw new Error('PDF document is not open')
      const result = await document.execute(command as Parameters<PdfDocumentPrimitives['execute']>[0])
      worker.postMessage({ ok: result })
    }
  } catch (value) {
    const error = value instanceof Error ? value : new Error(String(value))
    worker.postMessage({ error: { message: error.message } })
  } finally { active = false }
}
