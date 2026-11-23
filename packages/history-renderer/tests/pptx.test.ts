import { createHash } from 'node:crypto'
import type { HistoryRequest, Json } from '@seuoffice/collab/client'
import '@seuoffice/engine-wasm/node'
import { createBlankPptx, getSlideNotes, openPptx, slideDurableId } from '@seuoffice/pptx-engine'
import { render } from './render'
import JSZip from 'jszip'
import { SaxesParser } from 'saxes'
import { expect, it } from 'vitest'

const P = 'http://schemas.openxmlformats.org/presentationml/2006/main'
const A = 'http://schemas.openxmlformats.org/drawingml/2006/main'
const R = 'http://schemas.openxmlformats.org/officeDocument/2006/relationships'
const a16 = 'http://schemas.microsoft.com/office/drawing/2014/main'
const ns = `xmlns:p="${P}" xmlns:a="${A}" xmlns:a16="${a16}" xmlns:r="${R}"`
const rels = (...targets: [string, string][]) =>
  `<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">${targets.map(([type, target], i) => `<Relationship Id="rId${i + 1}" Type="${R}/${type}" Target="${target}"/>`).join('')}</Relationships>`
const header =
  '<p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/>'
const body = (shapes = '') => `<p:cSld><p:spTree>${header}${shapes}</p:spTree></p:cSld>`
const slidePath = 'ppt/slides/slide7.xml'
const title = '0A1B2C3D',
  subtitle = 'FE9D8C7B'

function shape(id: number, guid: string, text: string) {
  return `<p:sp><p:nvSpPr><p:cNvPr id="${id}" name="Box ${id}"><a:extLst><a:ext uri="{FF2B5EF4-FFF2-40B4-BE49-F238E27FC236}"><a16:creationId id="{${guid}-0000-4000-8000-000000000001}"/></a:ext></a:extLst></p:cNvPr><p:cNvSpPr txBox="1"/><p:nvPr/></p:nvSpPr><p:spPr><a:xfrm><a:off x="0" y="${id * 1000000}"/><a:ext cx="4000000" cy="1000000"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></p:spPr><p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:rPr lang="en-US"/><a:t>${text}</a:t></a:r></a:p></p:txBody></p:sp>`
}

async function deck(alter?: (xml: string) => string) {
  const archive = new JSZip()
  const overrides = [
    ['ppt/presentation.xml', 'presentation.main'],
    [slidePath, 'slide'],
    ['ppt/slideLayouts/slideLayout1.xml', 'slideLayout'],
    ['ppt/slideMasters/slideMaster1.xml', 'slideMaster'],
  ]
  archive.file(
    '[Content_Types].xml',
    `<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/>${overrides.map(([path, type]) => `<Override PartName="/${path}" ContentType="application/vnd.openxmlformats-officedocument.presentationml.${type}+xml"/>`).join('')}</Types>`,
  )
  archive.file('_rels/.rels', rels(['officeDocument', 'ppt/presentation.xml']))
  archive.file(
    'ppt/presentation.xml',
    `<p:presentation ${ns}><p:sldMasterIdLst><p:sldMasterId id="2147483648" r:id="rId1"/></p:sldMasterIdLst><p:sldIdLst><p:sldId id="260" r:id="rId2"/></p:sldIdLst><p:sldSz cx="9144000" cy="6858000"/><p:notesSz cx="6858000" cy="9144000"/></p:presentation>`,
  )
  archive.file(
    'ppt/_rels/presentation.xml.rels',
    rels(['slideMaster', 'slideMasters/slideMaster1.xml'], ['slide', 'slides/slide7.xml']),
  )
  archive.file(
    'ppt/slideMasters/slideMaster1.xml',
    `<p:sldMaster ${ns}>${body()}<p:clrMap bg1="lt1" tx1="dk1" bg2="lt2" tx2="dk2" accent1="accent1" accent2="accent2" accent3="accent3" accent4="accent4" accent5="accent5" accent6="accent6" hlink="hlink" folHlink="folHlink"/><p:sldLayoutIdLst><p:sldLayoutId id="2147483649" r:id="rId1"/></p:sldLayoutIdLst></p:sldMaster>`,
  )
  archive.file(
    'ppt/slideMasters/_rels/slideMaster1.xml.rels',
    rels(['slideLayout', '../slideLayouts/slideLayout1.xml']),
  )
  archive.file(
    'ppt/slideLayouts/slideLayout1.xml',
    `<p:sldLayout ${ns} type="blank">${body()}</p:sldLayout>`,
  )
  archive.file(
    'ppt/slideLayouts/_rels/slideLayout1.xml.rels',
    rels(['slideMaster', '../slideMasters/slideMaster1.xml']),
  )
  archive.file(
    'ppt/slides/_rels/slide7.xml.rels',
    rels(['slideLayout', '../slideLayouts/slideLayout1.xml']),
  )
  const xml = `<p:sld ${ns}>${body(shape(11, title, 'Base &amp; title') + shape(12, subtitle, 'Base subtitle'))}</p:sld>`
  archive.file(slidePath, alter ? alter(xml) : xml)
  return archive.generateAsync({ type: 'uint8array', compression: 'DEFLATE' })
}

