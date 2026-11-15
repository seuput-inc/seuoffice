import { init } from '@embedpdf/pdfium'
import { withPdfDocument, type PdfiumModule } from '../../../../packages/pdf2docx/src/extract/pdfium'
import { pageFingerprint } from '../../../../packages/pdf-structure/src/tree/pdfium-doc'
import { findBlankPages, type BlankPagesPdfium, type BlankPagesResult } from '../../../../apps/pdf/src/main/blank-pages'

interface Pdfium {
  HEAPU8: Uint8Array
  HEAPF64: Float64Array
  _malloc(size: number): number
  _free(ptr: number): void
  _PDFiumExt_Init(): void
  _FPDF_LoadMemDocument(ptr: number, size: number, password: number): number
  _FPDF_GetLastError(): number
  _FPDF_CloseDocument(doc: number): void
  _FPDF_GetPageCount(doc: number): number
  _FPDF_LoadPage(doc: number, index: number): number
  _FPDF_ClosePage(page: number): void
  _FPDF_GetPageWidthF(page: number): number
  _FPDF_GetPageHeightF(page: number): number
  _FPDF_GetMetaText(doc: number, tag: number, buf: number, size: number): number
  _FPDFText_LoadPage(page: number): number
  _FPDFText_ClosePage(page: number): void
  _FPDFText_CountChars(page: number): number
  _FPDFText_GetText(page: number, start: number, count: number, buf: number): number
  _FPDFText_GetCharOrigin(page: number, index: number, x: number, y: number): number
  _FPDFText_GetFontSize(page: number, index: number): number
  _FPDFText_IsGenerated(page: number, index: number): number
}

let instance: Promise<Pdfium> | undefined
let chain: Promise<unknown> = Promise.resolve()

async function pdfium(): Promise<Pdfium> {
  instance ??= (async () => {
    const response = await fetch(new URL('./pdfium.wasm', globalThis.location.href))
    if (!response.ok) throw new Error(`Cannot load PDFium: ${response.status}`)
    const initialize = init as unknown as (overrides: object) => Promise<{ pdfium: Pdfium } | Pdfium>
    const wrapped = await initialize({ wasmBinary: await response.arrayBuffer(), thisProgram: 'seuoffice-pdf' })
    const module = ('pdfium' in wrapped ? wrapped.pdfium : wrapped) as unknown as Pdfium
    module._PDFiumExt_Init()
    return module
  })()
  return instance
}

function readPage(m: Pdfium, textPage: number, count: number): string {
  if (count <= 0) return ''
    const buf = m._malloc((count + 1) * 2)
    if (!buf) return ''
    try {
      const written = m._FPDFText_GetText(textPage, 0, count, buf)
      if (written <= 0) return ''
      let text = new TextDecoder('utf-16le', { ignoreBOM: true }).decode(m.HEAPU8.subarray(buf, buf + written * 2))
      if (text.replace(/\0+$/, '').length === count) {
        const xy = m._malloc(16)
        if (xy) {
          try {
            const origin = (i: number): [number, number] => {
              m._FPDFText_GetCharOrigin(textPage, i, xy, xy + 8)
              return [m.HEAPF64[xy >> 3]!, m.HEAPF64[(xy >> 3) + 1]!]
            }
            let joined = ''
            for (let i = 0; i < count; i++) {
              if (text[i] !== '\r' || text[i + 1] !== '\n' || i === 0 || i + 2 >= count || m._FPDFText_IsGenerated(textPage, i) !== 1) {
                joined += text[i]
                continue
              }
              const [px, py] = origin(i - 1)
              const [nx, ny] = origin(i + 2)
              const size = Math.min(m._FPDFText_GetFontSize(textPage, i - 1), m._FPDFText_GetFontSize(textPage, i + 2))
              if (size > 0 && Math.abs(ny - py) < size * 0.3 && nx > px) {
                if (nx - px > size * 2) joined += ' '
                i++
              } else joined += text[i]
            }
            text = joined
          } finally { m._free(xy) }
        }
      }
      return text.replace(/\0+$/, '').replace(/\r\n/g, '\n').replace(/\r/g, '\n').trimEnd()
    } finally { m._free(buf) }
}

export interface PdfiumTextOptions {
  fromPage?: number
  toPage?: number
  pages?: number[]
  charBudget?: number
  password?: string
}

function metaText(m: Pdfium, doc: number, tag: string): string {
  const encoded = new TextEncoder().encode(`${tag}\0`)
  const tagPtr = m._malloc(encoded.length)
  if (!tagPtr) return ''
  try {
    m.HEAPU8.set(encoded, tagPtr)
    const size = m._FPDF_GetMetaText(doc, tagPtr, 0, 0)
    if (size <= 2) return ''
    const buf = m._malloc(size)
    if (!buf) return ''
    try {
      const written = m._FPDF_GetMetaText(doc, tagPtr, buf, size)
      if (written <= 0) return ''
      return new TextDecoder('utf-16le', { ignoreBOM: true }).decode(m.HEAPU8.subarray(buf, buf + written - (written % 2))).replace(/\0+$/, '')
    } finally { m._free(buf) }
  } finally { m._free(tagPtr) }
}

