/**
 * Records and markup the engine does not model survive an edit elsewhere and the save
 * (vendor/rhwp-core/patches/0047).
 *
 * `opaque-record.hwp` is the editor's `blank.hwp` with two paragraphs written by the engine
 * and one record the engine does not know (tag 0x3fe, level 1, payload
 * `vendor-opaque-metadata`) appended to the first paragraph.
 */
import { readFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import { dirname, join } from 'node:path'
import JSZip from 'jszip'
import { JSDOM } from 'jsdom'
import { beforeAll, expect, it } from 'vitest'
import init, { HwpDocument } from '@rhwp/core'
import { openHwp, PARA_HEADER, recordsOf, SECTION, streamOf } from '../src/main/hwp5-file'
import { openHwpFromBytes } from '../src/web/open-hwp'
import { webSaveBytes } from '../src/web/save-hwp'

const require = createRequire(import.meta.url)
const FIXTURE = join(__dirname, 'fixtures', 'opaque-record.hwp')
const BLANK = join(__dirname, '..', 'src', 'renderer', 'assets', 'blank.hwp')
const SECTION_XML = 'Contents/section0.xml'
const OPAQUE = 0x3fe
const PAYLOAD = Buffer.from('vendor-opaque-metadata')

beforeAll(async () => {
  const wasm = join(dirname(require.resolve('@rhwp/core')), 'rhwp_bg.wasm')
  await init({ module_or_path: readFileSync(wasm) })
})

/** The opaque records of each body paragraph, by paragraph. */
function opaqueByParagraph(file: Uint8Array): Buffer[][] {
  const opened = openHwp(file)
  const section = opened && streamOf(opened, SECTION(0))
  const records = section && recordsOf(section)
  if (!section || !records) throw new Error('unreadable section')
  const out: Buffer[][] = []
  for (const record of records) {
    if (record.tag === PARA_HEADER && record.level === 0) out.push([])
    else if (record.tag === OPAQUE) out.at(-1)?.push(section.subarray(record.body, record.end))
  }
  return out
}

it('keeps a record of an untouched paragraph through an edit and the web save', () => {
  const bytes = new Uint8Array(readFileSync(FIXTURE))
  expect(opaqueByParagraph(bytes)).toEqual([[PAYLOAD], []])
  const opened = openHwpFromBytes(
    { id: 'opaque', fileName: 'opaque-record.hwp', bytes, virtualPath: '/web/opaque-record.hwp' },
    true,
  )
  if (!opened) throw new Error('open failed')

  const doc = new HwpDocument(opened.bytes)
  let exported: Uint8Array
  try {
    doc.insertText(0, 1, 0, '가')
    exported = doc.exportHwp()
  } finally {
    doc.free()
  }
  const saved = webSaveBytes(opened.base, 'hwp', exported, [])

  if (!saved.ok) throw new Error(saved.error)
  expect(saved.tailsLost).toBe(false)
  expect(opaqueByParagraph(saved.bytes)).toEqual([[PAYLOAD], []])
})

/** Binds `ext` to `uri` on the paragraph holding `text`, with an attribute and a run child. */
function addExtension(xml: string, text: string, uri: string, attr: string, child: string) {
  const t = `<hp:t>${text}</hp:t>`
  const at = xml.indexOf(t)
  const p = xml.lastIndexOf('<hp:p ', at) + '<hp:p'.length
  const withChild = `${xml.slice(0, at + t.length)}<ext:${child}/>${xml.slice(at + t.length)}`
  return `${withChild.slice(0, p)} xmlns:ext="${uri}" ext:${attr}="1"${withChild.slice(p)}`
}

/** Body paragraphs "front" and "back", saved as HWPX, and their section XML. */
async function twoParagraphsHwpx(): Promise<{ zip: JSZip; xml: string }> {
  const doc = new HwpDocument(new Uint8Array(readFileSync(BLANK)))
  let source: Uint8Array
  try {
    doc.insertText(0, 0, 0, 'front')
    doc.splitParagraph(0, 0, 5)
    doc.insertText(0, 1, 0, 'back')
    source = doc.exportHwpx()
  } finally {
    doc.free()
  }
  const zip = await JSZip.loadAsync(source)
  return { zip, xml: await zip.files[SECTION_XML].async('string') }
}

it('merging paragraphs that bind one prefix to different namespaces keeps every name', async () => {
  const fixture = await twoParagraphsHwpx()
  const zip = fixture.zip
  let xml = fixture.xml
  xml = addExtension(xml, 'front', 'urn:a', 'mark', 'noteA')
  xml = addExtension(xml, 'back', 'urn:b', 'flag', 'noteB')
  zip.file(SECTION_XML, xml)

  const edited = new HwpDocument(await zip.generateAsync({ type: 'uint8array' }))
  let saved: Uint8Array
  try {
    edited.mergeParagraph(0, 1)
    saved = edited.exportHwpx()
  } finally {
    edited.free()
  }
  const out = await (await JSZip.loadAsync(saved)).files[SECTION_XML].async('string')
  const parsed = new new JSDOM().window.DOMParser().parseFromString(out, 'application/xml')

  expect(parsed.getElementsByTagName('parsererror')).toHaveLength(0)
  const noteA = parsed.getElementsByTagNameNS('urn:a', 'noteA')
  const noteB = parsed.getElementsByTagNameNS('urn:b', 'noteB')
  expect([noteA.length, noteB.length]).toEqual([1, 1])
  const p = noteA[0].closest('p')
  expect(p?.contains(noteB[0])).toBe(true)
  expect([p?.getAttributeNS('urn:a', 'mark'), p?.getAttributeNS('urn:b', 'flag')]).toEqual([
    '1',
    '1',
  ])
})

it('keeps text inside foreign elements bound on the run or by default out of the paragraph', async () => {
  const fixture = await twoParagraphsHwpx()
  const zip = fixture.zip
  let xml = fixture.xml
  const t = '<hp:t>front</hp:t>'
  const at = xml.indexOf(t) + t.length
  const run = xml.lastIndexOf('<hp:run', at) + '<hp:run'.length
  const foreign =
    '<x:note><hp:t>leak</hp:t></x:note><note xmlns="urn:vendor2"><hp:t>leak2</hp:t></note>'
  xml = `${xml.slice(0, run)} xmlns:x="urn:vendor"${xml.slice(run, at)}${foreign}${xml.slice(at)}`
  zip.file(SECTION_XML, xml)

  const edited = new HwpDocument(await zip.generateAsync({ type: 'uint8array' }))
  let saved: Uint8Array
  try {
    expect(edited.getTextRange(0, 0, 0, edited.getParagraphLength(0, 0))).toBe('front')
    saved = edited.exportHwpx()
  } finally {
    edited.free()
  }
  const reopened = new HwpDocument(saved)
  try {
    expect(reopened.getTextRange(0, 0, 0, reopened.getParagraphLength(0, 0))).toBe('front')
  } finally {
    reopened.free()
  }
  const out = new new JSDOM().window.DOMParser().parseFromString(
    await (await JSZip.loadAsync(saved)).files[SECTION_XML].async('string'),
    'application/xml',
  )
  expect(out.getElementsByTagName('parsererror')).toHaveLength(0)
  expect(out.getElementsByTagNameNS('urn:vendor', 'note')).toHaveLength(1)
  expect(out.getElementsByTagNameNS('urn:vendor2', 'note')).toHaveLength(1)
})