const edit = (id: string, el: string, text: string) => ({
  id,
  edit: {
    kind: 'text',
    at: { slide: 's_7', el },
    paragraphs: [{ srcPara: 0, runs: [{ srcRun: 0, text }] }],
  },
})
const target = edit('mine.0', 'e_0a1b2c3d', 'Draft <2> & review')
const peer = edit('peer.0', 'e_12', 'Revised subtitle')

function request(base: Uint8Array, changes: Json[][], undo?: number): HistoryRequest {
  return {
    epoch: 'deck',
    format: { name: 'pptx', schema: 3 },
    baseHash: createHash('sha256').update(base).digest('hex'),
    version: changes.length,
    changes: changes.map((ops, i) => ({
      v: i + 1,
      seq: i,
      client: `client-${i}`,
      author: `person-${i}`,
      ops,
    })),
    ...(undo === undefined ? {} : { undo: { epoch: 'deck', v: undo, index: 0 } }),
  }
}

async function textOf(bytes: Uint8Array) {
  const archive = await JSZip.loadAsync(bytes, { checkCRC32: true })
  const names = Object.keys(archive.files)
    .filter((p) => !archive.files[p]!.dir)
    .sort()
  const words: string[] = []
  const parser = new SaxesParser({ xmlns: true })
  let inText = false
  parser.on('opentag', (tag) => {
    inText = tag.uri === A && tag.local === 't'
  })
  parser.on('text', (text) => {
    if (inText) words.push(text)
  })
  parser.on('closetag', () => {
    inText = false
  })
  parser.write(await archive.file(slidePath)!.async('string')).close()
  return { words, names, archive }
}

it('writes the selected prefix and preserves slide formatting and other parts', async () => {
  const base = await deck()
  const first = await render(base, request(base, [[target]]))
  const head = await render(base, request(base, [[target], [peer]]))
  const saved = await textOf(first.bytes),
    original = await textOf(base)
  expect(saved.words).toEqual(['Draft <2> & review', 'Base subtitle'])
  expect((await textOf(head.bytes)).words).toEqual(['Draft <2> & review', 'Revised subtitle'])
  expect(saved.names).toEqual(original.names)
  for (const path of saved.names) {
    const before = await original.archive.file(path)!.async('string')
    expect(await saved.archive.file(path)!.async('string')).toBe(
      path === slidePath
        ? before.replace('Base &amp; title', 'Draft &lt;2&gt; &amp; review')
        : before,
    )
  }
})

it('appends a regular Slides text compensation and keeps the later peer box', async () => {
  const base = await deck()
  const undone = await render(base, request(base, [[target], [peer]], 1))
  expect((await textOf(undone.bytes)).words).toEqual(['Base & title', 'Revised subtitle'])
  expect(undone.compensation).toEqual([
    {
      id: expect.any(String),
      edit: {
        kind: 'text',
        at: target.edit.at,
        paragraphs: [{ runs: [{ text: 'Base & title' }] }],
      },
    },
  ])
  const appended = await render(base, request(base, [[target], [peer], undone.compensation!]))
  expect((await textOf(appended.bytes)).words).toEqual(['Base & title', 'Revised subtitle'])
})

it('treats a later numeric alias of the same GUID as an overlap, including inside one change', async () => {
  const base = await deck()
  const other = edit('peer.1', 'e_11', 'Peer title')
  await expect(render(base, request(base, [[target, other]], 1))).rejects.toMatchObject({
    code: 'conflict',
  })
})

it('refuses undo of a notes overwrite, unknown targets, and malformed wire records', async () => {
  const base = await deck()
  await expect(
    render(
      base,
      request(base, [[{ id: 'notes', edit: { kind: 'notes', slide: 's_7', text: 'notes' } }]], 1),
    ),
  ).rejects.toMatchObject({ code: 'unsupported' })
  await expect(
    render(base, request(base, [[edit('missing', 'e_99', 'Not here')]])),
  ).rejects.toMatchObject({ code: 'invalid_history' })
  await expect(
    render(base, request(base, [[{ ...target, extra: 'not on the wire' }]])),
  ).rejects.toMatchObject({ code: 'invalid_history' })
})

it('rejects ambiguous identities, flattening edits, and references not in the deck', async () => {
  for (const [alter, code] of [
    [(s: string) => s.replace(subtitle, title), 'invalid_history'],
    [(s: string) => s.replace('</a:r>', '</a:r><a:r><a:t>other run</a:t></a:r>'), 'unsupported'],
    [(s: string) => s.replace(/<a:extLst>.*?<\/a:extLst>/, ''), 'invalid_history'],
    [(s: string) => s.replace(a16, 'urn:not-the-drawing-namespace'), 'unsupported'],
  ] as const) {
    const base = await deck(alter)
    await expect(render(base, request(base, [[target]]))).rejects.toMatchObject({ code })
  }
})

