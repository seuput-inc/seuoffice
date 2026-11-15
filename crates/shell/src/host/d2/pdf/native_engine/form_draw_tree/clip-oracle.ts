// Run from the repository root with: node_modules/.bin/tsx crates/shell/src/host/d2/pdf/native_engine/form_draw_tree/clip-oracle.ts
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import { PDFDocument, rgb } from 'pdf-lib'
import { init } from '@embedpdf/pdfium'
import { liftOutOfForms } from '../../../../../../../../apps/pdf/src/main/form-text'
import type { Pdfium } from '../../../../../../../../apps/pdf/src/main/text-edit'

type OraclePdfium = Pdfium & {
  _FPDFPageObj_GetClipPath(object: number): number
  _FPDFClipPath_CountPaths(path: number): number
}

async function fixture(nested: boolean): Promise<Uint8Array> {
  const source = await PDFDocument.create()
  source
    .addPage([100, 100])
    .drawRectangle({ x: 0, y: 0, width: 100, height: 100, color: rgb(1, 0, 0) })
  let inner = await PDFDocument.load(await source.save({ useObjectStreams: false }))
  if (nested) {
    const middle = await PDFDocument.create()
    const form = await middle.embedPage(inner.getPage(0), {
      left: 0,
      bottom: 0,
      right: 60,
      top: 80,
    })
    middle.addPage([140, 140]).drawPage(form, { x: 10, y: 20, width: 90, height: 80 })
    inner = await PDFDocument.load(await middle.save({ useObjectStreams: false }))
  }
  const doc = await PDFDocument.create()
  const form = await doc.embedPage(
    inner.getPage(0),
    nested
      ? { left: 0, bottom: 0, right: 90, top: 100 }
      : { left: 0, bottom: 0, right: 50, top: 100 },
  )
  doc.addPage([240, 240]).drawPage(form, { x: 35, y: 40, width: 135, height: 150 })
  return doc.save({ useObjectStreams: false })
}

async function main(): Promise<void> {
  const require = createRequire(`${process.cwd()}/package.json`)
  // The package's generated Emscripten declaration omits these exported functions.
  const initialize = init as unknown as (
    options: object,
  ) => Promise<{ pdfium: OraclePdfium } | OraclePdfium>
  const wrapped = await initialize({
    wasmBinary: readFileSync(require.resolve('@embedpdf/pdfium/pdfium.wasm')),
    thisProgram: 'pdf',
  })
  const pdfium = 'pdfium' in wrapped ? wrapped.pdfium : wrapped
  pdfium._PDFiumExt_Init()

  function render(page: number): Uint8Array {
    const bitmap = pdfium._FPDFBitmap_Create(240, 240, 1)
    pdfium._FPDFBitmap_FillRect(bitmap, 0, 0, 240, 240, 0xffffffff)
    pdfium._FPDF_RenderPageBitmap(bitmap, page, 0, 0, 240, 240, 0, 0)
    const pointer = pdfium._FPDFBitmap_GetBuffer(bitmap)
    const length = pdfium._FPDFBitmap_GetStride(bitmap) * 240
    const pixels = pdfium.HEAPU8.slice(pointer, pointer + length)
    pdfium._FPDFBitmap_Destroy(bitmap)
    return pixels
  }

  for (const nested of [false, true]) {
    const bytes = await fixture(nested)
    const memory = pdfium._malloc(bytes.length)
    pdfium.HEAPU8.set(bytes, memory)
    const doc = pdfium._FPDF_LoadMemDocument(memory, bytes.length, 0)
    const page = pdfium._FPDF_LoadPage(doc, 0)
    const outer = pdfium._FPDFPage_GetObject(page, 0)
    const inner = nested ? pdfium._FPDFFormObj_GetObject(outer, 0) : outer
    const child = pdfium._FPDFFormObj_GetObject(inner, 0)
    assert.equal(pdfium._FPDFPageObj_GetType(outer), 5)
    assert.equal(pdfium._FPDFPageObj_GetType(child), 2)
    const clip = pdfium._FPDFPageObj_GetClipPath(child)
    assert.ok(clip && pdfium._FPDFClipPath_CountPaths(clip) > 0)
    const before = render(page)
    liftOutOfForms(pdfium, page, child, nested ? [outer, inner] : [outer])
    assert.equal(pdfium._FPDFPage_GenerateContent(page), 1)
    const after = render(page)
    let changed = 0
    for (let i = 0; i < before.length; i += 4) {
      if (
        before[i] !== after[i] ||
        before[i + 1] !== after[i + 1] ||
        before[i + 2] !== after[i + 2] ||
        before[i + 3] !== after[i + 3]
      )
        changed++
    }
    assert.ok(changed > 0, 'the original Form lift loses inherited clip positioning')
    console.log(JSON.stringify({ nested, changedPixels: changed }))
    pdfium._FPDF_ClosePage(page)
    pdfium._FPDF_CloseDocument(doc)
    pdfium._free(memory)
  }
}

void main()
