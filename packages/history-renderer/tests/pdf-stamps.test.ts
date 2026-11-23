import { createHash } from 'node:crypto'
import type { HistoryRequest, Json } from '@seuoffice/collab/client'
import {
  degrees,
  PDFArray,
  PDFDict,
  PDFDocument,
  PDFName,
  PDFNumber,
  PDFPage,
  PDFStream,
} from 'pdf-lib'
import { expect, it } from 'vitest'
import { render } from './render'

async function basePages() {
  const doc = await PDFDocument.create()
  doc.addPage([200, 300])
  doc.addPage([220, 300])
  doc.addPage([400, 600]).setRotation(degrees(90))
  return doc.save({ useObjectStreams: false })
}

function request(bytes: Uint8Array, batches: Json[][]): HistoryRequest {
  return {
    epoch: 'stamp-test',
    format: { name: 'pdf', schema: 2 },
    baseHash: createHash('sha256').update(bytes).digest('hex'),
    version: batches.length,
    changes: batches.map((ops, i) => ({ v: i + 1, client: `p${i}`, author: `p${i}`, seq: 1, ops })),
  }
}
const mark = { text: ' H ', sizeRatio: 0.1, color: '#1238', opacity: 0.5, angle: 90 }
const bar = {
  headerLeft: 'L {page}/{total}',
  headerCenter: '{total}',
  headerRight: 'I',
  footerLeft: '€',
  footerCenter: 'ignored',
  footerRight: '',
  pageNumber: true,
  startAt: 7,
  fontSize: 10,
  color: '#336699',
}
const cfg = (id: string, wm: Json, hf: Json) => ({ id, puts: [{ b: 'stampCfg', v: { wm, hf } }] })

function contents(page: PDFPage): string {
  const value = page.node.Contents()
  if (!value) return ''
  const streams =
    value instanceof PDFArray
      ? value.asArray().map((v) => page.doc.context.lookup(v, PDFStream))
      : [value]
  return streams.map((s) => Buffer.from(s.getContents()).toString('ascii')).join('\n')
}

function textRuns(page: PDFPage) {
  const resources = page.node.Resources()!
  return [
    ...contents(page).matchAll(
      /q\s+(?:(\/\S+) gs )?BT ([\d. ]+) rg (\/\S+) ([\d.]+) Tf ([-\d. ]+) Tm <([\dA-F]+)> Tj ET Q/g,
    ),
  ].map(([, gs, rgb, font, size, matrix, hex]) => ({
    text: Buffer.from(hex!, 'hex').toString('hex'),
    face: resources
      .lookup(PDFName.Font, PDFDict)
      .lookup(PDFName.of(font!.slice(1)), PDFDict)
      .get(PDFName.of('BaseFont'))!
      .toString(),
    alpha: gs
      ? resources
          .lookup(PDFName.ExtGState, PDFDict)
          .lookup(PDFName.of(gs.slice(1)), PDFDict)
          .lookup(PDFName.of('ca'), PDFNumber)
          .asNumber()
      : 1,
    rgb: rgb!.split(' ').map(Number),
    size: Number(size),
    matrix: matrix!.split(' ').map(Number),
  }))
}

it('places real stamp runs on the visible unrotated pages and numbers them in final order', async () => {
  const base = await basePages()
  const order: Json = {
    id: 'pages',
    puts: [
      { b: 'order', was: [0, 1, 2], v: [2, 1, 0] },
      { b: 'deleted', k: 1, v: true },
    ],
  }
  const prefix = await render(base, request(base, [[cfg('stamp', mark, bar)], [order]]))
  const doc = await PDFDocument.load(prefix.bytes)
  expect(doc.getPages().map((p) => [p.getWidth(), p.getRotation().angle])).toEqual([
    [400, 90],
    [200, 0],
  ])
  for (const [i, p] of doc.getPages().entries()) {
    const runs = textRuns(p)
    // WinAnsi byte 0x80 represents the euro sign, unlike Latin-1.
    expect(runs.map((r) => r.text)).toEqual([
      '48',
      Buffer.from(`L ${7 + i}/2`).toString('hex'),
      '32',
      '49',
      '80',
      Buffer.from(`${7 + i} / 2`).toString('hex'),
    ])
  }
  const [wm, left, center, right, footer] = textRuns(doc.getPage(0))
  expect(wm).toMatchObject({
    face: '/Helvetica-Bold',
    size: 40,
    rgb: [0.06667, 0.13333, 0.2],
    alpha: 0.26667,
    matrix: [0, 1, -1, 0, 210.22, 285.56],
  })
  expect(left).toMatchObject({
    face: '/Helvetica',
    size: 10,
    rgb: [0.2, 0.4, 0.6],
    alpha: 1,
    matrix: [1, 0, 0, 1, 24, 565.445],
  })
  expect(center!.matrix[4]).toBeCloseTo(197.22)
  expect(right!.matrix[4]).toBeCloseTo(373.22)
  expect(footer!.matrix[5]).toBeCloseTo(29.445)
  const reset = await render(
    prefix.bytes,
    request(prefix.bytes, [[{ id: 'clear-new', puts: [{ b: 'stampCfg', v: null }] }]]),
  )
  expect(reset.bytes).toEqual(prefix.bytes)
})

