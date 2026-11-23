import { Buffer } from 'node:buffer'
import { createHash } from 'node:crypto'
import type { HistoryRequest, Json } from '@seuoffice/collab/client'
import { PDFDocument } from 'pdf-lib'
import { expect, it } from 'vitest'
import { render } from './render'

const pageTree = '/Type /Pages\n/Count 1\n/Kids [ 4 0 R ]'
const mediaBox = '0.0 0.0 200 200'

// Matches PdfWriter.add_blank_page(200, 200) with /Title Base in pypdf 7.1.1.
function blankPdf(tree = pageTree, box = mediaBox) {
  let text = '%PDF-1.3\n%\xe2\xe3\xcf\xd3\n'
  const offsets: number[] = []
  for (const body of [
    '/Producer (pypdf)\n/Title (Base)',
    tree,
    '/Type /Catalog\n/Pages 2 0 R',
    `/Type /Page\n/Resources <<\n>>\n/MediaBox [ ${box} ]\n/Parent 2 0 R`,
  ]) {
    offsets.push(text.length)
    text += `${offsets.length} 0 obj\n<<\n${body}\n>>\nendobj\n`
  }
  const startxref = text.length
  text += 'xref\n0 5\n0000000000 65535 f \n'
  for (const offset of offsets) text += `${String(offset).padStart(10, '0')} 00000 n \n`
  text += `trailer\n<<\n/Size 5\n/Root 3 0 R\n/Info 1 0 R\n>>\nstartxref\n${startxref}\n%%EOF\n`
  return Buffer.from(text, 'latin1')
}

function request(base: Uint8Array, changes: Json[][] = []): HistoryRequest {
  return {
    epoch: 'pypdf',
    format: { name: 'pdf', schema: 2 },
    baseHash: createHash('sha256').update(base).digest('hex'),
    version: changes.length,
    changes: changes.map((ops, i) => ({ client: `c${i}`, author: `a${i}`, seq: 1, v: i + 1, ops })),
  }
}

async function checkPdf(bytes: Uint8Array, title: string, angle: number) {
  const doc = await PDFDocument.load(bytes, { updateMetadata: false })
  expect(doc.getTitle()).toBe(title)
  expect(doc.getProducer()).toBe('pypdf')
  expect(doc.getPageCount()).toBe(1)
  expect(doc.getPage(0).getMediaBox()).toEqual({ x: 0, y: 0, width: 200, height: 200 })
  expect(doc.getPage(0).getRotation().angle).toBe(angle)
}

it('edits the pypdf blank page and replays title compensation without undoing a peer rotation', async () => {
  const base = blankPdf()
  expect(request(base).baseHash).toBe(
    '70dd3b04cbd13cfb62230b09774dd6fca6290954bc90484a73374f4ff0b4ef88',
  )
  expect(Buffer.from((await render(base, request(base))).bytes)).toEqual(base)
  const edits: Json[][] = [
    [{ id: 'title', puts: [{ b: 'metadata', v: { title: 'Edited' } }] }],
    [{ id: 'peer', puts: [{ b: 'rotations', k: 0, was: 0, v: 90 }] }],
  ]
  await checkPdf((await render(base, request(base, edits))).bytes, 'Edited', 90)
  const undone = await render(base, {
    ...request(base, edits),
    undo: { epoch: 'pypdf', v: 1, index: 0 },
  })
  await checkPdf(undone.bytes, 'Base', 90)
  expect(undone.compensation).toEqual([
    { id: expect.any(String), puts: [{ b: 'metadata', v: null }] },
  ])
  const replay = await render(base, request(base, [...edits, undone.compensation!]))
  expect(replay.bytes).toEqual(undone.bytes)
  await checkPdf(replay.bytes, 'Base', 90)
})

it.each([
  [`${pageTree}\n/Count 1`, mediaBox],
  [`${pageTree}\n/Rotate 90`, mediaBox],
  [pageTree.replace('/Count 1', '/Count 2'), mediaBox],
  [pageTree, '0.1 0.0 200 200'],
  [pageTree, '0.0 0.1 200 200'],
  [pageTree, '0.0 0.0 14401 200'],
])('refuses out-of-profile page trees and boxes (%s, %s)', async (tree, box) => {
  const base = blankPdf(tree, box)
  await expect(render(base, request(base))).rejects.toMatchObject({ code: 'unsupported' })
})
