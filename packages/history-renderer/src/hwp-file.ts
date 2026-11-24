import { inflateRawSync, inflateSync } from 'node:zlib'
import { HistoryError } from '@seuoffice/collab/client'
import { SaxesParser } from 'saxes'
import type { HwpDocument } from '../../../vendor/rhwp-core/rhwp.js'
import { measureTab } from '../../../apps/hwp/studio-plugin/measure-tab.mjs'
import { checkHwpSourceRecords } from '../../../apps/hwp/src/main/collab-source'
import { scanFieldKinds } from '../../../apps/hwp/src/main/field-kinds'
import { openHwp, recordsOf, SIGNATURE } from '../../../apps/hwp/src/main/hwp5-file'
import { applySaveFixes, type SaveBase } from '../../../apps/hwp/src/main/save-fixes'
import { scanSectionTails } from '../../../apps/hwp/src/main/section-tails'
import { newTabsOf } from '../../../apps/hwp/src/main/tab-widths'
import { historyPartLimits, openCheckedArchive } from './archive'

type Format = 'hwp' | 'hwpx'
type Constructor = new (bytes: Uint8Array) => HwpDocument
interface CheckedFile {
  sections: number
  memos: number
  base: SaveBase
}

const maxFile = 2 * 1024 * 1024
const headerSignature = Buffer.alloc(32)
headerSignature.write(SIGNATURE, 'ascii')
function unsupported(reason: string): never {
  throw new HistoryError('unsupported', reason)
}

function checkSize(bytes: Uint8Array) {
  if (
    !ArrayBuffer.isView(bytes) ||
    Object.prototype.toString.call(bytes) !== '[object Uint8Array]' ||
    !bytes.length ||
    bytes.length > maxFile
  )
    unsupported('empty HWP document or outside the 2 MiB history limit')
}

// BinData can override the document's compression. Bound both decoders used by rhwp,
// including its fallback to the stored bytes, before it sees any picture stream.
function binarySize(bytes: Buffer): number {
  let size = bytes.length
  for (const inflate of [inflateRawSync, inflateSync]) {
    try {
      size = Math.max(
        size,
        inflate(bytes, { maxOutputLength: historyPartLimits.maxPartBytes }).length,
      )
    } catch (err) {
      const code: unknown = (err as NodeJS.ErrnoException).code
      // fflate uses numeric codes 0..3 for invalid/truncated deflate and 6 for an invalid zlib header.
      if (
        code !== 'Z_DATA_ERROR' &&
        code !== 'Z_BUF_ERROR' &&
        ![0, 1, 2, 3, 6].some((n) => n === code)
      )
        throw err
    }
  }
  return size
}

