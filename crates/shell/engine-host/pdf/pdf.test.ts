import { describe, expect, it, vi } from 'vitest'
import { PDFArray, PDFDocument, PDFName, degrees } from 'pdf-lib'
import { createRequire } from 'node:module'
import { readFile } from 'node:fs/promises'
import { applySaveRequest, cropPagesBytes, extractPagesBytes, insertBlankPageBytes, mergePagesBytes, replacePagesBytes, setPageSizeBytes, splitPagesBytes } from '../../../../apps/pdf/src/main/save-pdf'
import { readPdfText } from '../../../../apps/pdf/src/main/read-text'
import { hasPdfOperation, pdfPageOperation, pdfStructureOperation } from '../pdf'

const encoded = (bytes: Uint8Array) => Buffer.from(bytes).toString('base64')
const decoded = (value: unknown) => Uint8Array.from(Buffer.from(value as string, 'base64'))

async function fixture() {
  const doc = await PDFDocument.create()
  doc.addPage([200, 300]).setRotation(degrees(90))
  doc.addPage([400, 200])
  return doc.save({ useObjectStreams: false })
}

async function geometry(bytes: Uint8Array) {
  const doc = await PDFDocument.load(bytes, { updateMetadata: false })
  return doc.getPages().map(page => ({ media: page.getMediaBox(), crop: page.getCropBox(), rotation: page.getRotation().angle }))
}

