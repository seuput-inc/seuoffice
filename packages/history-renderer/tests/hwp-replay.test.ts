import { createHash } from 'node:crypto'
import { gunzipSync } from 'node:zlib'
import type { HistoryRequest, Json } from '@seuoffice/collab/client'
import { renderHistory } from '@seuoffice/history-renderer'
import JSZip from 'jszip'
import { SaxesParser } from 'saxes'
import { beforeAll, describe, expect, it } from 'vitest'
import {
  openHwp,
  recordsOf,
  rewriteStreams,
  SECTION,
  streamOf,
} from '../../../apps/hwp/src/main/hwp5-file'
import { HwpDocument, initSync } from '../../../vendor/rhwp-core/rhwp.js'
import { render } from './render'

type Format = 'hwp' | 'hwpx'
const base = new Map<Format, Uint8Array>()
const section = 'Contents/section0.xml'
const header = 'Contents/header.xml'
let table: Uint8Array
beforeAll(async () => {
  // Load the engine from the installed renderer, including when the tests run outside this checkout.
  const url = new URL('rhwp-engine.js', import.meta.resolve('@seuoffice/history-renderer'))
  const { default: compressed } = await import(url.href)
  initSync({ module: gunzipSync(Buffer.from(compressed, 'base64')) })
  const doc = HwpDocument.createEmpty()
  try {
    doc.createBlankDocument()
    doc.splitParagraph(0, 0, doc.textToLogicalOffset(0, 0, 0))
    doc.insertText(0, 1, 0, '한🙂글')
    base.set('hwp', doc.exportHwp())
    base.set('hwpx', doc.exportHwpx())
    expect(JSON.parse(doc.createTable(0, 0, doc.textToLogicalOffset(0, 0, 0), 1, 1)).ok).toBe(true)
    table = doc.exportHwpx()
  } finally {
    doc.free()
  }
})

function splice(from: number, to: number, text: string, paragraph = 1): Json {
  const position = (offset: number) => ({ section: 0, paragraph, path: [], offset })
  return { kind: 'splice', from: position(from), to: position(to), text }
}

function request(name: Format, bytes: Uint8Array, ...ops: Json[]): HistoryRequest {
  return {
    format: { name, schema: 1 },
    epoch: 'hangul',
    baseHash: createHash('sha256').update(bytes).digest('hex'),
    version: ops.length,
    changes: ops.map((op, i) => ({ v: i + 1, seq: i + 1, client: 'a', author: 'A', ops: [op] })),
  }
}

function expectText(bytes: Uint8Array, name: Format, ...paragraphs: string[]) {
  expect(bytes.length).toBeLessThanOrEqual(2 * 1024 * 1024)
  const reopened = new HwpDocument(bytes)
  try {
    expect(reopened.getSourceFormat()).toBe(name)
    expect(reopened.getParagraphCount(0)).toBe(1 + paragraphs.length)
    for (const [i, text] of paragraphs.entries()) {
      const length = reopened.getParagraphLength(0, i + 1)
      expect(reopened.getTextRange(0, i + 1, 0, length)).toBe(text)
    }
  } finally {
    reopened.free()
  }
}

async function replaceXml(zip: JSZip, part: string, before: string, after: string) {
  const xml = await zip.file(part)!.async('string')
  expect(xml).toContain(before)
  zip.file(part, xml.replace(before, after), { createFolders: false })
}

async function expectLayout(bytes: Uint8Array) {
  const zip = await JSZip.loadAsync(bytes)
  const found: string[][] = []
  for (const part of [header, section]) {
    const parser = new SaxesParser({ xmlns: true })
    const parents: string[] = []
    parser.on('opentag', (tag) => {
      const attr = (key: string) => tag.attributes[key]?.value
      if (
        tag.uri === 'http://www.hancom.co.kr/hwpml/2011/head' &&
        tag.local === 'paraPr' &&
        attr('textDir')
      )
        found.push([part, parents.at(-1)!, tag.local, attr('id')!, attr('textDir')!])
      if (tag.uri === 'http://www.hancom.co.kr/hwpml/2011/paragraph') {
        if (tag.local === 'grid')
          found.push([part, parents.at(-1)!, tag.local, attr('strikeContinue')!])
        if (tag.local === 'label')
          found.push([part, parents.at(-1)!, tag.local, attr('topmargin')!, attr('labelcols')!])
      }
      parents.push(tag.local)
    })
    parser.on('closetag', () => {
      parents.pop()
    })
    parser.write(await zip.file(part)!.async('string')).close()
  }
  expect(found).toEqual([
    [header, 'paraProperties', 'paraPr', '0', 'LTR'],
    [section, 'secPr', 'grid', '1'],
    [section, 'tbl', 'label', '3826', '2'],
  ])
}