export function readPdfiumDocument(bytes: Uint8Array, options: PdfiumTextOptions = {}) {
  const task = chain.then(async () => {
    const m = await pdfium()
    const docPtr = m._malloc(bytes.length)
    m.HEAPU8.set(bytes, docPtr)
    let pwPtr = 0
    if (options.password) {
      const pw = new TextEncoder().encode(options.password)
      pwPtr = m._malloc(pw.length + 1)
      m.HEAPU8.set(pw, pwPtr)
      m.HEAPU8[pwPtr + pw.length] = 0
    }
    const doc = m._FPDF_LoadMemDocument(docPtr, bytes.length, pwPtr)
    if (pwPtr) m._free(pwPtr)
    if (!doc) {
      m._free(docPtr)
      throw new Error('PDFium could not load the document')
    }
    try {
      const pageCount = m._FPDF_GetPageCount(doc)
      const from = Math.max(1, Math.min(options.fromPage ?? 1, pageCount))
      const to = Math.max(from, Math.min(options.toPage ?? pageCount, pageCount))
      const wanted = options.pages
        ? [...new Set(options.pages.filter(page => page >= 1 && page <= pageCount))].sort((a, b) => a - b)
        : Array.from({ length: to - from + 1 }, (_, i) => from + i)
      const info: { title?: string; author?: string } = {}
      const title = metaText(m, doc, 'Title')
      if (title) info.title = title
      const author = metaText(m, doc, 'Author')
      if (author) info.author = author
      const pages: { page: number; widthPt: number; heightPt: number; chars: number; text: string; hasTextLayer: boolean }[] = []
      const budget = options.charBudget ?? Infinity
      let spent = 0
      let truncated = false
      for (const index of wanted) {
        if (truncated) break
        const page = m._FPDF_LoadPage(doc, index - 1)
        if (!page) {
          pages.push({ page: index, widthPt: 0, heightPt: 0, chars: 0, text: '', hasTextLayer: false })
          continue
        }
        try {
          const widthPt = m._FPDF_GetPageWidthF(page)
          const heightPt = m._FPDF_GetPageHeightF(page)
          const textPage = m._FPDFText_LoadPage(page)
          if (!textPage) {
            pages.push({ page: index, widthPt, heightPt, chars: 0, text: '', hasTextLayer: false })
            continue
          }
          try {
            const chars = m._FPDFText_CountChars(textPage)
            if (spent >= budget) { truncated = true; break }
            let text = readPage(m, textPage, chars)
            if (spent + text.length > budget) {
              text = text.slice(0, Math.max(0, budget - spent))
              truncated = true
            }
            spent += text.length
            pages.push({ page: index, widthPt, heightPt, chars, text, hasTextLayer: chars > 0 })
          } finally { m._FPDFText_ClosePage(textPage) }
        } finally { m._FPDF_ClosePage(page) }
      }
      return { pageCount, info, pages, truncated }
    } finally {
      m._FPDF_CloseDocument(doc)
      m._free(docPtr)
    }
  })
  chain = task.catch(() => undefined)
  return task
}

export async function readPdfiumText(bytes: Uint8Array, password?: string): Promise<{ pageCount: number; pages: string[] }> {
  const result = await readPdfiumDocument(bytes, password === undefined ? {} : { password })
  return { pageCount: result.pageCount, pages: result.pages.map(page => page.text) }
}

export function pdfiumOpens(bytes: Uint8Array, password: string): Promise<'ok' | 'wrong-password' | 'unreadable'> {
  const task = chain.then(async () => {
    const m = await pdfium()
    const ptr = m._malloc(bytes.length)
    const pw = new TextEncoder().encode(`${password}\0`)
    const pwPtr = m._malloc(pw.length)
    if (!ptr || !pwPtr) {
      if (ptr) m._free(ptr)
      if (pwPtr) m._free(pwPtr)
      return 'unreadable' as const
    }
    try {
      m.HEAPU8.set(bytes, ptr)
      m.HEAPU8.set(pw, pwPtr)
      const doc = m._FPDF_LoadMemDocument(ptr, bytes.length, pwPtr)
      if (doc) {
        m._FPDF_CloseDocument(doc)
        return 'ok' as const
      }
      return m._FPDF_GetLastError() === 4 ? 'wrong-password' as const : 'unreadable' as const
    } finally {
      m.HEAPU8.fill(0, pwPtr, pwPtr + pw.length)
      m._free(pwPtr)
      m._free(ptr)
    }
  })
  chain = task.catch(() => undefined)
  return task
}

export function pageFingerprints(bytes: Uint8Array, password?: string, onProgress?: (done: number, total: number) => void): Promise<string[]> {
  const task = chain.then(async () => {
    const module = await pdfium() as unknown as PdfiumModule
    return withPdfDocument(module, bytes, doc => {
      const count = module._FPDF_GetPageCount(doc)
      return Array.from({ length: count }, (_, index) => {
        const fingerprint = pageFingerprint(module, doc, index)
        onProgress?.(index + 1, count)
        return fingerprint
      })
    }, password)
  })
  chain = task.catch(() => undefined)
  return task
}

export function withPdfium<T>(work: (module: unknown) => T | Promise<T>): Promise<T> {
  const task = chain.then(async () => work(await pdfium()))
  chain = task.catch(() => undefined)
  return task
}

export function blankPages(bytes: Uint8Array, password: string | undefined, onProgress: (done: number, total: number) => void): Promise<BlankPagesResult> {
  return withPdfium(raw => {
    const module = raw as PdfiumModule & BlankPagesPdfium
    return withPdfDocument(module, bytes, doc => findBlankPages(module, doc, onProgress), password)
  })
}
