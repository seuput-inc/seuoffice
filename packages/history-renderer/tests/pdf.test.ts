import { createHash } from 'node:crypto'
import type { HistoryRequest, Json } from '@seuoffice/collab/client'
import { render } from './render'
import { degrees, PDFDocument, PDFName, PDFStream } from 'pdf-lib'
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
      puts: [rotation(0, 0, 90), { b: 'metadata', v: { title: 'Later' } }],
    }
    await expect(render(base, request(base, [[mixed]]))).rejects.toMatchObject({
      code: 'unsupported',
    })
  })

  it('refuses object streams, filtered page streams and objects not covered by the allowed graph', async () => {
    const doc = await fixture()
    const compressed = await doc.save()
    const other = await fixture()
    other.context.register(other.context.obj({ Extra: 5 }))
    const orphan = await other.save({ useObjectStreams: false })
    const filtered = await (await fixture(true)).save({ useObjectStreams: false })
    for (const bytes of [compressed, filtered, orphan]) {
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
