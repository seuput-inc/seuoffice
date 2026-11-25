import '@seuoffice/engine-wasm/node'
import type * as DocxEngine from '@seuoffice/docx-engine'
import JSZip from 'jszip'
import { expect, it, vi } from 'vitest'
import { buildDocx } from '../../../docx-engine/tests/helpers/build-docx'

const parse = vi.hoisted(() => ({ calls: 0 }))
vi.mock('@seuoffice/docx-engine', async (original) => {
  const engine = await original<typeof DocxEngine>()
  return {
    ...engine,
    parseDocx: (...args: Parameters<typeof engine.parseDocx>) => {
      parse.calls++
      return engine.parseDocx(...args)
    },
  }
})
const { docxModel } = await import('../../src/docx')

it('refuses an imported chunk before the engine reads the package', async () => {
  // An inner package the archive gate never measures: 8 MiB of zeros in a few kilobytes.
  const inner = new JSZip()
  inner.file('word/document.xml', new Uint8Array(8 * 1024 * 1024), { binary: true })
  const chunk = await inner.generateAsync({ type: 'uint8array', compression: 'DEFLATE' })
  expect(chunk.length).toBeLessThan(64 * 1024)
  const bytes = await buildDocx({
    bodyXml: '<w:p><w:r><w:t>before</w:t></w:r></w:p><w:altChunk r:id="chunk"/>',
    extraRels:
      '<Relationship Id="chunk" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/aFChunk" Target="chunk.docx"/>',
    binaryParts: [
      {
        path: 'word/chunk.docx',
        base64: Buffer.from(chunk).toString('base64'),
        extension: 'docx',
        contentType: 'application/vnd.openxmlformats-officedocument.wordprocessingml.document',
      },
    ],
  })
  await expect(docxModel(bytes)).rejects.toMatchObject({
    code: 'unsupported',
    message: 'a DOCX body with an imported chunk (w:altChunk)',
  })
  expect(parse.calls).toBe(0)
})
