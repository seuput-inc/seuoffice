import { createHash } from 'node:crypto'
import { readFile } from 'node:fs/promises'
import { join } from 'node:path'
import type { HistoryRequest, Json } from '@seuoffice/collab/client'
import '@seuoffice/engine-wasm/node'
import { getSlideNotes, openPptx, slideDurableId } from '@seuoffice/pptx-engine'
import JSZip from 'jszip'
import { SaxesParser } from 'saxes'
import { describe, expect, it } from 'vitest'
import { render } from './render'

const fixtures = process.env.GENERATED_FIXTURES
const printer = 'ppt/printerSettings/printerSettings1.bin'
const thumb = 'docProps/thumbnail.jpeg'
const presRels = 'ppt/_rels/presentation.xml.rels'
const rootRels = '_rels/.rels'
const types = '[Content_Types].xml'
const printerType = 'application/vnd.openxmlformats-officedocument.presentationml.printerSettings'
const printerRel =
  'http://schemas.openxmlformats.org/officeDocument/2006/relationships/printerSettings'
const thumbRel = 'http://schemas.openxmlformats.org/package/2006/relationships/metadata/thumbnail'
const peer = { id: 'peer', edit: { kind: 'notes', slide: 's_2', text: 'Keep these notes' } }

function request(bytes: Uint8Array, ops: Json[], undo = false): HistoryRequest {
  return {
    epoch: 'deck',
    format: { name: 'pptx', schema: 3 },
    version: ops.length,
    baseHash: createHash('sha256').update(bytes).digest('hex'),
    changes: ops.map((op, i) => ({
      v: i + 1,
      seq: i,
      client: `c${i}`,
      author: `a${i}`,
      ops: [op],
    })),
    ...(undo ? { undo: { epoch: 'deck', v: 1, index: 0 } } : {}),
  }
}

async function attributes(zip: JSZip, path: string, tag: string) {
  const found: Record<string, string>[] = []
  const sax = new SaxesParser({ xmlns: false })
  sax.on('opentag', (node) => {
    if (node.name === tag) found.push(node.attributes)
  })
  sax.write(await zip.file(path)!.async('string')).close()
  return found
}

// python-pptx 1.0.2: Presentation(), add_slide(slide_layouts[6]) twice, save('plain.pptx').
describe.skipIf(!fixtures)('python-pptx default presentation', () => {
  it.each(['blankSlide', 'duplicateSlide'] as const)(
    'preserves opaque metadata across %s and its undo',
    async (kind) => {
      const base = await readFile(join(fixtures!, 'plain.pptx'))
      const original = await JSZip.loadAsync(base)
      const edit: Record<string, Json> = { kind, slide: 's_1', made: 's_3' }
      edit[kind === 'blankSlide' ? 'before' : 'clearText'] = false
      const create = { id: 'mine', edit }
      for (const undo of [false, true]) {
        const saved = await render(base, request(base, [create, peer], undo))
        if (undo)
          expect(saved.compensation).toEqual([
            { id: expect.any(String), edit: { kind: 'deleteSlides', slides: ['s_3'] } },
          ])
        const { archive, deck } = await openPptx(saved.bytes)
        expect(deck.slides.map((s) => [slideDurableId(s), getSlideNotes(archive, s.path)])).toEqual(
          [['s_1', ''], ...(undo ? [] : [['s_3', '']]), ['s_2', 'Keep these notes']],
        )
        const zip = await JSZip.loadAsync(saved.bytes, { checkCRC32: true })
        for (const path of [printer, thumb]) {
          expect(await zip.file(path)!.async('uint8array')).toEqual(
            await original.file(path)!.async('uint8array'),
          )
        }
        for (const [path, Type, Target] of [
          [presRels, printerRel, printer.slice(4)],
          [rootRels, thumbRel, thumb],
        ] as const) {
          const match = (await attributes(original, path, 'Relationship')).filter(
            (r) => r.Type === Type,
          )
          expect(match).toEqual([expect.objectContaining({ Type, Target })])
          expect(
            (await attributes(zip, path, 'Relationship')).filter((r) => r.Type === Type),
          ).toEqual(match)
        }
        expect(await attributes(zip, types, 'Default')).toEqual(
          expect.arrayContaining([
            { Extension: 'bin', ContentType: printerType },
            { Extension: 'jpeg', ContentType: 'image/jpeg' },
          ]),
        )
      }
    },
  )

  it.each([
    [presRels, /<Relationship [^>]*Target="printerSettings\/printerSettings1.bin"\/>/, ''],
    [rootRels, /<Relationship [^>]*Target="docProps\/thumbnail.jpeg"\/>/, ''],
    [types, printerType, 'application/vnd.ms-office.vbaProject'],
    [types, 'image/jpeg', 'image/svg+xml'],
    [
      presRels,
      printerRel,
      'http://schemas.openxmlformats.org/officeDocument/2006/relationships/oleObject',
    ],
    [presRels, 'Target="printerSettings/', 'TargetMode="External" Target="printerSettings/'],
    [printer, /[\s\S]+/, 'not the stock printer settings'],
  ] as const)('refuses altered metadata in %s (%s)', async (path, from, to) => {
    const zip = await JSZip.loadAsync(await readFile(join(fixtures!, 'plain.pptx')))
    const before = await zip.file(path)!.async('string')
    const after = before.replace(from, to)
    expect(after).not.toBe(before)
    zip.file(path, after)
    const bytes = await zip.generateAsync({ type: 'uint8array', compression: 'DEFLATE' })
    await expect(render(bytes, request(bytes, [peer]))).rejects.toMatchObject({
      code: 'unsupported',
    })
  })
})
