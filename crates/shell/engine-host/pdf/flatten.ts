import { withPdfium } from './pdfium-text'

interface PdfiumFlatten {
  HEAPU8: Uint8Array
  _malloc(size: number): number
  _free(ptr: number): void
  _FPDF_LoadMemDocument(ptr: number, size: number, password: number): number
  _FPDF_CloseDocument(doc: number): void
  _FPDF_GetPageCount(doc: number): number
  _FPDF_LoadPage(doc: number, index: number): number
  _FPDF_ClosePage(page: number): void
  _FPDFPage_GetAnnotCount(page: number): number
  _FPDFPage_Flatten(page: number, flags: number): number
  _PDFiumExt_OpenFileWriter(): number
  _PDFiumExt_SaveAsCopy(doc: number, writer: number): number
  _PDFiumExt_GetFileWriterSize(writer: number): number
  _PDFiumExt_GetFileWriterData(writer: number, buf: number, size: number): number
  _PDFiumExt_CloseFileWriter(writer: number): void
}

export function flattenAnnotations(bytes: Uint8Array): Promise<Uint8Array> {
  return withPdfium((raw) => {
    const m = raw as PdfiumFlatten
    const ptr = m._malloc(bytes.length)
    m.HEAPU8.set(bytes, ptr)
    const doc = m._FPDF_LoadMemDocument(ptr, bytes.length, 0)
    if (!doc) {
      m._free(ptr)
      throw new Error('PDFium could not load the document')
    }
    try {
      let changed = false
      const count = m._FPDF_GetPageCount(doc)
      for (let i = 0; i < count; i++) {
        const page = m._FPDF_LoadPage(doc, i)
        if (!page) throw new Error(`could not load page ${i + 1}`)
        try {
          if (m._FPDFPage_GetAnnotCount(page) === 0) continue
          if (m._FPDFPage_Flatten(page, 0) === 0)
            throw new Error(`could not flatten annotations on page ${i + 1}`)
          changed = true
        } finally {
          m._FPDF_ClosePage(page)
        }
      }
      if (!changed) return bytes
      const writer = m._PDFiumExt_OpenFileWriter()
      try {
        if (!m._PDFiumExt_SaveAsCopy(doc, writer)) throw new Error('PDFium SaveAsCopy failed')
        const size = m._PDFiumExt_GetFileWriterSize(writer)
        const buf = m._malloc(size)
        try {
          m._PDFiumExt_GetFileWriterData(writer, buf, size)
          return Uint8Array.from(m.HEAPU8.subarray(buf, buf + size))
        } finally {
          m._free(buf)
        }
      } finally {
        m._PDFiumExt_CloseFileWriter(writer)
      }
    } finally {
      m._FPDF_CloseDocument(doc)
      m._free(ptr)
    }
  })
}
