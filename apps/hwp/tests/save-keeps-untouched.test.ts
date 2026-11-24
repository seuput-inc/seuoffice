/**
 * A save after typing at the end of the document keeps what the user did not touch: every
 * compound-file stream the engine does not rebuild, the document properties apart from the
 * caret, and in an HWPX the table cell widths, the character shapes and the paragraph ids
 * (vendor/rhwp-core/patches/0058, 0059).
 *
 * `opaque-record.hwp` was written by the container library without `writeContainer`'s
 * repair: its unused sector-table slots read "end of chain", so the engine opens it with its
 * lenient reader, as it does some files Hangul writes. It carries the library's own marker
 * stream (`\x01Sh33tJ5`) next to Hangul's summary, preview, scripts and link streams.
 * `para-float-tail-table.hwpx` was saved by Hangul: its two-column cell is narrower than the
 * table, its default character shape has `strikeout shape="3D"`, and its paragraph ids
 * repeat (0 and 2147483648) as Hangul writes them.
 */
import { readFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import { dirname, join } from 'node:path'
import * as CFB from 'cfb'
import JSZip from 'jszip'
import { beforeAll, describe, expect, it } from 'vitest'
import init, { HwpDocument } from '@rhwp/core'
import {
  DOC_INFO,
  LIST_HEADER,
  openHwp,
  pathsOf,
  recordsOf,
  SECTION,
  streamOf,
} from '../src/main/hwp5-file'
import type { OpenedHwp } from '../src/main/hwp5-file'
import { openHwpFromBytes } from '../src/web/open-hwp'
import { webSaveBytes } from '../src/web/save-hwp'

const require = createRequire(import.meta.url)
const fixture = (name: string) => new Uint8Array(readFileSync(join(__dirname, 'fixtures', name)))
const TYPED = 'SEUEDIT'
/** HWPTAG_DOCUMENT_PROPERTIES: seven u16 counts and start numbers, then the caret (3 × u32) */
const DOCUMENT_PROPERTIES = 0x10
const CARET = 14

beforeAll(async () => {
  const wasm = join(dirname(require.resolve('@rhwp/core')), 'rhwp_bg.wasm')
  await init({ module_or_path: readFileSync(wasm) })
})

/** The file as the web save writes it after `edit`, through the same checks and fixes. */
function saved(
  name: string,
  format: 'hwp' | 'hwpx',
  edit: (doc: HwpDocument) => void,
  bytes = fixture(name),
) {
  const opened = openHwpFromBytes(
    { id: name, fileName: name, bytes, virtualPath: `/web/${name}` },
    true,
  )
  if (!opened) throw new Error('open failed')
  const doc = new HwpDocument(opened.bytes)
  let exported: Uint8Array
  try {
    edit(doc)
    exported = format === 'hwp' ? doc.exportHwp() : doc.exportHwpx()
  } finally {
    doc.free()
  }
  const result = webSaveBytes(opened.base, format, exported, [])
  if (!result.ok) throw new Error(result.error)
  return result.bytes
}

function typeAtEnd(doc: HwpDocument) {
  const para = doc.getParagraphCount(0) - 1
  doc.insertText(0, para, doc.getParagraphLength(0, para), TYPED)
}

const streamBytes = (opened: OpenedHwp, path: string) =>
  Buffer.from((CFB.find(opened.cfb, path)?.content ?? []) as Uint8Array)

function docInfoRecords(opened: OpenedHwp) {
  const data = streamOf(opened, DOC_INFO)
  const records = data && recordsOf(data)
  if (!data || !records) throw new Error('unreadable DocInfo')
  return records.map((r) => ({ tag: r.tag, level: r.level, body: data.subarray(r.body, r.end) }))
}

describe('an HWP save after typing at the end', () => {
  const source = openHwp(fixture('opaque-record.hwp'))!
  let after: OpenedHwp
  beforeAll(() => {
    after = openHwp(saved('opaque-record.hwp', 'hwp', typeAtEnd))!
  })

  it('keeps every storage and stream, by name and place', () => {
    expect(pathsOf(after).sort()).toEqual(pathsOf(source).sort())
  })

  it('keeps every stream the engine does not rebuild byte for byte', () => {
    const rebuilt = ['/DocInfo', '/BodyText/Section0']
    const root = pathsOf(source)[0]!
    const kept = pathsOf(source)
      .filter((path) => !path.endsWith('/'))
      .map((path) => `/${path.slice(root.length)}`)
      .filter((path) => !rebuilt.includes(path))
    expect(kept).toEqual(
      expect.arrayContaining([
        '/\x05HwpSummaryInformation',
        '/PrvImage',
        '/PrvText',
        '/Scripts/DefaultJScript',
        '/Scripts/JScriptVersion',
        '/DocOptions/_LinkDoc',
        '/\x01Sh33tJ5',
      ]),
    )
    for (const path of kept) {
      expect(streamBytes(after, path).equals(streamBytes(source, path)), path).toBe(true)
    }
  })

  it('changes only the caret in the document properties, and no other DocInfo record', () => {
    const was = docInfoRecords(source)
    const is = docInfoRecords(after)
    expect(is.map((r) => [r.tag, r.level])).toEqual(was.map((r) => [r.tag, r.level]))
    const [wasProps, isProps] = [was[0]!, is[0]!]
    expect(isProps.tag).toBe(DOCUMENT_PROPERTIES)
    expect(isProps.body.length).toBe(wasProps.body.length)
    expect(isProps.body.subarray(0, CARET).equals(wasProps.body.subarray(0, CARET))).toBe(true)
    // the caret (list, paragraph, character) is where the typing ended
    const last = 1
    const typedFrom = wasProps.body.readUInt32LE(CARET + 8)
    expect([0, 4, 8].map((at) => isProps.body.readUInt32LE(CARET + at))).toEqual([
      0,
      last,
      typedFrom + TYPED.length,
    ])
    for (let i = 1; i < was.length; i++) {
      expect(is[i]!.body.equals(was[i]!.body), `DocInfo record ${i}`).toBe(true)
    }
  })
})

async function hwpxFacts(bytes: Uint8Array) {
  const zip = await JSZip.loadAsync(bytes)
  const section = await zip.files['Contents/section0.xml']!.async('string')
  const header = await zip.files['Contents/header.xml']!.async('string')
  const all = (xml: string, re: RegExp) => [...xml.matchAll(re)].map((m) => m[1]!)
  return {
    cellWidths: all(section, /<hp:cellSz width="(\d+)"/g),
    strikeouts: all(header, /<hh:strikeout shape="([^"]+)"/g),
    paragraphIds: all(section, /<hp:p id="(\d+)"/g),
  }
}