const notes = (id: string, slide: string, text: string) => ({
  id,
  edit: { kind: 'notes', slide, text },
})
const blank = (id: string, slide: string, made: string, before = false) => ({
  id,
  edit: { kind: 'blankSlide', slide, made, before },
})
const remove = (id: string, ...slides: string[]) => ({ id, edit: { kind: 'deleteSlides', slides } })

async function slidesAndNotes(bytes: Uint8Array) {
  const { archive, deck } = await openPptx(bytes)
  return deck.slides.map((slide) => [slideDurableId(slide), getSlideNotes(archive, slide.path)])
}

it('inserts before a source, duplicates without sharing its notes, then deletes the named slides', async () => {
  const base = await createBlankPptx()
  const changes: Json[][] = [
    [notes('n1', 's_1', 'Original\n한글'), blank('add', 's_1', 's_2', true)],
    [{ id: 'dup', edit: { kind: 'duplicateSlide', slide: 's_1', made: 's_3', clearText: false } }],
  ]
  const copy = await render(base, request(base, changes))
  expect(await slidesAndNotes(copy.bytes)).toEqual([
    ['s_2', ''],
    ['s_1', 'Original\n한글'],
    ['s_3', ''],
  ])
  const remaining = await render(
    base,
    request(base, [...changes, [notes('n3', 's_3', 'Peer <&>'), remove('delete', 's_2', 's_1')]]),
  )
  expect(await slidesAndNotes(remaining.bytes)).toEqual([['s_3', 'Peer <&>']])
})

it('uses the engine-assigned part id and refuses undo after a deleted id has been reused', async () => {
  const base = await createBlankPptx()
  await expect(render(base, request(base, [[blank('bad', 's_1', 's_3')]]))).rejects.toMatchObject({
    code: 'invalid_history',
  })
  const changes = [
    [blank('add', 's_1', 's_2')],
    [remove('rm', 's_2'), blank('reuse', 's_1', 's_2')],
  ]
  await expect(render(base, request(base, changes, 1))).rejects.toMatchObject({ code: 'conflict' })
})

it('replays numeric text ids through the engine even before a box has a creation id', async () => {
  const base = await deck((s) => s.replace(/<a:extLst>.*?<\/a:extLst>/, ''))
  const changes = [[edit('type', 'e_11', 'Typed')]]
  const result = await render(base, request(base, changes))
  expect((await textOf(result.bytes)).words).toEqual(['Typed', 'Base subtitle'])
  const again = await render(base, request(base, changes))
  expect(again.bytes).toEqual(result.bytes)
})

it('undoes a duplicate with an ordinary deletion while preserving notes on the source', async () => {
  const base = await deck()
  const duplicate = {
    id: 'copy',
    edit: { kind: 'duplicateSlide', slide: 's_7', made: 's_8', clearText: true },
  }
  const result = await render(
    base,
    request(base, [[duplicate], [notes('peer', 's_7', 'Keep me')]], 1),
  )
  expect(result.compensation).toEqual([
    { id: expect.any(String), edit: { kind: 'deleteSlides', slides: ['s_8'] } },
  ])
  expect(await slidesAndNotes(result.bytes)).toEqual([['s_7', 'Keep me']])
})

it('reopens an in-place, multi-run edit with the peer formatting intact', async () => {
  const base = await deck((s) =>
    s.replace('</a:r>', '</a:r><a:r><a:rPr i="1"/><a:t>extra</a:t></a:r>'),
  )
  const op: Json = {
    id: 'words',
    edit: {
      kind: 'text',
      at: target.edit.at,
      paragraphs: [{ runs: [{ text: 'First', bold: true }, { text: 'Second' }] }],
    },
  }
  const saved = await render(base, request(base, [[op]]))
  const { deck: reopened } = await openPptx(saved.bytes)
  const box = reopened.slides[0]!.elements[0]!
  expect(
    (box.type === 'text' || box.type === 'shape') && box.text?.paragraphs[0]?.runs,
  ).toMatchObject([
    { text: 'First', bold: true },
    { text: 'Second', italic: true },
  ])
})

it('refuses embedded archives even when the outer presentation fits the ZIP limits', async () => {
  const zip = await JSZip.loadAsync(await createBlankPptx())
  zip.file('ppt/embeddings/another.pptx', await createBlankPptx())
  const base = await zip.generateAsync({ type: 'uint8array', compression: 'DEFLATE' })
  await expect(
    render(base, request(base, [[notes('write', 's_1', 'Hello')]])),
  ).rejects.toMatchObject({ code: 'unsupported' })
})
