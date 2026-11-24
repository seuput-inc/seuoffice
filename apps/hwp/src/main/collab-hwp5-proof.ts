import { createHash } from 'node:crypto'
import { inflateRawSync, inflateSync } from 'node:zlib'
import type { HwpDocument } from '@rhwp/core'
import { restoreFieldKinds, scanFieldKinds } from './field-kinds'
import { openHwp, recordsOf, SIGNATURE, type HwpRecord } from './hwp5-file'
import { scanSectionTails } from './section-tails'

const maxFile = 2 * 1024 * 1024
const maxPart = 1024 * 1024
const maxTotal = 4 * maxPart
const maxParts = 256
const section = /^BodyText\/Section(0|[1-9]\d*)$/
const image = /^BinData\/BIN[0-9A-F]{4}\.(?:png|jpe?g|gif|bmp)$/i
const storages = ['BodyText/', 'BinData/', 'Scripts/', 'DocOptions/']
const staticStreams = [
  'FileHeader',
  'PrvText',
  'PrvImage',
  '\x05HwpSummaryInformation',
  'DocOptions/_LinkDoc',
]
const scripts: Record<string, string> = {
  'Scripts/DefaultJScript': '00000000000000000000000000000000ffffffff',
  'Scripts/JScriptVersion': '0100000000000000',
}
const signature = Buffer.alloc(32)
signature.write(SIGNATURE, 'ascii')

function refuse(path: string, reason: string): never {
  throw new Error(`HWP source preservation: ${path}: ${reason}`)
}

function docInfo(data: Buffer, records: HwpRecord[], assets: Set<string>): number {
  const props = records[0]!
  if (props.tag !== 16 || props.end - props.body !== 26 || records[1]?.tag !== 17)
    return refuse('DocInfo', 'missing document properties or ID mappings')
  const extras = new Set<number>()
  for (const { tag, level, body, end } of records) {
    const value = data.subarray(body, end)
    if (tag >= 16 && tag <= 26 && level === (tag <= 17 ? 0 : 1)) {
      if (tag !== 18) continue
      if (value.length < 6 || (value.readUInt16LE() & 0xf) !== 1)
        refuse('DocInfo', 'linked or nested binary object')
      const ext = value.toString('utf16le', 6)
      const path = `BinData/BIN${value.readUInt16LE(2).toString(16).toUpperCase().padStart(4, '0')}.${ext}`
      if (!image.test(path) || value.length !== 6 + 2 * value.readUInt16LE(4) || assets.has(path))
        refuse('DocInfo', 'unsupported or ambiguous embedded image')
      assets.add(path)
      continue
    }
    // These four records are copied whole by the DocInfo writer. Only the bundled blank's
    // defaults have an independent witness; a successful copy of a custom payload proves nothing.
    const zeroBytes = tag === 94 ? 16 : tag === 30 ? 4 : tag === 31 ? 20 : -1
    const known =
      level === (tag === 30 ? 0 : 1) &&
      (tag === 32
        ? value.length === 1032 &&
          createHash('sha256').update(value).digest('hex') ===
            '246701e2b58df835707eaf9fe99e0e79fbe7352282ab87d14d62b60c5ffe6a37'
        : value.length === zeroBytes && value.every((b) => b === 0))
    if (!known || extras.has(tag)) refuse('DocInfo', `unsupported record 0x${tag.toString(16)}`)
    extras.add(tag)
  }
  return data.readUInt16LE(props.body)
}

function bodyRecords(path: string, data: Buffer, records: HwpRecord[]): void {
  let last = records.length - 1
  while (last >= 0 && records[last]!.level !== 0) last--
  if (last < 0) refuse(path, 'missing body paragraph')
  const parents: { tag: number; control?: string }[] = []
  let tail = false
  for (const [index, { level, tag, body, end }] of records.entries()) {
    if (level > parents.length || level > 64) refuse(path, 'record without an owner')
    const parent = parents[level - 1]
    if (index > last && level === 1 && (tag === 72 || tag === 93)) tail = true
    const inTail = tail && level === 1
    const owner = parent?.control
    const ok =
      tag === 66
        ? level === 0 || inTail || parent?.tag === 71 || parent?.tag === 76
        : tag >= 67 && tag <= 71
          ? parent?.tag === 66
          : tag === 72
            ? inTail || parent?.tag === 71 || parent?.tag === 76
            : tag >= 73 && tag <= 75
              ? owner === 'secd'
              : tag === 76
                ? owner === 'gso ' || parent?.tag === 76
                : tag === 77
                  ? owner === 'tbl '
                  : (tag >= 78 && tag <= 83) || tag === 85 || tag === 86
                    ? parent?.tag === 76
                    : tag === 87
                      ? parent?.tag === 71 || parent?.tag === 76
                      : tag === 88
                        ? owner === 'eqed'
                        : tag === 93 && inTail
    if (!ok) refuse(path, `unsupported record 0x${tag.toString(16)} at level ${level}`)
    if (
      tag === 77 &&
      (end - body < 8 || data.readUInt16LE(body + 4) * data.readUInt16LE(body + 6) > 40000)
    )
      refuse(path, 'table grid exceeds source limits')
    parents.length = level
    parents.push(
      tag === 71 && end - body >= 4
        ? {
            tag,
            control: String.fromCharCode(
              data[body + 3]!,
              data[body + 2]!,
              data[body + 1]!,
              data[body]!,
            ),
          }
        : { tag },
    )
  }
}