describe('an HWPX save', () => {
  const name = 'para-float-tail-table.hwpx'

  it('keeps cell widths, character shapes and paragraph ids after typing at the end', async () => {
    const before = await hwpxFacts(fixture(name))
    expect(before.cellWidths).toContain('40535')
    expect(before.strikeouts[0]).toBe('3D')
    expect(await hwpxFacts(saved(name, 'hwpx', typeAtEnd))).toEqual(before)
  })

  it('gives a paragraph made by Enter an id no other paragraph has, keeping the others', async () => {
    const before = await hwpxFacts(fixture(name))
    const after = await hwpxFacts(
      saved(name, 'hwpx', (doc) => {
        const para = doc.getParagraphCount(0) - 1
        doc.splitParagraph(0, para, doc.getParagraphLength(0, para))
      }),
    )
    expect(after.paragraphIds).toHaveLength(before.paragraphIds.length + 1)
    const fresh = after.paragraphIds.at(-1)!
    expect(after.paragraphIds.slice(0, -1)).toEqual(before.paragraphIds)
    expect(before.paragraphIds).not.toContain(fresh)
  })
})

/** The body paragraph whose first control is the document's first table. */
function tableParagraph(doc: HwpDocument) {
  for (let para = 0; para < doc.getParagraphCount(0); para++) {
    try {
      doc.getTableDimensions(0, para, 0)
      return para
    } catch {
      // no table in this paragraph
    }
  }
  throw new Error('no table')
}

/** `para-float-tail-table.hwpx` with its parts changed by `change`, repacked. */
async function changedHwpx(change: (part: string, xml: string) => string) {
  const zip = await JSZip.loadAsync(fixture('para-float-tail-table.hwpx'))
  for (const part of ['Contents/section0.xml', 'Contents/header.xml']) {
    zip.file(part, change(part, await zip.files[part]!.async('string')))
  }
  return zip.generateAsync({ type: 'uint8array', compression: 'DEFLATE' })
}

