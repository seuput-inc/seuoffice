import { createHash } from 'node:crypto'
import type { HistoryRequest, Json } from '@seuoffice/collab/client'
import { render } from './render'
import {
  decodePDFRawStream,
  degrees,
  PDFArray,
  PDFDocument,
  PDFName,
  PDFRawStream,
  PDFStream,
  StandardFonts,
} from 'pdf-lib'
import { describe, expect, it } from 'vitest'

const paint = '12 24 96 32 re f\n'
const target = { epoch: 'pdf-test', v: 1, index: 1 }

async function fixture(compress = false) {
  const doc = await PDFDocument.create()
  doc.setTitle('Box drawing')
  doc.addPage([400, 300])
  const first = doc.insertPage(0, [595.28, 841.89])
  first.setRotation(degrees(270))
  const stream = compress ? doc.context.flateStream(paint) : doc.context.stream(paint)
  first.node.set(PDFName.of('Contents'), doc.context.register(stream))
  return doc
}

function request(base: Uint8Array, batches: Json[][]): HistoryRequest {
  return {
    epoch: target.epoch,
    format: { name: 'pdf', schema: 2 },
    baseHash: createHash('sha256').update(base).digest('hex'),
    version: batches.length,
    changes: batches.map((ops, i) => ({
      client: `client-${i}`,
      author: `person-${i}`,
      seq: 1,
      v: i + 1,
      ops,
    })),
  }
}

const rotation = (k: number, was: number, v: number) => ({ b: 'rotations', k, was, v })
const turn = (id: string, k: number, was: number, v: number) => ({
  id,
  puts: [rotation(k, was, v)],
})
const earlier = turn('earlier', 0, 0, 90)
const chosen = turn('chosen', 0, 90, 180)
const later = turn('later', 1, 0, 270)

async function readPages(bytes: Uint8Array) {
  const doc = await PDFDocument.load(bytes, { updateMetadata: false })
  expect(doc.getTitle()).toBe('Box drawing')
  const pages = doc.getPages()
  expect(pages.map((page) => page.getSize())).toEqual([
    { width: 595.28, height: 841.89 },
    { width: 400, height: 300 },
  ])
  const stream = pages[0]!.node.lookup(PDFName.of('Contents'), PDFStream)
  expect(stream.getContents()).toEqual(new TextEncoder().encode(paint))
  return pages.map((page) => page.getRotation().angle)
}

