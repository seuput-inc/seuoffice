// @vitest-environment node
import { readFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import { dirname, join } from 'node:path'
import init, { HwpDocument } from '@rhwp/core'
import JSZip from 'jszip'
import { beforeAll, describe, expect, it } from 'vitest'
import { certifyHwpxSource } from '../src/shared/collab-hwpx-source'
import { applyHwpOp, decodeHwpOp } from '../src/shared/collab-editor-ops'

let source: Uint8Array
const section = 'Contents/section0.xml'
const header = 'Contents/header.xml'
const manifest = 'Contents/content.hpf'

beforeAll(async () => {
  const wasm = join(dirname(createRequire(import.meta.url).resolve('@rhwp/core')), 'rhwp_bg.wasm')
  await init({ module_or_path: readFileSync(wasm) })
  const doc = HwpDocument.createEmpty()
  try {
    doc.createBlankDocument()
    doc.insertText(0, 0, 0, '원본 & 띄어쓰기')
    source = doc.exportHwpx()
  } finally {
    doc.free()
  }
})

async function replace(part: string, before: string, after: string) {
  const zip = await JSZip.loadAsync(source)
  const xml = await zip.file(part)!.async('string')
  expect(xml).toContain(before)
  zip.file(part, xml.replace(before, after), { createFolders: false })
  return zip.generateAsync({ type: 'uint8array', compression: 'DEFLATE' })
}

describe('HWPX source preservation before collaboration', () => {
  it('admits an engine-created document without changing its source', async () => {
    const original = source.slice()
    await certifyHwpxSource(source, HwpDocument)
    expect(source).toEqual(original)
  })

  it('compares expanded element names and decoded text, not their XML spelling', async () => {
    const bytes = await replace(
      section,
      '<hp:t>원본 &amp; 띄어쓰기</hp:t>',
      '<x:t xmlns:x="http://www.hancom.co.kr/hwpml/2011/paragraph">원본 &#38; 띄어쓰기</x:t>',
    )
    await expect(certifyHwpxSource(bytes, HwpDocument)).resolves.toBeUndefined()
  })

  it.each([
    [section, '<hp:p ', '<hp:p data-sample="보관" '],
    [section, '<hp:p ', '<hp:p xmlns:x="urn:example:note" x:mark="보관" '],
    [section, '</hp:run>', '<x:note xmlns:x="urn:example:note">보관</x:note></hp:run>'],
    [section, '</hp:p>', '<x:note xmlns:x="urn:example:note">보관</x:note></hp:p>'],
    [section, '<hp:p ', '<hp:p hp:unmodelled="preserve" '],
    [section, '</hs:sec>', '<hp:t>omitted by the model</hp:t></hs:sec>'],
    [header, '<hh:compatibleDocument ', '<hh:compatibleDocument hh:unmodelled="preserve" '],
    [header, '<hh:paraHead ', '<hh:paraHead unmodelled="preserve" '],
    [
      header,
      '<hh:layoutCompatibility/>',
      '<hh:layoutCompatibility><hh:flag/></hh:layoutCompatibility>',
    ],
    [header, '<hh:numbering id="1" ', '<hh:numbering id="1" restart="1" '],
    [header, '</hh:paraHead>', '<hh:img binaryItemIDRef="image1"/></hh:paraHead>'],
    [header, '<hh:trackchageConfig ', 'loose text<hh:trackchageConfig '],
    [header, '</hh:head>', '<hh:extension/></hh:head>'],
    [
      manifest,
      '<opf:title/>',
      '<opf:title/><opf:creator>not in the manifest vocabulary</opf:creator>',
    ],
    [manifest, '<opf:item ', '<opf:item opf:unmodelled="preserve" '],
    ['settings.xml', '<ha:CaretPosition ', '<ha:CaretPosition ha:unmodelled="preserve" '],
    ['settings.xml', '<ha:CaretPosition ', '<ha:CaretPosition caret="kept" '],
    ['settings.xml', '</ha:HWPApplicationSetting>', '<ha:Unknown/></ha:HWPApplicationSetting>'],
    ['settings.xml', '<ha:CaretPosition ', 'loose text<ha:CaretPosition '],
    ['version.xml', ' appVersion=', ' hv:build="1" appVersion='],
    ['version.xml', ' appVersion=', ' channel="beta" appVersion='],
  ])('refuses unmodelled content in %s (%s)', async (part, before, after) => {
    const bytes = await replace(part, before, after)
    await expect(certifyHwpxSource(bytes, HwpDocument)).rejects.toThrow(part)
  })

  // Hancom Office files whose every part rhwp reconstructs, with Hancom's version, print settings,
  // compatibility options, metadata and numbering or bullet definitions.
  it.each([
    'generated-form-aligned-cells',
    'generated-form-tables',
    'generated-form-tables-hancom',
    'paper-square-table-tac-host',
    'title-table-then-inline-table',
    'title-table-zone-fill',
  ])('admits %s, and a co-edited save keeps every part rhwp copies', async (name) => {
    const hancom = new Uint8Array(readFileSync(new URL(`./fixtures/${name}.hwpx`, import.meta.url)))
    const copied = async (bytes: Uint8Array) => {
      const zip = await JSZip.loadAsync(bytes)
      const part = (n: string) => zip.file(n)!.async('string')
      const head = await part(header)
      return [
        await part('version.xml'),
        await part('settings.xml'),
        /<hh:numberings[\s\S]*?<\/hh:numberings>/.exec(head)?.[0],
        /<hh:bullets[\s\S]*?<\/hh:bullets>/.exec(head)?.[0],
        head.slice(head.indexOf('</hh:refList>')),
        /<opf:metadata[\s\S]*?<\/opf:metadata>/.exec(await part(manifest))?.[0],
      ]
    }
    const before = await copied(hancom)
    expect(before[0]).toContain('application="Hancom Office Hangul"')
    await expect(certifyHwpxSource(hancom, HwpDocument)).resolves.toBeUndefined()
    const doc = new HwpDocument(hancom)
    try {
      doc.insertText(0, 0, 0, '공동')
      // Co-editing may set a paragraph's heading type and level; the definitions stay as they are.
      for (const headType of ['Number', 'Bullet', 'Outline'] as const) {
        const at = { section: 0, paragraph: 0, path: [], offset: 0 }
        const op = { kind: 'command', action: 'paragraph', at, props: { headType, paraLevel: 1 } }
        applyHwpOp(doc, decodeHwpOp(op))
      }
      expect(await copied(doc.exportHwpx())).toEqual(before)
    } finally {
      doc.free()
    }
  })

  it('refuses an image asset the document model will not write back', async () => {
    const zip = await JSZip.loadAsync(source)
    zip.file('BinData/orphan.png', Uint8Array.of(1, 2, 3), { createFolders: false })
    const bytes = await zip.generateAsync({ type: 'uint8array', compression: 'DEFLATE' })
    await expect(certifyHwpxSource(bytes, HwpDocument)).rejects.toThrow('BinData/orphan.png')
  })
})