function checkHwp(bytes: Uint8Array): CheckedFile {
  // openHwp passes the CFB gate, checks the HWP identity and rejects encryption and ViewText.
  const opened = openHwp(bytes)
  if (!opened) return unsupported('not a verified, unprotected HWP 5 document')
  const { FullPaths: paths, FileIndex: entries } = opened.cfb
  const root = paths[0]!
  const seen = new Set<string>()
  let sections = 0
  let memos = 0
  let declaredSections = 0
  let total = 0
  for (const [i, entry] of entries.entries()) {
    if (entry.type !== 2 && entry.type !== 1) continue
    const path = paths[i]!.slice(root.length)
    const key = path.toLowerCase()
    if (seen.size >= historyPartLimits.maxParts || seen.has(key))
      unsupported('ambiguous HWP directory')
    seen.add(key)
    if (entry.type === 1) {
      if (!['BodyText/', 'BinData/', 'Scripts/', 'DocOptions/'].includes(path))
        unsupported('unverified HWP storage')
      continue
    }
    const raw = Buffer.from(entry.content as Uint8Array)
    let size = raw.length
    if (size > historyPartLimits.maxPartBytes) unsupported('HWP stream exceeds history limit')
    if (path === 'FileHeader') {
      // All other flag bits describe features beyond plain compressed/uncompressed HWP,
      // including tracking even when its ViewText copy is missing, DRM and signatures.
      if (
        size !== 256 ||
        !raw.subarray(0, 32).equals(headerSignature) ||
        raw[35] !== 5 ||
        raw.readUInt32LE(36) & ~1 ||
        raw.readUInt32LE(40) !== 0
      )
        unsupported('protected or unsupported HWP header')
    } else if (path === 'DocInfo' || /^BodyText\/Section(?:0|[1-9]\d*)$/.test(path)) {
      const data = opened.compressed
        ? inflateRawSync(raw, { maxOutputLength: historyPartLimits.maxPartBytes })
        : raw
      size = data.length
      const records = recordsOf(data, 40000)
      if (!records?.length || records.some((r) => r.level > 64))
        return unsupported('invalid or excessive HWP records')
      if (path === 'DocInfo') {
        if (records.some((r) => r.tag === 28 || r.tag === 96 || r.tag === 97))
          unsupported('HWP distribution data or tracked changes')
        const props = records[0]!
        if (props.tag !== 16 || props.end - props.body < 2)
          unsupported('missing HWP document properties')
        declaredSections = data.readUInt16LE(props.body)
      } else {
        sections++
        if (records[0]!.tag !== 66 || records[0]!.level !== 0)
          unsupported('missing HWP section paragraph')
        for (const record of records) if (record.tag === 93) memos++
      }
    } else if (
      /^BinData\/[^/]+$/.test(path) ||
      /^Scripts\/(?:DefaultJScript|JScriptVersion)$/.test(path)
    ) {
      size = binarySize(raw)
    } else if (
      ![
        'PrvText',
        'PrvImage',
        '\x05HwpSummaryInformation',
        'DocOptions/_LinkDoc',
        'RhwpHwpxOrigin',
      ].includes(path)
    ) {
      unsupported('unverified HWP stream')
    }
    if ((total += size) > historyPartLimits.maxTotalBytes)
      unsupported('expanded HWP exceeds history limit')
  }
  if (!seen.has('fileheader') || !sections || declaredSections !== sections)
    unsupported('incomplete HWP sections')
  for (let i = 0; i < sections; i++) {
    if (!seen.has(`bodytext/section${i}`)) unsupported('missing HWP section')
  }
  checkHwpSourceRecords(opened)
  return {
    sections,
    memos,
    base: { tails: scanSectionTails(opened) ?? undefined, kinds: scanFieldKinds(opened) },
  }
}

const hml = 'http://www.hancom.co.kr/hwpml/2011/'
const opf = 'http://www.idpf.org/2007/opf/'
const containerNamespace = 'urn:oasis:names:tc:opendocument:xmlns:container'
const contentNamespaces = [
  hml + 'head',
  hml + 'section',
  hml + 'core',
  hml + 'paragraph',
  'http://www.hancom.co.kr/hwpml/2016/paragraph',
]
const roots = new Map([
  ['Contents/content.hpf', [opf, 'package']],
  ['Contents/header.xml', [hml + 'head', 'head']],
  ['META-INF/container.xml', [containerNamespace, 'container']],
  ['META-INF/manifest.xml', ['urn:oasis:names:tc:opendocument:xmlns:manifest:1.0', 'manifest']],
])

