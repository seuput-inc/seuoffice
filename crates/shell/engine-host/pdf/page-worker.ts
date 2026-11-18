import { hasPdfOperation, pdfPageOperation } from '../pdf'

interface WorkerRequest {
  name: string
  input: unknown
}

const worker = self as unknown as {
  onmessage: ((event: MessageEvent<unknown>) => void) | null
  postMessage(message: unknown): void
}
let started = false
let invalid = false

worker.onmessage = async (event) => {
  if (started) {
    if (invalid) return
    invalid = true
    worker.postMessage({ type: 'error', error: { name: 'Error', message: 'PDF page worker received a duplicate request' } })
    return
  }
  started = true
  try {
    const request: unknown = event.data
    if (!request || typeof request !== 'object' || !('name' in request) || typeof request.name !== 'string' ||
        !('input' in request) || !hasPdfOperation(request.name) ||
        request.name === 'pdf.qpdfEncrypt' || request.name === 'pdf.qpdfDecrypt' || request.name === 'pdf.repair') {
      throw new TypeError('invalid PDF page worker request')
    }
    const { name, input }: WorkerRequest = request
    const result = await pdfPageOperation(name, input)
    if (invalid) return
    if (result === undefined) throw new Error('PDF page worker returned no output')
    worker.postMessage({ type: 'result', result })
  } catch (thrown) {
    if (invalid) return
    const error = thrown instanceof Error ? thrown : new Error(String(thrown))
    const details = error as Error & { code?: unknown; reason?: unknown; validRange?: unknown }
    try {
      worker.postMessage({ type: 'error', error: {
        name: error.name,
        message: error.message,
        ...('code' in details ? { code: details.code } : {}),
        ...('reason' in details ? { reason: details.reason } : {}),
        ...('validRange' in details ? { validRange: details.validRange } : {}),
      } })
    } catch {
      worker.postMessage({ type: 'error', error: { name: 'Error', message: 'PDF page worker could not serialize its response' } })
    }
  }
}