// zlib and the browser's fflate shim report malformed or incomplete input differently.
const invalidDeflate: readonly unknown[] = [
  'Z_DATA_ERROR',
  'Z_BUF_ERROR',
  'Z_NEED_DICT',
  0,
  1,
  2,
  3,
  6,
]

function binaryFootprint(data: Buffer): number {
  let size = data.length
  // rhwp also tries raw/zlib decoders on BinData with a misleading compression flag.
  for (const decode of [inflateRawSync, inflateSync]) {
    try {
      size = Math.max(size, decode(data, { maxOutputLength: maxPart }).length)
    } catch (err) {
      // Stored images need not decode; allocation limits and unexpected errors must still fail closed.
      const code: unknown = err && typeof err === 'object' && 'code' in err ? err.code : undefined
      if (!invalidDeflate.includes(code)) throw err
    }
  }
  return size
}

function parts(bytes: Uint8Array) {
  if (!bytes.length || bytes.length > maxFile) refuse('CFB', 'file size limit')
  const opened = openHwp(bytes) ?? refuse('CFB', 'not a plain HWP5 document')
  const { FileIndex, FullPaths } = opened.cfb
  const root = FullPaths[0]!
  const seen = new Set<string>()
  const assets = new Set<string>()
  const streams = new Map<string, Buffer>()
  let total = 0
  let declared = 0
  let sections = 0
  for (const [i, e] of FileIndex.entries()) {
    if (e.type === 0) continue
    if (e.state !== 0 || e.clsid !== '00000000000000000000000000000000')
      refuse('CFB', 'unmodelled directory attributes')
    if (e.type === 5 && i === 0) continue
    const path = FullPaths[i]!.slice(root.length)
    const key = path.toLowerCase()
    if (seen.size >= maxParts || seen.has(key)) refuse(path, 'ambiguous or excessive directory')
    seen.add(key)
    if (e.type === 1 && storages.includes(path)) {
      streams.set(path, Buffer.alloc(0))
      continue
    }
    if (e.type !== 2 || e.size > maxPart || !e.content) refuse(path, 'unsupported directory entry')
    const raw = Buffer.from(e.content as Uint8Array)
    let data: Buffer = raw
    let size = raw.length
    if (path === 'DocInfo' || section.test(path)) {
      data = opened.compressed ? inflateRawSync(raw, { maxOutputLength: maxPart }) : raw
      size = data.length
      const records = recordsOf(data, 40000)
      if (!records?.length) refuse(path, 'invalid or excessive records')
      if (path === 'DocInfo') declared = docInfo(data, records, assets)
      else {
        bodyRecords(path, data, records)
        sections++
      }
    } else if (path === 'FileHeader') {
      if (
        size !== 256 ||
        !data.subarray(0, 32).equals(signature) ||
        data[35] !== 5 ||
        (data.readUInt32LE(36) & ~1) !== 0 ||
        data.readUInt32LE(40) !== 0 ||
        ![0, 4].includes(data.readUInt32LE(44)) ||
        data.subarray(48).some((v) => v !== 0)
      )
        refuse(path, 'protected or unmodelled header')
    } else if (Object.hasOwn(scripts, path)) {
      const script = opened.compressed ? inflateRawSync(raw, { maxOutputLength: maxPart }) : raw
      if (script.toString('hex') !== scripts[path]) refuse(path, 'custom script')
      size = Math.max(size, script.length)
    } else if (image.test(path)) size = binaryFootprint(raw)
    else if (!staticStreams.includes(path)) refuse(path, 'unrecognised stream')
    total += size
    if (total > maxTotal) refuse(path, 'expanded size limit')
    streams.set(path, data)
  }
  if (!streams.has('FileHeader') || !sections || declared !== sections)
    refuse('DocInfo', 'section count mismatch')
  for (let i = 0; i < sections; i++)
    if (!streams.has(`BodyText/Section${i}`)) refuse('BodyText', 'missing section')
  for (const asset of assets) if (!streams.has(asset)) refuse(asset, 'missing image')
  for (const path of streams.keys())
    if (image.test(path) && !assets.has(path)) refuse(path, 'unowned image')
  return { opened, streams, sections }
}