describe('an edit next to what a save keeps', () => {
  const name = 'para-float-tail-table.hwpx'

  it('saves a row inserted under a widened cell at the width the file stored', async () => {
    // the table cut to its widened cell alone: one column, whose width layout widens
    const oneColumn = await changedHwpx((part, xml) => {
      if (part !== 'Contents/section0.xml') return xml
      const firstRow = xml.indexOf('<hp:tr>', xml.indexOf('<hp:tbl '))
      const cut = xml.slice(0, firstRow) + xml.slice(xml.indexOf('<hp:tr>', firstRow + 1))
      return cut
        .replace(/(<hp:tbl [^>]*)rowCnt="2" colCnt="2"/, '$1rowCnt="1" colCnt="1"')
        .replace(
          '<hp:cellAddr colAddr="0" rowAddr="1"/><hp:cellSpan colSpan="2"',
          '<hp:cellAddr colAddr="0" rowAddr="0"/><hp:cellSpan colSpan="1"',
        )
    })
    expect((await hwpxFacts(oneColumn)).cellWidths).toEqual(['40535'])
    const after = saved(
      name,
      'hwpx',
      (doc) => doc.insertTableRow(0, tableParagraph(doc), 0, 0, true),
      oneColumn,
    )
    expect((await hwpxFacts(after)).cellWidths).toEqual(['40535', '40535'])
  })

  it('keeps a strikeout turned on over the no-strikeout placeholder through an HWP save', async () => {
    const placeholders = await changedHwpx((part, xml) =>
      part === 'Contents/header.xml'
        ? xml.replaceAll('<hh:strikeout shape="NONE"', '<hh:strikeout shape="3D"')
        : xml,
    )
    const hwp = saved(
      name,
      'hwp',
      (doc) => {
        expect(JSON.parse(doc.applyCharFormat(0, 0, 0, 1, '{"strikethrough":true}'))).toEqual({
          ok: true,
        })
      },
      placeholders,
    )
    const back = new HwpDocument(hwp)
    try {
      expect(JSON.parse(back.getCharPropertiesAt(0, 0, 0)).strikethrough).toBe(true)
      expect(JSON.parse(back.getCharPropertiesAt(0, 0, 1)).strikethrough).toBe(false)
    } finally {
      back.free()
    }
  })

  it('gives the paragraphs of a pasted table ids no other paragraph has', async () => {
    const before = await hwpxFacts(fixture(name))
    const after = await hwpxFacts(
      saved(name, 'hwpx', (doc) => {
        doc.copyControl(0, tableParagraph(doc), '[]', 0)
        const para = doc.getParagraphCount(0) - 1
        doc.pasteControl(0, para, doc.getParagraphLength(0, para))
      }),
    )
    const fresh = after.paragraphIds.filter((id) => !before.paragraphIds.includes(id))
    expect(after.paragraphIds.length - fresh.length).toBe(before.paragraphIds.length)
    // the pasted table's three cell paragraphs and the paragraph that holds it
    expect(new Set(fresh).size).toBeGreaterThanOrEqual(4)
    expect(new Set(fresh).size).toBe(fresh.length)
  })
})

/**
 * `widened-cell.hwp` is `para-float-tail-table.hwpx` saved as HWP by the ieum.35 engine. Its
 * two-column cell stores 40535 in LIST_HEADER, narrower than the table, so the engine widens it
 * on open, as it does for the HWPX.
 */
it('keeps the LIST_HEADER of a widened HWP cell through a save after typing at the end', () => {
  const cells = (bytes: Uint8Array) => {
    const opened = openHwp(bytes)
    const section = opened && streamOf(opened, SECTION(0))
    const records = section && recordsOf(section)
    if (!section || !records) throw new Error('unreadable section')
    return records
      .filter((r) => r.tag === LIST_HEADER)
      .map((r) => section.subarray(r.body, r.end).toString('hex'))
  }
  const before = cells(fixture('widened-cell.hwp'))
  expect(before.length).toBeGreaterThanOrEqual(3)
  expect(cells(saved('widened-cell.hwp', 'hwp', typeAtEnd))).toEqual(before)
})
