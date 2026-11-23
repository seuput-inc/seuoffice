import { createHash } from 'node:crypto'
import { PDFDict, PDFDocument, PDFName, PDFStream, PDFString } from 'pdf-lib'
import { renderStream } from '@seuoffice/history-renderer'
import { expect, it } from 'vitest'

it('edits a PDF saved by pdf-lib and retains its source image and literal metadata', async () => {
  const pdf = await PDFDocument.create({ updateMetadata: false })
  const page = pdf.addPage([128, 128])
  pdf.setTitle('Preliminary')
  // A literal Info string with significant spaces must survive dictionary normalization.
  const info = pdf.context.lookup(pdf.context.trailerInfo.Info, PDFDict)
  const label = 'Northern  slope'
  info.set(PDFName.of('Author'), PDFString.of(label))
  const pixels = Uint8Array.from(
    { length: 128 * 128 * 3 },
    (_, i) => ((Math.floor(i / 3) % 128) + Math.floor(i / (128 * 3)) * ((i % 3) + 1)) % 256,
  )
  const raster = pdf.context.register(
    pdf.context.stream(pixels, {
      Type: 'XObject',
      Subtype: 'Image',
      Width: 128,
      Height: 128,
      BitsPerComponent: 8,
      ColorSpace: 'DeviceRGB',
    }),
  )
  page.node.set(PDFName.Resources, pdf.context.obj({ XObject: { Map: raster } }))
  page.node.set(
    PDFName.Contents,
    pdf.context.register(pdf.context.stream('q 128 0 0 128 0 0 cm /Map Do Q')),
  )
  const base = await pdf.save({ useObjectStreams: false })
  const baseHash = createHash('sha256').update(base).digest('hex')
  const source = {
    byteLength: base.length,
    async *open() {
      for (let i = 0; i < base.length; i += 997) yield base.subarray(i, i + 997)
    },
  }
  const encode = (x: unknown) => new TextEncoder().encode(JSON.stringify(x))
  const frame = encode({
    v: 1,
    seq: 1,
    author: 'Surveyor',
    client: 'field',
    ops: [{ id: 'sheet', puts: [{ b: 'rotations', k: 0, was: 0, v: 90 }] }],
  })
  for (const version of [0, 1]) {
    const output: Uint8Array[] = []
    const stage = {
      async write(b: Uint8Array) {
        output.push(b.slice())
      },
      async abort() {
        output.length = 0
      },
    }
    const result = await renderStream(
      source,
      encode({ epoch: 'terrain', version, format: { name: 'pdf', schema: 2 }, baseHash }),
      (async function* () {
        if (version) yield frame
      })(),
      stage,
    )
    expect(result).toEqual({ fileName: 'history.pdf' })
    const bytes = Buffer.concat(output)
    if (!version) expect(bytes.equals(base)).toBe(true)
    const reopened = await PDFDocument.load(bytes, { updateMetadata: false })
    expect(reopened.getAuthor()).toBe(label)
    expect(reopened.getTitle()).toBe('Preliminary')
    const p = reopened.getPage(0)
    expect(p.getRotation().angle).toBe(version ? 90 : 0)
    const savedPixels = p.node
      .Resources()!
      .lookup(PDFName.XObject, PDFDict)
      .lookup(PDFName.of('Map'), PDFStream)
      .getContents()
    expect(Buffer.compare(savedPixels, pixels)).toBe(0)
  }
})