async function checkHwpx(bytes: Uint8Array): Promise<CheckedFile> {
  const zip = await openCheckedArchive(bytes, [
    'mimetype',
    ...roots.keys(),
    'Contents/section0.xml',
  ])
  const mime = await zip.file('mimetype')?.async('string')
  if (mime !== 'application/hwp+zip' && mime !== 'application/haansofthwpx')
    unsupported('not a HWPX package')
  for (const name of roots.keys()) if (!zip.file(name)) unsupported('missing HWPX package part')
  let packageRoots = 0
  const sections = new Set<string>()
  let declaredSections = 0
  const items = new Map<string, string>()
  const spine: string[] = []
  let memos = 0
  let elements = 0
  for (const part of Object.values(zip.files)) {
    if (part.dir) continue
    const section = /^Contents\/section(?:0|[1-9]\d*)\.xml$/.test(part.name)
    const master = /^Contents\/masterpage(?:0|[1-9]\d*)\.xml$/.test(part.name)
    if (
      !section &&
      !master &&
      !roots.has(part.name) &&
      !/^BinData\/[^/]+$/.test(part.name) &&
      ![
        'mimetype',
        'version.xml',
        'settings.xml',
        'META-INF/container.rdf',
        'META-INF/rhwp-hwp5-origin',
        'Preview/PrvText.txt',
        'Preview/PrvImage.png',
      ].includes(part.name)
    )
      unsupported('unverified HWPX package part')
    if (!/\.(?:xml|hpf|rdf)$/i.test(part.name)) continue
    const expected = section ? [hml + 'section', 'sec'] : roots.get(part.name)
    if (section) sections.add(part.name)
    const xml = new TextDecoder('utf-8', { fatal: true }).decode(await part.async('uint8array'))
    const parser = new SaxesParser({ xmlns: true })
    let depth = 0
    parser.on('doctype', () => unsupported('unsupported HWPX document type declaration'))
    parser.on('xmldecl', ({ encoding }) => {
      if (encoding && !/^utf-8$/i.test(encoding)) unsupported('unsupported HWPX XML encoding')
    })
    parser.on('opentag', (tag) => {
      if (++depth > 64 || ++elements > 40000) unsupported('excessive HWPX XML structure')
      if (section || part.name === 'Contents/header.xml') {
        if (
          !contentNamespaces.includes(tag.uri) ||
          Object.values(tag.attributes).some(
            (a) =>
              a.uri &&
              a.prefix !== 'xmlns' &&
              a.name !== 'xmlns' &&
              a.prefix !== 'xml' &&
              !contentNamespaces.includes(a.uri),
          )
        )
          unsupported('unverified HWPX extension markup')
      }
      if (depth === 1 && expected && (tag.uri !== expected[0] || tag.local !== expected[1]))
        unsupported('wrong HWPX document part')
      const attr = (key: string) => tag.attributes[key]?.value
      if (
        part.name === 'META-INF/container.xml' &&
        tag.local === 'rootfile' &&
        tag.uri === containerNamespace &&
        attr('media-type') === 'application/hwpml-package+xml'
      ) {
        if (attr('full-path') !== 'Contents/content.hpf') unsupported('ambiguous HWPX root')
        packageRoots++
      }
      if (depth === 1 && part.name === 'Contents/header.xml')
        declaredSections = Number(attr('secCnt'))
      if (
        /^(?:encryption-data|signature|trackChange|trackChangeAuthor|(?:insert|delete)(?:Begin|End))$/i.test(
          tag.local,
        ) ||
        (/^trackcha(?:n)?geConfig$/i.test(tag.local) && attr('flags') !== '56')
      )
        unsupported('protected or tracked HWPX content')
      if (tag.local === 'fieldBegin' && attr('type') === 'MEMO') memos++
      if (part.name !== 'Contents/content.hpf' || tag.uri !== opf) return
      if (tag.local === 'item') {
        const id = attr('id')
        const path = attr('href')
        if (!id || !path || items.has(id) || !zip.file(path))
          return unsupported('unverified HWPX manifest item')
        items.set(id, path)
      } else if (tag.local === 'itemref') spine.push(attr('idref') ?? '')
    })
    parser.on('closetag', () => {
      depth--
    })
    parser.write(xml).close()
  }
  if (packageRoots !== 1) unsupported('missing HWPX package root')
  const listed = new Set(items.values())
  for (const name of Object.keys(zip.files)) {
    if (
      !name.endsWith('/') &&
      /^(?:BinData\/|Contents\/masterpage)/.test(name) &&
      !listed.has(name)
    )
      unsupported('unreferenced HWPX content')
  }
  const body = spine.map((id) => items.get(id)).filter((path) => path !== 'Contents/header.xml')
  if (
    !sections.size ||
    sections.size !== declaredSections ||
    sections.size !== body.length ||
    body.some((path, i) => path !== `Contents/section${i}.xml` || !sections.has(path))
  )
    unsupported('unverified HWPX section order')
  return { sections: sections.size, memos, base: {} }
}

async function check(format: Format, bytes: Uint8Array): Promise<CheckedFile> {
  checkSize(bytes)
  if (format === 'hwp') return checkHwp(bytes)
  if (format === 'hwpx') return checkHwpx(bytes)
  return unsupported('unsupported HWP source format')
}

// Runtime memo ids and page positions are assigned on load. Compare the persisted identity,
// note and anchor instead. A readonly rich memo cannot be verified by its plain text.
function memoSnapshot(doc: HwpDocument): string[] {
  const list = JSON.parse(doc.getMemoList())
  if (!Array.isArray(list)) return unsupported('unverified HWP memos')
  return list
    .map((memo) => {
      if (memo.editable !== true || memo.readOnlyReason !== null)
        unsupported('unverified HWP memo content')
      return JSON.stringify([
        memo.index,
        memo.author,
        memo.date,
        memo.text,
        memo.place,
        memo.sectionIndex,
        memo.paraIndex,
        memo.startCharIdx,
        memo.endCharIdx,
        memo.path,
      ])
    })
    .sort()
}

