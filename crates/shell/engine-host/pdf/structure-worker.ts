import { extractImages, extractStructure } from './structure'
import { blankPages, pageFingerprints } from './pdfium-text'
import { sha256Hex } from '../../../../packages/pdf-structure/src/tree/hash'
import type { OcrEngine, OcrRecognition } from '../../../../packages/pdf2docx/src/ocr'

interface Request {
  operation: 'tree' | 'fingerprints' | 'blankPages' | 'images'
  bytes: ArrayBuffer
  password?: string
  pages?: number[]
  minImagePx?: number
  ocrAvailable: boolean
}
interface OcrRequest {
  type: 'ocr'
  requestId: number
  png: ArrayBuffer
  page: { widthPt: number; heightPt: number }
  response: SharedArrayBuffer
}

const worker = self as unknown as {
  onmessage: ((event: MessageEvent<Request>) => void) | null
  postMessage(message: unknown, transfer?: Transferable[]): void
}
const MAX_RESPONSE = 64 * 1024 * 1024
let nextOcr = 0

function ocr(): OcrEngine {
  return (png, page): OcrRecognition | null => {
    const GrowableSharedArrayBuffer = SharedArrayBuffer as unknown as new (
      length: number,
      options: { maxByteLength: number },
    ) => SharedArrayBuffer
    const response = new GrowableSharedArrayBuffer(8, { maxByteLength: 8 + MAX_RESPONSE })
    if (typeof (response as SharedArrayBuffer & { grow?: unknown }).grow !== 'function') {
      throw new Error('growable SharedArrayBuffer is required for OCR')
    }
    const control = new Int32Array(response, 0, 2)
    const data =
      png.byteOffset === 0 &&
      png.byteLength === png.buffer.byteLength &&
      png.buffer instanceof ArrayBuffer
        ? png.buffer
        : png.slice().buffer
    const message: OcrRequest = { type: 'ocr', requestId: ++nextOcr, png: data, page, response }
    worker.postMessage(message, [data])
    Atomics.wait(control, 0, 0)
    if (Atomics.load(control, 0) === 3) Atomics.wait(control, 0, 3)
    if (Atomics.load(control, 0) !== 1) throw new Error('native OCR request failed')
    const length = Atomics.load(control, 1)
    if (
      !Number.isSafeInteger(length) ||
      length < 0 ||
      length > MAX_RESPONSE ||
      8 + length > response.byteLength
    )
      throw new Error('invalid native OCR response')
    const value: unknown = JSON.parse(new TextDecoder().decode(new Uint8Array(response, 8, length)))
    if (value === null) return null
    if (!value || typeof value !== 'object' || !('lines' in value) || !Array.isArray(value.lines))
      throw new Error('invalid native OCR response')
    return value as OcrRecognition
  }
}

function transferables(value: unknown, buffers: Set<ArrayBuffer>, seen: Set<object>): void {
  if (!value || typeof value !== 'object' || seen.has(value)) return
  seen.add(value)
  if (value instanceof Uint8Array) {
    if (value.buffer instanceof ArrayBuffer) buffers.add(value.buffer)
    return
  }
  if (Array.isArray(value)) {
    for (const item of value) transferables(item, buffers, seen)
  } else {
    for (const item of Object.values(value)) transferables(item, buffers, seen)
  }
}

worker.onmessage = async (event) => {
  try {
    const request = event.data
    const bytes = new Uint8Array(request.bytes)
    const onProgress = (done: number, total: number) =>
      worker.postMessage({ type: 'progress', done, total })
    const result =
      request.operation === 'fingerprints'
        ? {
            docHash: sha256Hex(bytes),
            fingerprints: await pageFingerprints(bytes, request.password, onProgress),
          }
        : request.operation === 'blankPages'
          ? await blankPages(bytes, request.password, onProgress)
          : request.operation === 'images'
            ? await extractImages(
                bytes,
                request.password,
                request.pages,
                request.minImagePx,
                onProgress,
              )
            : await extractStructure({
                bytes,
                ...(request.password === undefined ? {} : { password: request.password }),
                ...(request.pages === undefined ? {} : { pages: request.pages }),
                ocr: request.ocrAvailable ? ocr() : null,
                onProgress,
                signal: new AbortController().signal,
              })
    const buffers = new Set<ArrayBuffer>()
    transferables(result, buffers, new Set())
    worker.postMessage({ type: 'result', result }, [...buffers])
  } catch (error) {
    const e = error instanceof Error ? error : new Error(String(error))
    const extra = e as Error & { code?: unknown; validRange?: unknown }
    worker.postMessage({
      type: 'error',
      error: { name: e.name, message: e.message, code: extra.code, validRange: extra.validRange },
    })
  }
}