describe('PDF page vendor boundary', () => {
  it('rejects unknown operations before reading their input', async () => {
    expect(hasPdfOperation('pdf.unrecognized')).toBe(false)
    await expect(pdfPageOperation('pdf.unrecognized', null)).rejects.toThrow('Unknown PDF operation')
    expect(hasPdfOperation('pdf.pageCount')).toBe(true)
    await expect(pdfPageOperation('pdf.pageCount', {})).rejects.toThrow('bytesBase64 must be a string')
  })
  it('rejects malformed structure jobs before starting a worker', () => {
    const callbacks = { ocr: null, onProgress: () => {}, signal: new AbortController().signal }
    expect(() => pdfStructureOperation({ operation: 'unknown', bytesBase64: '' }, callbacks)).toThrow('invalid PDF structure operation')
    expect(() => pdfStructureOperation({ operation: 'tree', bytesBase64: '', pages: ['1'] }, callbacks)).toThrow('pages must be a number')
  })
  it('stitches workbook pages with the original odd-pass fallback', async () => {
    const odd = await fixture()
    const firstDoc = await PDFDocument.create()
    firstDoc.addPage([75, 100])
    const first = await firstDoc.save()
    const result = await pdfPageOperation('pdf.stitchPages', {
      oddBase64: encoded(odd),
      firstBase64: encoded(first),
      evenBase64: null,
      steps: [{ source: 'first', index: 0, page: 1 }, { source: 'even', index: 0, page: 2 }],
    })
    const pages = await geometry(decoded(result))
    expect(pages.map(page => [page.media.width, page.media.height])).toEqual([[75, 100], [400, 200]])
  })

  it('preserves bytes when document links are invalid and stamps valid links', async () => {
    const bytes = await fixture()
    const input = encoded(bytes)
    const invalid = [{ page: 5, toPage: 0, x: 0, y: 0, w: 0.2, h: 0.2, toY: 0 }]
    expect(await pdfPageOperation('pdf.docsAddInternalLinks', { bytesBase64: input, links: invalid })).toBe(input)
    const links = [{ page: 0, toPage: 1, x: 0, y: 0, w: 0.2, h: 0.2, toY: 0 }]
    const result = await pdfPageOperation('pdf.docsAddInternalLinks', { bytesBase64: input, links })
    const doc = await PDFDocument.load(decoded(result))
    const annots = doc.getPage(0).node.lookup(PDFName.of('Annots'))
    expect(annots).toBeInstanceOf(PDFArray)
    expect((annots as PDFArray).size()).toBe(1)
  })


  it('matches the original page geometry for edits and rotations', async () => {
    const bytes = await fixture()
    const common = { bytesBase64: encoded(bytes) }
    const rect = { l: 0.1, t: 0.2, r: 0.8, b: 0.9 }
    const cases: Array<[string, Record<string, unknown>, Promise<Uint8Array>]> = [
      ['pdf.extractPages', { pages: [1, 0] }, extractPagesBytes(bytes, [1, 0])],
      ['pdf.insertBlankPage', { after: 0 }, insertBlankPageBytes(bytes, 0)],
      ['pdf.cropPages', { pages: [0, 1], rect }, cropPagesBytes(bytes, [0, 1], rect)],
      ['pdf.setPageSize', { width: 500, height: 600 }, setPageSizeBytes(bytes, 500, 600)],
      ['pdf.splitPages', { perPage: 4 }, splitPagesBytes(bytes, 4)],
      ['pdf.mergePages', { perSheet: 2, direction: 'horizontal', separator: true }, mergePagesBytes(bytes, { perSheet: 2, direction: 'horizontal', separator: true })],
    ]
    for (const [name, args, expected] of cases) {
      const actual = await pdfPageOperation(name, { ...common, ...args })
      expect(await geometry(decoded(actual))).toEqual(await geometry(await expected))
    }
  })

  it('matches page-only editor saves for rotation and page deletion', async () => {
    const source = await fixture()
    const rotations = [{ pageIndex: 0, delta: 90 }, { pageIndex: 1, delta: 270 }]
    const deletedPages = [1]
    const expected = await applySaveRequest(source, { path: '', markups: [], drawings: [], formValues: [], stamps: [], rotations, deletedPages })
    const actual = await pdfPageOperation('pdf.applyPageEdits', { bytesBase64: encoded(source), rotations, deletedPages })
    expect(await geometry(decoded(actual))).toEqual(await geometry(expected.bytes))
  })

  it('appends blank pages with their requested dimensions', async () => {
    const source = await fixture()
    const actual = await pdfPageOperation('pdf.appendBlankPages', {
      bytesBase64: encoded(source), pages: [{ width: 321, height: 456 }],
    })
    const pages = await geometry(decoded(actual))
    expect(pages).toHaveLength(3)
    expect(pages[2]?.media).toMatchObject({ width: 321, height: 456 })
  })

  it('writes metadata and the supplied modification time', async () => {
    const source = await fixture()
    const actual = await pdfPageOperation('pdf.applyMetadata', {
      bytesBase64: encoded(source), metadata: { title: 'Title', author: 'Author', keywords: ['one', 'two'] }, modifiedAt: 1700000000000,
    })
    const doc = await PDFDocument.load(decoded(actual), { updateMetadata: false })
    expect(doc.getTitle()).toBe('Title')
    expect(doc.getAuthor()).toBe('Author')
    expect(doc.getKeywords()).toBe('one two')
    expect(doc.getModificationDate()?.getTime()).toBe(1700000000000)
  })

  it('extracts text with the same PDFium engine as the editor', async () => {
    const doc = await PDFDocument.create()
    const page = doc.addPage([200, 300])
    page.drawText('PDFium text')
    const bytes = await doc.save()
    const wasm = await readFile(createRequire(import.meta.url).resolve('@embedpdf/pdfium/pdfium.wasm'))
    vi.stubGlobal('location', { href: 'https://engine-host/' })
    vi.stubGlobal('fetch', async () => new Response(new Uint8Array(wasm).buffer))
    try {
      const expected = await readPdfText(bytes)
      const actual = await pdfPageOperation('pdf.readPdfText', { bytesBase64: encoded(bytes) })
      expect(actual).toEqual({ pageCount: expected.pageCount, pages: expected.pages.map(page => page.text) })
      const expectedLimited = await readPdfText(bytes, { pages: [1], charBudget: 4 })
      const actualLimited = await pdfPageOperation('pdf.readPdfDocument', { bytesBase64: encoded(bytes), pages: [1], charBudget: 4 })
      expect(actualLimited).toEqual(expectedLimited)
    } finally {
      vi.unstubAllGlobals()
    }
  })

  it('preserves replacement counts and page order', async () => {
    const bytes = await fixture()
    const original = await replacePagesBytes(bytes, bytes, [1])
    const result = await pdfPageOperation('pdf.replacePages', { bytesBase64: encoded(bytes), otherBase64: encoded(bytes), pages: [1] }) as { merged: string; removed: number; inserted: number }
    expect([result.removed, result.inserted]).toEqual([original.removed, original.inserted])
    expect(await geometry(decoded(result.merged))).toEqual(await geometry(original.merged))
  })
})
