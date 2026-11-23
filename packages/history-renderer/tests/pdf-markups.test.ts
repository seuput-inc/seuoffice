import { createHash } from 'node:crypto'
import type { HistoryRequest, Json } from '@seuoffice/collab/client'
import {
  PDFArray,
  PDFDict,
  PDFDocument,
  PDFHexString,
  PDFName,
  PDFNumber,
  PDFStream,
} from 'pdf-lib'
import { describe, expect, it } from 'vitest'
import { render } from './render'

const name = PDFName.of
const quad = [10, 30, 30, 30, 10, 20, 30, 20]
const other = [40, 60, 50, 60, 40, 50, 50, 50]
const rgb = [0.2, 0.4, 1]
const epoch = 'markup-history'

async function baseFile() {
  const doc = await PDFDocument.create({ updateMetadata: false })
  doc.addPage([200, 300])
  doc.addPage([300, 200])
  return doc.save({ useObjectStreams: false })
}

const item = (id: string, type: string, pageIndex: number) => ({
  id,
  type,
  pageIndex,
  color: rgb,
  quads: [quad],
})
const put = (id: string, v: Json) => ({ b: 'markups', id, v })
const op = (id: string, ...puts: Json[]) => ({ id, puts })

function request(base: Uint8Array, changes: Json[][], undoVersion?: number): HistoryRequest {
  return {
    epoch,
    format: { name: 'pdf', schema: 2 },
    version: changes.length,
    baseHash: createHash('sha256').update(base).digest('hex'),
    changes: changes.map((ops, i) => ({ v: i + 1, client: `c${i}`, seq: 1, author: 'Alice', ops })),
    ...(undoVersion === undefined ? {} : { undo: { epoch, v: undoVersion, index: 0 } }),
  }
}

function readPage(doc: PDFDocument, index: number) {
  const page = doc.getPage(index)
  const refs = page.node.lookupMaybe(name('Annots'), PDFArray)
  if (!refs) return []
  return Array.from({ length: refs.size() }, (_, i) => {
    const d = refs.lookup(i, PDFDict)
    expect(d.lookup(name('Type'), PDFName).decodeText()).toBe('Annot')
    expect(d.get(name('P'))).toBe(page.ref)
    expect(d.lookup(name('F'), PDFNumber).asNumber()).toBe(4)
    expect(d.lookup(name('T'), PDFHexString).decodeText()).toBe('SeuOffice')
    for (const absent of ['Contents', 'CA', 'M', 'CreationDate'])
      expect(d.has(name(absent))).toBe(false)
    const ap = d.lookup(name('AP'), PDFDict).lookup(name('N'), PDFStream)
    expect(ap.dict.lookup(name('Type'), PDFName).decodeText()).toBe('XObject')
    expect(ap.dict.lookup(name('Subtype'), PDFName).decodeText()).toBe('Form')
    const numbers = (dict: PDFDict, key: string) =>
      dict
        .lookup(name(key), PDFArray)
        .asArray()
        .map((n) => Number(n.toString()))
    const rect = numbers(d, 'Rect')
    expect(numbers(ap.dict, 'BBox')).toEqual(rect)
    return {
      type: d.lookup(name('Subtype'), PDFName).decodeText(),
      subject: d.lookupMaybe(name('Subj'), PDFHexString)?.decodeText(),
      rect,
      quadPoints: numbers(d, 'QuadPoints'),
      color: numbers(d, 'C'),
      paint: Buffer.from(ap.getContents()).toString('ascii'),
      resources: ap.dict.lookup(name('Resources'), PDFDict),
    }
  })
}

