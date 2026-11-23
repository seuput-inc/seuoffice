import { createHash } from 'node:crypto'
import type { HistoryRequest, Json } from '@seuoffice/collab/client'
import { parseDocx } from '@seuoffice/docx-engine'
import { buildDocx } from '../../docx-engine/tests/helpers/build-docx'
import { render } from './render'
import JSZip from 'jszip'
import { SaxesParser } from 'saxes'
import { describe, expect, it } from 'vitest'

const word = 'http://schemas.openxmlformats.org/wordprocessingml/2006/main'
const opc = 'http://schemas.openxmlformats.org/package/2006/'
const office = 'http://schemas.openxmlformats.org/officeDocument/2006/relationships/'
const props = `<?xml version="1.0"?><cp:coreProperties xmlns:cp="${opc}metadata/core-properties"/>`
const untouched = '<w:p>\n<w:r><w:t>un</w:t></w:r> <w:r><w:t>touched</w:t></w:r>\n</w:p>'
const body =
  '<w:p><w:r><w:t>A&#x1F642;Z</w:t></w:r></w:p><w:p><w:r><w:t>tail</w:t></w:r></w:p>' + untouched
const section = '<w:sectPr><w:pgSz w:w="12240" w:h="15840"/></w:sectPr>'

async function file(content = body, target = 'word/document.xml'): Promise<Uint8Array> {
  const zip = new JSZip()
  zip.file(
    '[Content_Types].xml',
    `<Types xmlns="${opc}content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/><Override PartName="/docProps/core.xml" ContentType="application/vnd.openxmlformats-package.core-properties+xml"/></Types>`,
  )
  zip.file(
    '_rels/.rels',
    `<Relationships xmlns="${opc}relationships"><Relationship Id="main" Type="${office}officeDocument" Target="${target}"/><Relationship Id="meta" Type="${opc}relationships/metadata/core-properties" Target="docProps/core.xml"/></Relationships>`,
  )
  zip.file(
    'word/document.xml',
    `<w:document xmlns:w="${word}"><w:body>${content}${section}</w:body></w:document>`,
  )
  zip.file('docProps/core.xml', props)
  return zip.generateAsync({ type: 'uint8array', compression: 'DEFLATE' })
}

async function reopen(bytes: Uint8Array) {
  const zip = await JSZip.loadAsync(bytes)
  const xml = await zip.file('word/document.xml')!.async('string')
  const paragraphs: string[] = []
  const parser = new SaxesParser({ xmlns: true })
  let text = false
  parser.on('opentag', (tag) => {
    if (tag.uri === word && tag.local === 'p') paragraphs.push('')
    text = tag.uri === word && tag.local === 't'
  })
  parser.on('text', (s) => {
    if (text) paragraphs[paragraphs.length - 1] += s
  })
  parser.on('closetag', () => {
    text = false
  })
  parser.write(xml).close()
  return { paragraphs, xml, zip }
}

function replace(from: number, to: number, text?: string): Json {
  const step = { stepType: 'replace', from, to }
  return {
    kind: 'steps',
    steps: [text === undefined ? step : { ...step, slice: { content: [{ type: 'text', text }] } }],
  }
}

function request(
  base: Uint8Array,
  changes: Json[][],
  undo?: HistoryRequest['undo'],
): HistoryRequest {
  return {
    baseHash: createHash('sha256').update(base).digest('hex'),
    epoch: 'doc',
    format: { name: 'docx', schema: 4 },
    version: changes.length,
    changes: changes.map((ops, i) => ({
      v: i + 1,
      author: `person-${i}`,
      client: `client-${i}`,
      seq: 1,
      ops,
    })),
    ...(undo ? { undo } : {}),
  }
}
const undo = { epoch: 'doc', v: 1, index: 0 }