it('fits a long watermark to the diagonal and expands footer templates when automatic numbering is off', async () => {
  const base = await basePages()
  const stamp = cfg(
    'long',
    { ...mark, text: 'H'.repeat(50), angle: 0, sizeRatio: 1 },
    {
      ...bar,
      pageNumber: false,
      startAt: 0,
      footerCenter: '{page} of {total} ({page})',
      color: '#abcd',
    },
  )
  const result = await render(base, request(base, [[stamp]]))
  const runs = textRuns((await PDFDocument.load(result.bytes)).getPage(0))
  expect(runs[0]!.size * 50 * 0.722).toBeCloseTo(0.8 * Math.hypot(200, 300), 3)
  expect(runs.at(-1)).toMatchObject({
    text: Buffer.from('0 of 3 (0)').toString('hex'),
    alpha: 0.86667,
  })
})

it('reopens distinct pinned bold and regular Hangul contours and admits those same bytes as a new base', async () => {
  const base = await basePages()
  const stamp = cfg('hangul', { ...mark, text: '한글' }, { ...bar, headerLeft: '한 {page}' })
  const out = await render(base, request(base, [[stamp]]))
  const title = { id: 'peer', puts: [{ b: 'metadata', v: { title: '동료' } }] }
  const next = await render(out.bytes, request(out.bytes, [[title]]))
  const reopened = await PDFDocument.load(next.bytes)
  expect(reopened.getTitle()).toBe('동료')
  const paint = contents(reopened.getPage(0))
  expect(paint).toMatch(/\/ActualText <FEFFD55C> >> BDC 1498 1634 m 1242 1634 l[^\n]+ f EMC/)
  expect(paint).toMatch(/\/ActualText <FEFFD55C> >> BDC 1462 1614 m 1306 1614 l[^\n]+ f EMC/)
  expect(paint).toContain('/ActualText <FEFFAE00>')
  expect(paint).toContain('/ActualText <FEFF0037>')
})

it('selectively restores the earlier stamp without losing peer metadata and refuses overlapping writes', async () => {
  const base = await basePages()
  const first = cfg('before', { ...mark, text: 'Before' }, null)
  const chosen = cfg('chosen', null, bar)
  const peer = { id: 'peer', puts: [{ b: 'metadata', v: { title: 'Retained peer title' } }] }
  const batches = [[first], [chosen], [peer]]
  const undo = { epoch: 'stamp-test', v: 2, index: 0 }
  const saved = await render(base, { ...request(base, batches), undo })
  const opened = await PDFDocument.load(saved.bytes)
  expect(opened.getTitle()).toBe('Retained peer title')
  for (const p of opened.getPages())
    expect(textRuns(p).map((r) => r.text)).toEqual([Buffer.from('Before').toString('hex')])
  expect(saved.compensation).toEqual([{ id: expect.any(String), puts: first.puts }])
  const appended = await render(base, request(base, [...batches, saved.compensation!]))
  expect(appended.bytes).toEqual(saved.bytes)
  await expect(
    render(base, {
      ...request(base, [[first], [chosen], [cfg('restore', first.puts[0]!.v.wm, null)]]),
      undo,
    }),
  ).rejects.toMatchObject({ code: 'conflict' })
})

it('refuses missing glyphs, shaped scripts and stamps exceeding the PDF paint budget', async () => {
  const base = await basePages()
  for (const text of ['😀', '한😀', 'سلام', 'H'.repeat(1100)]) {
    await expect(
      render(base, request(base, [[cfg('bad', { ...mark, text }, null)]])),
    ).rejects.toMatchObject({ code: 'unsupported' })
  }
})