describe('unstored PDF markups', () => {
  it('writes all four appearances against final rotation and keeps their page references across a fork deletion', async () => {
    const base = await baseFile()
    const edit = op(
      'markup',
      put('hi', { ...item('hi', 'highlight', 0), quads: [quad, other], subject: '  검토  ' }),
      put('line', item('line', 'underline', 0)),
      put('strike', item('strike', 'strikeout', 1)),
      put('wave', item('wave', 'squiggly', 1)),
      { b: 'rotations', k: 1, was: 0, v: 90 },
      { b: 'order', was: [0, 1], v: [1, 0] },
    )
    const prefix = await render(base, request(base, [[edit]]))
    expect((await render(base, request(base, [[edit]]))).bytes).toEqual(prefix.bytes)
    const opened = await PDFDocument.load(prefix.bytes)
    const [hi, line] = readPage(opened, 1)
    expect(hi).toMatchObject({
      type: 'Highlight',
      subject: '검토',
      rect: [10, 20, 50, 60],
      quadPoints: [...quad, ...other],
      color: rgb,
      paint: '/GsM gs\n0.2 0.4 1 rg\n10 20 20 10 re f\n40 50 10 10 re f',
    })
    const gs = hi!.resources.lookup(name('ExtGState'), PDFDict).lookup(name('GsM'), PDFDict)
    expect(gs.lookup(name('BM'), PDFName).decodeText()).toBe('Multiply')
    expect(gs.lookup(name('ca'), PDFNumber).asNumber()).toBe(1)
    expect(line).toMatchObject({
      type: 'Underline',
      subject: undefined,
      paint: '0.2 0.4 1 RG\n0.8 w\n10 20.8 m 30 20.8 l S',
    })
    const [strike, wave] = readPage(opened, 0)
    expect(strike).toMatchObject({
      type: 'StrikeOut',
      paint: '0.2 0.4 1 RG\n1.2 w\n20.8 20 m 20.8 30 l S',
    })
    expect(wave).toMatchObject({
      type: 'Squiggly',
      quadPoints: quad,
      color: rgb,
      rect: [10, 20, 30, 30],
    })
    expect(wave!.paint.split('\n').slice(0, 4)).toEqual([
      '0.2 0.4 1 RG',
      '1.2 w',
      '26.9 20 m',
      '24.9 20.67 24.9 21.33 26.9 22 c',
    ])
    expect(wave!.paint.match(/ c\n/g)).toHaveLength(5)
    expect(wave!.paint.endsWith('26.9 30 c\nS')).toBe(true)
    const drop = op('remove-page', { b: 'deleted', k: 1, v: true })
    const fork = await render(prefix.bytes, { ...request(prefix.bytes, [[drop]]), epoch: 'fork' })
    const kept = await PDFDocument.load(fork.bytes)
    expect(kept.getPageCount()).toBe(1)
    expect(readPage(kept, 0).map((m) => [m.type, m.paint])).toEqual([
      ['StrikeOut', strike!.paint],
      ['Squiggly', wave!.paint],
    ])
  })

  it('restores an edited pending ID without replacing peer annotations, and replays the compensation bytes', async () => {
    const base = await baseFile()
    const old = { ...item('id', 'highlight', 0), subject: 'First' }
    const edited = { ...old, type: 'underline', color: [0, 1, 0], subject: 'Second' }
    const peer = { ...item('peer', 'squiggly', 0), quads: [other] }
    const add = op('add', put('id', old))
    const change = op('edit', put('id', edited))
    const batches = [[add], [change], [op('peer-add', put('peer', peer))]]
    const undo = await render(base, request(base, batches, 2))
    const [restored, kept] = readPage(await PDFDocument.load(undo.bytes), 0)
    expect(restored).toMatchObject({
      type: 'Highlight',
      subject: 'First',
      color: rgb,
      quadPoints: quad,
    })
    expect(kept).toMatchObject({ type: 'Squiggly', quadPoints: other })
    expect(undo.compensation).toEqual([op(expect.any(String), put('id', old))])
    const replay = await render(base, request(base, [...batches, undo.compensation!]))
    expect(replay.bytes).toEqual(undo.bytes)

    const remove = op('drop', put('id', null))
    const absent = await render(base, request(base, [[add], [remove]]))
    expect(readPage(await PDFDocument.load(absent.bytes), 0)).toEqual([])
    await expect(render(base, request(base, [[add], [remove]], 2))).rejects.toMatchObject({
      code: 'unsupported',
    })
    const onlyPeer = await render(base, request(base, [[add], batches[2]!], 1))
    expect(readPage(await PDFDocument.load(onlyPeer.bytes), 0).map((m) => m.type)).toEqual([
      'Squiggly',
    ])
    for (const conflict of [
      [remove, op('recreated', put('id', edited))],
      [op('hide', { b: 'deleted', k: 0, v: true })],
    ]) {
      await expect(
        render(base, request(base, [[add], [change], conflict], 2)),
      ).rejects.toMatchObject({
        code: 'conflict',
      })
    }
  })

  it('refuses invalid wire fields and valid wire geometry beyond the bounded appearance profile', async () => {
    const base = await baseFile()
    const mark = item('id', 'highlight', 0)
    for (const bad of [
      { ...mark, contents: 'not a wire field' },
      { ...mark, color: [2, 1, 0] },
      { ...mark, type: 'text' },
    ]) {
      const output = render(base, request(base, [[op('bad', put('id', bad))]]))
      await expect(output).rejects.toMatchObject({ code: 'invalid_history' })
    }
    for (const bad of [
      { ...mark, color: [1e-7, 0, 1] },
      { ...mark, quads: Array.from({ length: 17 }, () => quad) },
      { ...mark, quads: [[10, 30, 10, 30, 10, 20, 10, 20]] },
      { ...mark, quads: [[10, 30, 14401, 30, 10, 20, 14401, 20]] },
      { ...mark, type: 'squiggly', quads: [[10, 30, 210, 30, 10, 20, 210, 20]] },
    ]) {
      const output = render(base, request(base, [[op('too-large', put('id', bad))]]))
      await expect(output).rejects.toMatchObject({ code: 'unsupported' })
    }
  })
})