function verifyBase(before: SaveBase, after: SaveBase) {
  for (const [key, kind] of before.kinds ?? []) {
    if (after.kinds?.has(key) && after.kinds.get(key) !== kind)
      unsupported('HWP field kind was lost')
  }
  if (!before.tails) return
  for (const [section, lists] of before.tails.tails) {
    const keys = (after.tails?.tails.get(section) ?? []).map((tail) => tail.key)
    for (const tail of lists) {
      if (tail.memo !== undefined) continue // Deleting a memo deliberately removes its tail.
      const index = keys.indexOf(tail.key)
      if (index < 0) unsupported('HWP master page was lost')
      keys.splice(index, 1)
    }
  }
  for (const key of before.tails.captions.keys())
    if (!after.tails?.captions.has(key)) unsupported('HWP equation caption was lost')
}

async function produce(
  format: Format,
  doc: HwpDocument,
  base: SaveBase,
  Document: Constructor,
): Promise<Uint8Array> {
  const memos = memoSnapshot(doc)
  const sections = doc.getSectionCount()
  const pages = doc.pageCount()
  const exported = format === 'hwp' ? doc.exportHwpWithReport() : doc.exportHwpxWithReport()
  let raw: Uint8Array
  try {
    const report = JSON.parse(exported.contentLoss())
    if (
      report.schemaVersion !== 1 ||
      report.outputFormat !== format ||
      report.count !== 0 ||
      !Array.isArray(report.losses) ||
      report.losses.length
    )
      unsupported('HWP serializer reported content loss')
    const bytes = exported.takeBytes()
    checkSize(bytes)
    // Studio owns the bytes; JSZip requires a Uint8Array view from this realm.
    raw = new Uint8Array(bytes.buffer, bytes.byteOffset, bytes.byteLength)
  } finally {
    exported.free()
  }
  const checked = await check(format, raw)
  const tabs = newTabsOf(raw, format).map((spot) => {
    const measure = measureTab(doc, spot)
    if (!measure) return unsupported('cannot verify HWP tab width')
    return { ...spot, ...measure }
  })
  const fixed = applySaveFixes(raw, format, tabs, base)
  if (fixed.tailsSkipped) unsupported('cannot restore HWP section tails')
  const saved = fixed.bytes === raw ? checked : await check(format, fixed.bytes)
  if (tabs.length && newTabsOf(fixed.bytes, format).length) unsupported('HWP tab width was lost')
  verifyBase(base, saved.base)
  if (sections !== saved.sections || memos.length !== saved.memos)
    unsupported('incomplete HWP export')
  // The desktop's exportHwpVerify compares page counts on a second export. Verify the actual
  // fixed bytes instead, and apply the read-back check to both formats.
  const back = new Document(fixed.bytes)
  try {
    const restored = memoSnapshot(back)
    if (
      back.getSourceFormat() !== format ||
      back.getSectionCount() !== sections ||
      back.pageCount() !== pages ||
      restored.length !== memos.length ||
      restored.some((memo, i) => memo !== memos[i])
    )
      unsupported('HWP export did not reopen intact')
  } finally {
    back.free()
  }
  return fixed.bytes
}

/** The caller owns doc.free() and must not mutate the document during produce(). */
export async function openHwpFile(
  format: Format,
  bytes: Uint8Array,
  Document: Constructor,
): Promise<{
  readonly bytes: Uint8Array
  readonly format: Format
  readonly doc: HwpDocument
  produce(live?: HwpDocument): Promise<Uint8Array>
}> {
  checkSize(bytes)
  // ZIP verification yields. A caller cannot replace already verified bytes before construction.
  const source = new Uint8Array(bytes)
  const original = await check(format, source)
  const doc = new Document(source)
  try {
    if (
      doc.getSourceFormat() !== format ||
      doc.getSectionCount() !== original.sections ||
      memoSnapshot(doc).length !== original.memos
    )
      unsupported('incomplete HWP document model')
    return {
      format,
      bytes: source,
      doc,
      produce: (live = doc) => produce(format, live, original.base, Document),
    }
  } catch (err) {
    doc.free()
    throw err
  }
}