describe('DOCX schema 4 body text', () => {
  it('refuses the wrong base, version gaps, duplicate batches and a target outside the retained epoch', async () => {
    const base = await file()
    const req = request(base, [[replace(2, 4, 'x')], [replace(1, 1, '!')]])
    const [a, b] = req.changes
    for (const bad of [
      { ...req, baseHash: '0'.repeat(64) },
      { ...req, version: 3 },
      { ...req, changes: [a!, { ...b!, v: 3 }] },
      { ...req, changes: [a!, { ...b!, client: a!.client, seq: a!.seq }] },
      { ...req, undo: { ...undo, epoch: 'another-epoch' } },
    ])
      await expect(render(base, bad)).rejects.toMatchObject({ code: 'invalid_history' })
  })

  it('renders exactly the requested prefix using UTF-16 positions and keeps untouched parts', async () => {
    const base = await file()
    const zero = await render(base, request(base, []))
    expect(zero.bytes).toEqual(base)
    const out = await render(base, request(base, [[replace(2, 4, '<&')]]))
    expect(out.fileName).toMatch(/\.docx$/)
    expect(out.compensation).toBeUndefined()
    const doc = await reopen(out.bytes)
    expect(doc.paragraphs).toEqual(['A<&Z', 'tail', 'untouched'])
    expect(doc.xml).toContain(untouched)
    expect(doc.xml).toContain(section)
    const original = await JSZip.loadAsync(base)
    for (const name of ['[Content_Types].xml', '_rels/.rels', 'docProps/core.xml']) {
      expect(await doc.zip.file(name)!.async('uint8array')).toEqual(
        await original.file(name)!.async('uint8array'),
      )
    }
  })

  it('uses the Docs space rule per run and preserves a newly typed leading space', async () => {
    const base = await file(
      '<w:p><w:r><w:t>  A  </w:t></w:r><w:r><w:t xml:space="preserve"> B </w:t></w:r></w:p>',
    )
    const out = await render(base, request(base, [[replace(1, 1, ' '), replace(6, 6, '!')]]))
    expect((await reopen(out.bytes)).paragraphs).toEqual([' A B !'])
  })

  it('remaps a replacement over later typing in the same batch and replays its wire compensation', async () => {
    const base = await file()
    const changes = [[replace(2, 4, '<&'), replace(11, 11, '!')], [replace(1, 1, '++')]]
    const out = await render(base, request(base, changes, undo))
    expect(out.compensation).toEqual([replace(4, 6, '🙂')])
    expect((await reopen(out.bytes)).paragraphs).toEqual(['++A🙂Z', 'tail!', 'untouched'])
    const peer = await render(base, request(base, [...changes, out.compensation!]))
    expect((await reopen(peer.bytes)).paragraphs).toEqual(['++A🙂Z', 'tail!', 'untouched'])
  })

  it('restores a deletion and keeps later typing on both boundaries of an insertion', async () => {
    const base = await file()
    const deleted = await render(base, request(base, [[replace(2, 4), replace(1, 1, 'x')]], undo))
    expect(deleted.compensation).toEqual([replace(3, 3, '🙂')])
    expect((await reopen(deleted.bytes)).paragraphs[0]).toBe('xA🙂Z')

    const inserted = await render(
      base,
      request(base, [[replace(2, 2, 'xy'), replace(2, 2, '['), replace(5, 5, ']')]], undo),
    )
    expect(inserted.compensation).toEqual([replace(3, 5)])
    expect((await reopen(inserted.bytes)).paragraphs[0]).toBe('A[]🙂Z')
  })

  it('refuses an interior edit and an edit at the gap left by a deletion', async () => {
    const base = await file()
    for (const ops of [
      [replace(2, 4, 'xy'), replace(3, 3, '!')],
      [replace(2, 4), replace(2, 2, '!')],
    ]) {
      await expect(render(base, request(base, [ops], undo))).rejects.toMatchObject({
        code: 'conflict',
      })
    }
  })

  it('keeps paragraph properties, run styles and package parts through a compound edit and its compensation', async () => {
    const paragraph = '<w:p w:rsidR="00000001"><w:pPr><w:jc w:val="center"/></w:pPr>'
    const base = await buildDocx({
      bodyXml:
        paragraph +
        '<w:r><w:rPr><w:b/><w:color w:val="884422"/></w:rPr><w:t>AB</w:t></w:r>' +
        '<w:r><w:t>CD</w:t></w:r></w:p><w:p><w:r><w:t>peer</w:t></w:r></w:p>',
      withNumbering: true,
      headerXml: '<w:p><w:r><w:t>Unchanged header</w:t></w:r></w:p>',
      footerXml: '<w:p><w:r><w:t>Unchanged footer</w:t></w:r></w:p>',
    })
    const compound: Json = {
      kind: 'steps',
      steps: [
        { stepType: 'replace', from: 2, to: 3, slice: { content: [{ type: 'text', text: 'x' }] } },
        { stepType: 'addMark', from: 2, to: 3, mark: { type: 'italic' } },
      ],
    }
    const alice = await render(base, request(base, [[compound]]))
    expect((await parseDocx(alice.bytes)).blocks[0]!.runs).toMatchObject([
      { text: 'A', bold: true, color: '884422' },
      { text: 'x', italic: true },
      { text: 'CD' },
    ])
    const original = await JSZip.loadAsync(base)
    const edited = await reopen(alice.bytes)
    expect(edited.xml).toContain(paragraph)
    for (const entry of Object.values(original.files)) {
      if (!entry.dir && entry.name !== 'word/document.xml') {
        expect(await edited.zip.file(entry.name)!.async('uint8array')).toEqual(
          await entry.async('uint8array'),
        )
      }
    }
    const changes = [[compound], [replace(7, 7, '!')]]
    const compensated = await render(base, request(base, changes, undo))
    expect(compensated.compensation).toHaveLength(1)
    for (const bytes of [
      compensated.bytes,
      (await render(base, request(base, [...changes, compensated.compensation!]))).bytes,
    ]) {
      const [first, second] = (await parseDocx(bytes)).blocks
      expect(first!.runs).toMatchObject([
        { text: 'AB', bold: true, color: '884422' },
        { text: 'CD' },
      ])
      expect(second!.runs?.map((run) => run.text).join('')).toBe('!peer')
    }
    await expect(
      render(base, request(base, [[compound], [replace(3, 3, 'X')]], undo)),
    ).rejects.toMatchObject({ code: 'conflict' })
  })

  it('accepts marked text but does not feed picture numbering to the engine', async () => {
    const op: Json = {
      kind: 'steps',
      steps: [
        {
          stepType: 'replace',
          from: 2,
          to: 2,
          slice: {
            content: [
              {
                type: 'text',
                text: 'red',
                marks: [{ type: 'docTextStyle', attrs: { color: 'FF0000', sizeHalfPoints: 24 } }],
              },
              { type: 'text', text: 'solid', marks: [{ type: 'bold' }] },
            ],
          },
        },
      ],
    }
    const bodyXml = '<w:p><w:r><w:t>AB</w:t></w:r></w:p>'
    const base = await buildDocx({ bodyXml })
    const out = await render(base, request(base, [[op]]))
    expect((await parseDocx(out.bytes)).blocks[0]!.runs).toMatchObject([
      { text: 'A' },
      { text: 'red', color: 'FF0000', sizeHalfPoints: 24 },
      { text: 'solid', bold: true },
      { text: 'B' },
    ])
    const picture = await buildDocx({
      bodyXml,
      numberingXml: `<w:numbering xmlns:w="${word}"><w:numPicBullet w:numPicBulletId="1"><w:pict/></w:numPicBullet></w:numbering>`,
    })
    await expect(render(picture, request(picture, [[op]]))).rejects.toMatchObject({
      code: 'unsupported',
    })
  })

  it('rejects a mark inverse that would clear formatting already present in half the range', async () => {
    const base = await file(
      '<w:p><w:r><w:rPr><w:b/></w:rPr><w:t>A</w:t></w:r><w:r><w:t>B</w:t></w:r></w:p>',
    )
    await expect(
      render(
        base,
        request(
          base,
          [
            [
              {
                kind: 'steps',
                steps: [{ stepType: 'addMark', from: 1, to: 3, mark: { type: 'bold' } }],
              },
            ],
          ],
          undo,
        ),
      ),
    ).rejects.toMatchObject({ code: 'unsupported' })
  })

  it('does not guess editor positions for structural, unknown marked or side edits', async () => {
    const base = await file()
    const unsupported: Json[] = [
      { kind: 'side', key: 'section', value: {} },
      { kind: 'steps', steps: [{ stepType: 'addMark', from: 1, to: 2, mark: { type: 'link' } }] },
      {
        kind: 'steps',
        steps: [
          {
            stepType: 'replace',
            from: 1,
            to: 1,
            slice: {
              content: [
                {
                  type: 'text',
                  text: 'X',
                  marks: [
                    { type: 'docTextStyle', attrs: { rawRPr: '<w:rPr><w:vanish/></w:rPr>' } },
                  ],
                },
              ],
            },
          },
        ],
      },
      { kind: 'steps', extra: 1, steps: [{ stepType: 'replace', from: 1, to: 2 }] },
      { kind: 'steps', steps: [{ stepType: 'replace', extra: 1, from: 1, to: 2 }] },
      {
        kind: 'steps',
        steps: [
          {
            stepType: 'replace',
            from: 1,
            to: 2,
            slice: { extra: 1, content: [{ type: 'text', text: 'X' }] },
          },
        ],
      },
      replace(5, 7),
      replace(3, 3, 'split-surrogate'),
    ]
    for (const op of unsupported) {
      await expect(render(base, request(base, [[op]]))).rejects.toMatchObject({
        code: 'unsupported',
      })
    }
    const invalid: Json[] = [
      replace(42, 42, 'X'),
      replace(-1, 1),
      { kind: 'steps', steps: [null] },
      { kind: 'steps', steps: [{ stepType: 'replace', structure: '', from: 1, to: 2 }] },
      {
        kind: 'steps',
        steps: [
          {
            stepType: 'replace',
            from: 1,
            to: 2,
            slice: { openStart: false, content: [{ type: 'text', text: 'X' }] },
          },
        ],
      },
    ]
    for (const op of invalid) {
      await expect(render(base, request(base, [[op]]))).rejects.toMatchObject({
        code: 'invalid_history',
      })
    }
  })

  it('rejects bodies with non-paragraph blocks or an unexpected package main relationship', async () => {
    for (const base of [await file('<w:tbl/>' + body), await file(body, 'elsewhere.xml')]) {
      await expect(render(base, request(base, [[replace(1, 1, 'X')]]))).rejects.toMatchObject({
        code: 'unsupported',
      })
    }
  })
})