describe('PDF history', () => {
  it('opens the actual page tree, applies confirmed turns relative to the base and preserves drawing bytes', async () => {
    const base = await (await fixture()).save({ useObjectStreams: false })
    expect((await render(base, request(base, []))).bytes).toEqual(base)
    const prefix = await render(base, request(base, [[earlier, chosen]]))
    expect(await readPages(prefix.bytes)).toEqual([90, 0])
    expect(prefix.compensation).toBeUndefined()
    const nextEpoch = await render(
      prefix.bytes,
      request(prefix.bytes, [[turn('on-new-base', 0, 0, 90)]]),
    )
    expect(await readPages(nextEpoch.bytes)).toEqual([180, 0])
  })

  it('restores the pre-op delta and exports compensation that a peer can append at the head', async () => {
    const base = await (await fixture()).save({ useObjectStreams: false })
    const batches = [[earlier, chosen], [later]]
    const undo = await render(base, { ...request(base, batches), undo: target })
    expect(await readPages(undo.bytes)).toEqual([0, 270])
    expect(undo.compensation).toEqual([{ id: expect.any(String), puts: [rotation(0, 180, 90)] }])
    const replay = await render(base, request(base, [...batches, undo.compensation!]))
    expect(replay.bytes).toEqual(undo.bytes)
    expect(await readPages(base)).toEqual([270, 0])
  })

  it('detects a later write even if a second write restores the chosen angle', async () => {
    const base = await (await fixture()).save({ useObjectStreams: false })
    const change = [earlier, chosen, turn('other', 0, 180, 270), turn('restore', 0, 270, 180)]
    await expect(render(base, { ...request(base, [change]), undo: target })).rejects.toMatchObject({
      code: 'conflict',
    })
  })

  it('inverts a whole batch against the actual pre-op values of both pages', async () => {
    const base = await (await fixture()).save({ useObjectStreams: false })
    const batch = {
      id: 'batch',
      puts: [rotation(1, 0, 270), rotation(0, 90, 180), rotation(1, 270, 90)],
    }
    const undone = await render(base, { ...request(base, [[earlier, batch]]), undo: target })
    expect(await readPages(undone.bytes)).toEqual([0, 0])
    const replay = await render(base, request(base, [[earlier, batch], undone.compensation!]))
    expect(replay.bytes).toEqual(undone.bytes)
  })

  it('rejects unsupported buckets, bad turns and targets outside the page tree', async () => {
    const base = await (await fixture()).save({ useObjectStreams: false })
    for (const bad of [
      turn('missing', 2, 0, 90),
      turn('bad', 0, 0, 45),
      { id: 'bad', puts: [{ b: 'rotations', k: 0, v: 90 }] },
    ]) {
      await expect(render(base, request(base, [[bad]]))).rejects.toMatchObject({
        code: 'invalid_history',
      })
    }
    const mixed: Json = {
      id: 'mixed',
      puts: [rotation(0, 0, 90), { b: 'stampCfg', v: null }],
    }
    await expect(render(base, request(base, [[mixed]]))).rejects.toMatchObject({
      code: 'unsupported',
    })
  })

  it('replays metadata against the epoch Info and retains the independent rotation on selective undo', async () => {
    const doc = await fixture(true)
    doc.setAuthor('Original author')
    doc.setSubject('Original subject')
    doc.setKeywords(['original'])
    const base = await doc.save()
    const edit = {
      id: 'meta',
      puts: [{ b: 'metadata', v: { title: '제목', keywords: 'one, two；three' } }],
    }
    const atEdit = await render(base, request(base, [[edit]]))
    const opened = await PDFDocument.load(atEdit.bytes, { updateMetadata: false })
    expect([
      opened.getTitle(),
      opened.getKeywords(),
      opened.getAuthor(),
      opened.getSubject(),
    ]).toEqual(['제목', 'one two three', 'Original author', 'Original subject'])
    expect(opened.getModificationDate()).toEqual(doc.getModificationDate())
    const undo = { ...target, index: 0 }
    const batches = [[edit], [later]]
    const undone = await render(base, { ...request(base, batches), undo })
    expect(await readPages(undone.bytes)).toEqual([270, 270])
    const reopened = await PDFDocument.load(undone.bytes, { updateMetadata: false })
    expect(reopened.getKeywords()).toBe('original')
    expect(undone.compensation).toEqual([
      { id: expect.any(String), puts: [{ b: 'metadata', v: null }] },
    ])
    expect((await render(base, request(base, [...batches, undone.compensation!]))).bytes).toEqual(
      undone.bytes,
    )
    await expect(
      render(base, { ...request(base, [[edit], [{ ...edit, id: 'peer' }]]), undo }),
    ).rejects.toMatchObject({ code: 'conflict' })
  })

  it('preserves standard-font text on its page when adding, rotating and reordering pages', async () => {
    const doc = await PDFDocument.create()
    doc.addPage([300, 400]).drawText('Hello', { x: 20, y: 30, size: 12 })
    const base = await doc.save()
    const add = {
      b: 'blankPages',
      id: 'new',
      v: { id: 'new', input: { pageIndex: 1, width: 200, height: 250 } },
    }
    const order = { b: 'order', was: [0, 1], v: [1, 0] }
    const out = await render(
      base,
      request(base, [[{ id: 'add', puts: [add, order, rotation(0, 0, 90)] }]]),
    )
    const opened = await PDFDocument.load(out.bytes)
    const [blank, original] = opened.getPages()
    expect([blank!.getWidth(), original!.getWidth()]).toEqual([200, 300])
    expect(original!.getRotation().angle).toBe(90)
    const contents = original!.node.lookup(PDFName.of('Contents'), PDFArray)
    const stream = contents.lookup(0, PDFStream)
    expect(Buffer.from(stream.getContents()).toString('ascii')).toContain('<48656C6C6F> Tj')
    expect(original!.node.Resources()!.toString()).toContain('Helvetica')
  })

  it('keeps the surviving text, shared font, Info and peer page order when undoing a turn across a deletion', async () => {
    const doc = await PDFDocument.create()
    doc.setTitle('Three printed pages')
    doc.setAuthor('Printer')
    doc.setSubject('Surviving text')
    const font = await doc.embedFont(StandardFonts.TimesRomanItalic)
    for (const [i, text] of ['First', 'Removed', 'Last'].entries())
      doc.addPage([250 + i, 350]).drawText(text, { font, x: 10, y: 20, size: 12 })
    const base = await doc.save()
    const input = await PDFDocument.load(base, { updateMetadata: false })
    const content = (file: PDFDocument, i: number) =>
      decodePDFRawStream(
        file.getPage(i).node.lookup(PDFName.of('Contents'), PDFArray).lookup(0, PDFRawStream),
      ).decode()
    const batches: Json[][] = [
      [turn('alice', 0, 0, 90)],
      [
        {
          id: 'peer',
          puts: [
            { b: 'deleted', k: 1, v: true },
            { b: 'order', was: [0, 1, 2], v: [2, 1, 0] },
            rotation(2, 0, 180),
          ],
        },
      ],
    ]
    const undo = { ...target, index: 0 }
    const undone = await render(base, { ...request(base, batches), undo })
    const kept = await PDFDocument.load(undone.bytes, { updateMetadata: false })
    expect(kept.getPages().map((page) => [page.getWidth(), page.getRotation().angle])).toEqual([
      [252, 180],
      [250, 0],
    ])
    expect([content(kept, 0), content(kept, 1)]).toEqual([content(input, 2), content(input, 0)])
    expect(kept.getPage(0).node.Resources()!.toString()).toContain('Times-Italic')
    expect(kept.context.lookup(kept.context.trailerInfo.Info)!.toString()).toBe(
      input.context.lookup(input.context.trailerInfo.Info)!.toString(),
    )
    expect(Buffer.from(undone.bytes).toString('latin1')).not.toContain('<52656D6F766564> Tj')
    expect((await render(base, request(base, [...batches, undone.compensation!]))).bytes).toEqual(
      undone.bytes,
    )
    const dependent = [
      [turn('alice', 0, 0, 90)],
      [{ id: 'peer', puts: [{ b: 'deleted', k: 0, v: true }] }],
    ]
    await expect(render(base, { ...request(base, dependent), undo })).rejects.toMatchObject({
      code: 'conflict',
    })
  })

  it('accepts multiple object streams and rejects inconsistent counts, unknown objects and active actions', async () => {
    const doc = await PDFDocument.create()
    for (let i = 0; i < 52; i++) doc.addPage([200 + i, 300])
    const base = await doc.save()
    const out = await render(base, request(base, [[turn('last', 51, 0, 180)]]))
    const opened = await PDFDocument.load(out.bytes)
    expect(opened.getPageCount()).toBe(52)
    expect(opened.getPage(51).getRotation().angle).toBe(180)
    const badCount = Buffer.from(
      Buffer.from(base).toString('latin1').replace('/N 50', '/N 49'),
      'latin1',
    )
    expect(badCount).not.toEqual(Buffer.from(base))
    const other = await fixture()
    other.context.register(other.context.obj({ Extra: 5 }))
    const orphan = await other.save()
    doc.catalog.set(PDFName.of('OpenAction'), doc.context.obj({ S: 'Named' }))
    for (const bytes of [badCount, orphan, await doc.save()]) {
      await expect(render(bytes, request(bytes, []))).rejects.toMatchObject({ code: 'unsupported' })
    }
  })

  it('refuses actual decoded page and object streams over the limits, even in a small file', async () => {
    const doc = await PDFDocument.create()
    const page = doc.addPage()
    const s = doc.context.flateStream(' '.repeat(2049))
    page.node.set(PDFName.of('Contents'), doc.context.register(s))
    const longPage = await doc.save()
    const giantInfo = await PDFDocument.create()
    giantInfo.addPage()
    giantInfo.setTitle('A'.repeat(256 * 1024))
    for (const bytes of [longPage, await giantInfo.save()]) {
      expect(bytes.length).toBeLessThan(32768)
      await expect(render(bytes, request(bytes, []))).rejects.toMatchObject({ code: 'unsupported' })
    }
  })

  it('refuses a file whose xref entries or stream lengths disagree with the bytes', async () => {
    const base = await (await fixture()).save({ useObjectStreams: false })
    const text = Buffer.from(base).toString('latin1')
    for (const changed of [
      text.replace(/\d{10} 00000 n/, '0000000001 00000 n'),
      text.replace(`/Length ${paint.length}`, `/Length ${paint.length - 1}`),
    ]) {
      expect(changed).not.toBe(text)
      const bytes = Buffer.from(changed, 'latin1')
      await expect(render(bytes, request(bytes, []))).rejects.toMatchObject({ code: 'unsupported' })
    }
  })
})
