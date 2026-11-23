import assert from 'node:assert/strict'
import { readFile, stat } from 'node:fs/promises'
import {
  decodePDFRawStream,
  PDFArray,
  PDFDict,
  PDFDocument,
  PDFName,
  PDFNumber,
  PDFRawStream,
} from 'pdf-lib'
import { MiB, paint, rectangle, sha } from './stream-proof-fixture.mjs'

const name = PDFName.of
function content(s) {
  return s.dict.has(name('Filter')) ? decodePDFRawStream(s).decode() : s.getContents()
}

process.once('message', async ({ path, meta, angles, images }) => {
  try {
    const { size } = await stat(path)
    assert(size > 0 && size <= 50 * MiB)
    const bytes = await readFile(path)
    const doc = await PDFDocument.load(bytes, { updateMetadata: false, throwOnInvalidObject: true })
    assert.deepEqual(
      [doc.getTitle(), doc.getAuthor(), doc.getSubject(), doc.getKeywords()],
      [meta.title, meta.author, meta.subject, meta.keywords],
    )
    const pages = doc.getPages()
    assert.deepEqual(
      pages.map((p) => p.getRotation().angle),
      angles,
    )
    const pixelHashes = []
    for (const [i, page] of pages.entries()) {
      assert.deepEqual(page.getSize(), { width: 612, height: 792 })
      const contents = page.node.lookup(name('Contents'))
      const streams =
        contents instanceof PDFArray
          ? contents.asArray().map((s) => doc.context.lookup(s, PDFRawStream))
          : [contents]
      const program = streams
        .map((s) => Buffer.from(content(s)).toString('ascii'))
        .join(' ')
        .trim()
        .replace(/\s+/g, ' ')
      if (!images.length) {
        assert.equal(program, rectangle)
        continue
      }
      const invocation = /^q 612 0 0 792 0 0 cm \/([^\s/]+) Do Q$/.exec(program)
      assert(invocation, `page ${i + 1} no longer paints its full-size scan (${paint})`)
      const image = page.node
        .Resources()
        .lookup(name('XObject'), PDFDict)
        .lookup(name(invocation[1]), PDFRawStream)
      const { width, height, byteLength, sha256 } = images[i]
      assert.equal(image.dict.lookup(name('Subtype'), PDFName).toString(), '/Image')
      assert.equal(image.dict.lookup(name('ColorSpace'), PDFName).toString(), '/DeviceRGB')
      assert.equal(image.dict.lookup(name('BitsPerComponent'), PDFNumber).asNumber(), 8)
      assert.equal(image.dict.lookup(name('Width'), PDFNumber).asNumber(), width)
      assert.equal(image.dict.lookup(name('Height'), PDFNumber).asNumber(), height)
      // A mask or decode array could change the display without changing the raw samples.
      for (const k of ['Mask', 'SMask', 'ImageMask', 'Decode']) assert(!image.dict.has(name(k)))
      const pixels = content(image)
      assert.equal(pixels.byteLength, byteLength)
      const digest = sha().update(pixels).digest('hex')
      assert.equal(digest, sha256)
      pixelHashes.push(digest)
    }
    process.send({
      size,
      sha256: sha().update(bytes).digest('hex'),
      title: doc.getTitle(),
      angles,
      pixelHashes,
    })
  } catch (error) {
    console.error(path, error)
    process.exitCode = 1
  } finally {
    process.disconnect()
  }
})
