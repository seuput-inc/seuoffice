import { extractDocumentTree, PageRangeError, type ExtractTreeResult } from '../../../../packages/pdf-structure/src/tree/extract'
import { sha256Hex } from '../../../../packages/pdf-structure/src/tree/hash'
import type { PdfiumModule } from '../../../../packages/pdf2docx/src/extract/pdfium'
import type { OcrEngine } from '../../../../packages/pdf2docx/src/ocr'
import { extractEmbeddedImages, type EmbeddedImage } from '../../../../packages/pdf-structure/src/images'
import { withPdfDocument } from '../../../../packages/pdf2docx/src/extract/pdfium'
import { withPdfium } from './pdfium-text'

export interface StructureTreeRequest {
  bytes: Uint8Array
  password?: string
  pages?: number[]
  ocr: OcrEngine | null
  onProgress: (done: number, total: number) => void
  signal: AbortSignal
}

export async function extractStructure(request: StructureTreeRequest): Promise<ExtractTreeResult & { docHash: string }> {
  const docHash = sha256Hex(request.bytes)
  const result = await withPdfium(module => extractDocumentTree(request.bytes, {
    pdfium: module as PdfiumModule,
    ...(request.password === undefined ? {} : { password: request.password }),
    ...(request.pages === undefined ? {} : { pages: request.pages }),
    ...(request.ocr === null ? {} : { ocr: request.ocr }),
    onProgress: request.onProgress,
    signal: request.signal,
  }))
  return { docHash, ...result }
}

export function extractImages(
  bytes: Uint8Array,
  password: string | undefined,
  requested: number[] | undefined,
  minImagePx: number | undefined,
  onProgress: (done: number, total: number) => void,
): Promise<{ docHash: string; images: EmbeddedImage[] }> {
  const docHash = sha256Hex(bytes)
  return withPdfium(raw => {
    const module = raw as PdfiumModule
    const pages = withPdfDocument(module, bytes, doc => {
      const count = module._FPDF_GetPageCount(doc)
      if (requested === undefined) return Array.from({ length: count }, (_, i) => i + 1)
      const bad = requested.filter(page => !Number.isInteger(page) || page < 1 || page > count)
      if (requested.length === 0 || bad.length > 0) throw new PageRangeError(count, bad.length ? `page ${bad.join(', ')}` : 'empty range')
      return [...new Set(requested)]
    }, password)
    onProgress(0, pages.length)
    const images = extractEmbeddedImages(module, bytes, pages, {
      ...(password === undefined ? {} : { password }),
      ...(minImagePx === undefined ? {} : { minImagePx }),
    })
    return { docHash, images }
  })
}