const formats = ['hwp', 'hwpx'] as const
describe.each(formats)('%s retained history in the Deno sandbox', (name) => {
  it('returns the exact admitted source for version zero and reopens edits at scalar offsets', async () => {
    const source = base.get(name)!
    const zero = await render(source, request(name, source))
    expect(zero.fileName).toBe(`history.${name}`)
    expect(Buffer.compare(zero.bytes, source)).toBe(0)
    expectText(zero.bytes, name, '한🙂글')
    const edited = await render(
      source,
      request(name, source, splice(1, 2, '동료'), splice(4, 4, '!')),
    )
    expect(edited.fileName).toBe(`history.${name}`)
    expectText(edited.bytes, name, '한동료글!')
  }, 40_000)

  it('stages the edited file through the mixed history API', async () => {
    const source = base.get(name)!
    const { changes, ...header } = request(name, source, splice(0, 1, '새'))
    const chunks: Uint8Array[] = []
    const file = await renderHistory(
      {
        byteLength: source.length,
        async *open() {
          yield source
        },
      },
      Buffer.from(JSON.stringify(header)),
      (async function* () {
        yield Buffer.from(JSON.stringify(changes[0]))
      })(),
      {
        async write(bytes) {
          chunks.push(bytes.slice())
        },
        async abort() {
          chunks.length = 0
        },
      },
    )
    expect(file.fileName).toBe(`history.${name}`)
    expectText(Buffer.concat(chunks), name, '새🙂글')
  }, 20_000)

  it('reopens certified compensation without losing later text in the new paragraph', async () => {
    const source = base.get(name)!
    const ops = [splice(3, 3, '\n원🙂본'), splice(1, 2, 'X', 2), splice(0, 0, '앞', 2)]
    const prefix = request(name, source, ...ops)
    const undone = await render(source, {
      ...prefix,
      undo: { epoch: prefix.epoch, v: 2, index: 0 },
    })
    expect(undone.compensation).toEqual([splice(2, 3, '🙂', 2)])
    expectText(undone.bytes, name, '한🙂글', '앞원🙂본')
    const appended = await render(source, request(name, source, ...ops, ...undone.compensation!))
    expectText(appended.bytes, name, '한🙂글', '앞원🙂본')
  }, 40_000)

  it('rejects unsupported source content even at version zero', async () => {
    let unsupported: Uint8Array
    if (name === 'hwp') {
      const doc = new HwpDocument(base.get(name)!)
      try {
        unsupported = doc.exportHwpWithPassword('test-only')
      } finally {
        doc.free()
      }
    } else {
      const zip = await JSZip.loadAsync(base.get(name)!)
      zip.file('Contents/vendor.xml', '<authored/>', { createFolders: false })
      unsupported = await zip.generateAsync({ type: 'uint8array', compression: 'DEFLATE' })
    }
    await expect(render(unsupported, request(name, unsupported))).rejects.toMatchObject({
      code: 'unsupported',
    })
  }, 20_000)
})

it('refuses an unmodelled paragraph record before returning the source for version zero', async () => {
  const opened = openHwp(base.get('hwp')!)!
  const path = SECTION(0)
  const data = streamOf(opened, path)!
  const { end } = recordsOf(data)!.find((r) => r.tag === 67 && r.level === 1)!
  const record = Buffer.alloc(5)
  record.writeUInt32LE((1 << 20) | (1 << 10) | 0x3fe)
  record[4] = 42
  const body = Buffer.concat([data.subarray(0, end), record, data.subarray(end)])
  const source = rewriteStreams(opened, new Map([[path, body]]))!
  await expect(render(source, request('hwp', source))).rejects.toMatchObject({
    code: 'unsupported',
  })
}, 20_000)

it('retains HWPX layout fields after unrelated body edits and replay of their compensation', async () => {
  const zip = await JSZip.loadAsync(table)
  await replaceXml(zip, header, '<hh:paraPr ', '<hh:paraPr textDir="LTR" ')
  await replaceXml(zip, section, '<hp:grid ', '<hp:grid strikeContinue="1" ')
  await replaceXml(zip, section, '</hp:tbl>', '<hp:label topmargin="3826" labelcols="2"/></hp:tbl>')
  const source = await zip.generateAsync({ type: 'uint8array', compression: 'DEFLATE' })
  const zero = await render(source, request('hwpx', source))
  expect(Buffer.compare(zero.bytes, source)).toBe(0)
  const ops = [splice(0, 1, '새', 2), splice(3, 3, '!', 2)]
  const prefix = request('hwpx', source, ...ops)
  const edited = await render(source, prefix)
  expectText(edited.bytes, 'hwpx', '', '새🙂글!')
  await expectLayout(edited.bytes)
  const undone = await render(source, { ...prefix, undo: { epoch: prefix.epoch, v: 1, index: 0 } })
  expect(undone.compensation).toEqual([splice(0, 1, '한', 2)])
  const appended = await render(source, request('hwpx', source, ...ops, ...undone.compensation!))
  for (const result of [undone, appended]) {
    expectText(result.bytes, 'hwpx', '', '한🙂글!')
    await expectLayout(result.bytes)
  }
}, 60_000)

it.each([
  ['<hp:p ', '<hp:p data-sample="보관" '],
  ['<hp:p ', '<hp:p xmlns:x="urn:example:note" x:mark="보관" '],
  ['</hp:p>', '<x:note xmlns:x="urn:example:note">보관</x:note></hp:p>'],
  ['</hp:run>', '<x:note xmlns:x="urn:example:note">보관</x:note></hp:run>'],
  ['</hp:tbl>', '<hp:label data-sample="보관"/></hp:tbl>'],
])(
  'refuses opaque HWPX markup at admission (%s, %s)',
  async (before, after) => {
    const zip = await JSZip.loadAsync(table)
    await replaceXml(zip, section, before, after)
    const source = await zip.generateAsync({ type: 'uint8array', compression: 'DEFLATE' })
    await expect(render(source, request('hwpx', source))).rejects.toMatchObject({
      code: 'unsupported',
    })
  },
  20_000,
)

it('does not dispatch unknown wire operations or pretend unsupported undo succeeded', async () => {
  const source = base.get('hwp')!
  const unknown = request('hwp', source, splice(0, 0, '먼저'), { kind: 'exportHwp', args: [] })
  await expect(render(source, unknown)).rejects.toMatchObject({ code: 'invalid_history' })
  const split = request('hwp', source, splice(1, 1, '\n'))
  split.undo = { epoch: split.epoch, v: 1, index: 0 }
  await expect(render(source, split)).rejects.toMatchObject({ code: 'unsupported' })
}, 40_000)