// rhwp can drop supplementary characters from PrvText on reopen, even when PARA_TEXT is intact.
function matchesBodyPreview(doc: HwpDocument, original: Buffer, rebuilt: Buffer): boolean {
  if (doc.getSectionCount() !== 1 || rebuilt.length >= original.length) return false
  let text = ''
  const paragraphs = doc.getParagraphCount(0)
  for (let i = 0; i < paragraphs; i++) {
    const length = doc.getParagraphLength(0, i)
    if (length) text += `${doc.getTextRange(0, i, 0, length)}\r\n`
  }
  return (
    Buffer.from(text, 'utf16le').equals(original) &&
    Buffer.from(text.replace(/[\u{10000}-\u{10ffff}]/gu, ''), 'utf16le').equals(rebuilt)
  )
}

function compareStream(
  path: string,
  original: Buffer,
  rebuilt: Buffer | undefined,
  doc: HwpDocument,
): void {
  if (!rebuilt) refuse(path, 'missing after reconstruction')
  if (original.equals(rebuilt)) return
  if (path === 'PrvText' && matchesBodyPreview(doc, original, rebuilt)) return
  if (path !== 'DocInfo' && !section.test(path)) refuse(path, 'changed during reconstruction')
  const before = recordsOf(original, 40000)!
  const after = recordsOf(rebuilt, 40000)!
  if (before.length !== after.length) refuse(path, 'record count changed during reconstruction')
  for (const [i, record] of before.entries()) {
    const next = after[i]!
    if (record.tag !== next.tag || record.level !== next.level)
      refuse(path, `record ${i} changed owner`)
    const a = original.subarray(record.body, record.end)
    const b = rebuilt.subarray(next.body, next.end)
    if (a.equals(b)) continue
    // createTable omits the optional zero prevent-page-break word before its empty description.
    if (
      record.tag === 71 &&
      a.length === 42 &&
      b.length === 46 &&
      a.readUInt32LE() === 0x74626c20 &&
      a.subarray(0, 40).equals(b.subarray(0, 40)) &&
      a.readUInt16LE(40) === 0 &&
      b.readUInt32LE(40) === 0 &&
      b.readUInt16LE(44) === 0
    )
      continue
    refuse(path, `record ${i} (0x${record.tag.toString(16)}) lost a property during reconstruction`)
  }
}

/** Prove the original HWP5 records survive saving from editable fields, not just from untouched stream caches. */
export function certifyHwp5Source(
  bytes: Uint8Array,
  Document: new (bytes: Uint8Array) => HwpDocument,
): void {
  const source = parts(bytes)
  // A whole missing tail cannot prove itself by being reinserted from the original.
  scanSectionTails(source.opened)
  const kinds = scanFieldKinds(source.opened)
  for (const kind of kinds.values()) {
    if (kind !== 0x25256d65 && kind !== 0x25756e6b)
      refuse('BodyText', 'unmodelled field opening kind')
  }
  const doc = new Document(bytes)
  try {
    if (
      doc.getSourceFormat() !== 'hwp' ||
      doc.getSectionCount() !== source.sections ||
      !('exportReconstructedHwpForCollab' in doc) ||
      typeof doc.exportReconstructedHwpForCollab !== 'function'
    )
      refuse('engine', 'missing HWP5 reconstruction proof')
    const rebuilt: unknown = doc.exportReconstructedHwpForCollab()
    if (!(rebuilt instanceof Uint8Array)) refuse('engine', 'invalid reconstruction result')
    const checked = parts(rebuilt)
    const fixed = kinds.size ? restoreFieldKinds(rebuilt, kinds).bytes : rebuilt
    const after = fixed === rebuilt ? checked : parts(fixed)
    for (const [name, original] of source.streams)
      compareStream(name, original, after.streams.get(name), doc)
    if (after.streams.size !== source.streams.size)
      refuse('CFB', 'additional stream during reconstruction')
  } finally {
    doc.free()
  }
}
