import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import '@seuoffice/engine-wasm/node'
import { PDFDict, PDFDocument, PDFName, PDFStream, PDFString } from 'pdf-lib'
import { examples } from '../../../apps/web/tests/collab-history-cases'

export const hash = (bytes: Uint8Array) => createHash('sha256').update(bytes).digest('hex')
type Document = (typeof examples)[number] & { id: string; bytes: Uint8Array }
const loadPdf = (bytes: Uint8Array) =>
  PDFDocument.load(bytes, { updateMetadata: false, throwOnInvalidObject: true })

async function survey(example: Document, side: number): Promise<Document> {
  const pdf = await PDFDocument.create({ updateMetadata: false })
  pdf.setTitle('Base')
  const page = pdf.addPage([side, side])
  const pixels = Buffer.alloc(side * side * 3)
  for (let y = 0, i = 0; y < side; y++) {
    for (let x = 0; x < side; x++) {
      pixels[i++] = x % 256
      pixels[i++] = y % 256
      pixels[i++] = (x + y) % 256
    }
  }
  const digest = hash(pixels)
  const raster = pdf.context.register(
    pdf.context.stream(pixels, {
      Type: 'XObject',
      Subtype: 'Image',
      Width: side,
      Height: side,
      BitsPerComponent: 8,
      ColorSpace: 'DeviceRGB',
    }),
  )
  const program = `q ${side} 0 0 ${side} 0 0 cm /Survey Do Q`
  page.node.set(PDFName.Resources, pdf.context.obj({ XObject: { Survey: raster } }))
  page.node.set(PDFName.Contents, pdf.context.register(pdf.context.stream(program)))
  return {
    ...example,
    id: `survey-${side}`,
    bytes: await pdf.save({ useObjectStreams: false }),
    async reopen(bytes) {
      const saved = await loadPdf(bytes)
      assert.equal(saved.getPageCount(), 1)
      const p = saved.getPage(0)
      assert.deepEqual(p.getSize(), { width: side, height: side })
      const image = p.node
        .Resources()!
        .lookup(PDFName.XObject, PDFDict)
        .lookup(PDFName.of('Survey'), PDFStream)
      assert.equal(hash(image.getContents()), digest)
      const contents = p.node.lookup(PDFName.Contents, PDFStream).getContents()
      assert.equal(Buffer.from(contents).toString(), program)
      return [saved.getTitle(), p.getRotation().angle]
    },
  }
}

export async function mixedDocuments() {
  const documents = await Promise.all(
    examples.map(async (example) => ({
      ...example,
      id: example.name,
      bytes: await example.base(),
    })),
  )
  const pdf = documents.find((doc) => doc.id === 'pdf')!
  const small = await survey(pdf, 16)
  const large = await survey(pdf, 1024)
  const ordinary = await loadPdf(pdf.bytes)
  const marker = '/Type /XObject /Subtype /Image /ColorSpace /DeviceRGB /BitsPerComponent 8'
  ordinary.context
    .lookup(ordinary.context.trailerInfo.Info, PDFDict)
    .set(PDFName.of('Subject'), PDFString.of(marker))
  const bytes = Buffer.from(await ordinary.save({ useObjectStreams: false }))
  assert(bytes.includes(marker))
  // Whitespace after the trailer keeps the existing xref offsets and gives both profiles exactly the same size.
  const split = bytes.lastIndexOf('\nstartxref')
  assert(split > 0 && bytes.length < small.bytes.length && small.bytes.length < 1024 * 1024)
  pdf.bytes = Buffer.concat([
    bytes.subarray(0, split),
    Buffer.alloc(small.bytes.length - bytes.length, 10),
    bytes.subarray(split),
  ])
  assert.equal(pdf.bytes.length, small.bytes.length)
  assert(large.bytes.length > 2 * 1024 * 1024)
  pdf.reopen = async (bytes) => {
    const saved = await loadPdf(bytes)
    assert.equal(saved.getSubject(), marker)
    return [saved.getTitle(), saved.getPage(0).getRotation().angle]
  }
  const docx = documents.find((doc) => doc.id === 'docx')!
  const xlsx = documents.find((doc) => doc.id === 'xlsx')!
  const pptx = documents.find((doc) => doc.id === 'pptx')!
  return { docx, xlsx, pptx, pdf, small, large }
}
