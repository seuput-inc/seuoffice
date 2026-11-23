import { HistoryError } from '@seuoffice/collab/client'
import { assertZipInflatesWithinLimits } from '@seuoffice/zip-gate'
import JSZip from 'jszip'

const limits = { maxParts: 256, maxPartBytes: 1024 * 1024, maxTotalBytes: 4 * 1024 * 1024 }
const invalid = (): never => {
  throw new HistoryError('invalid_history', 'invalid office archive')
}
const unsupported = (): never => {
  throw new HistoryError('unsupported', 'outside the classic office ZIP profile')
}

/** Admit exactly one directory and the local records it names before handing bytes to JSZip. */
export async function checkArchive(bytes: Uint8Array): Promise<void> {
  const buf = Buffer.from(bytes.buffer, bytes.byteOffset, bytes.byteLength)
  const end = buf.length - 22
  if (end < 0 || buf.readUInt32LE(end) !== 0x06054b50 || buf.readUInt16LE(end + 20)) invalid()
  const count = buf.readUInt16LE(end + 10)
  const cd = buf.readUInt32LE(end + 16)
  if (
    !count ||
    count > limits.maxParts ||
    buf.readUInt32LE(end + 4) !== 0 ||
    count !== buf.readUInt16LE(end + 8) ||
    cd + buf.readUInt32LE(end + 12) !== end
  )
    invalid()
  let pos = cd
  let total = 0
  const locals: { start: number; end: number }[] = []
  const names = new Set<string>()
  for (let i = 0; i < count; i++) {
    if (pos + 46 > end || buf.readUInt32LE(pos) !== 0x02014b50) invalid()
    const flags = buf.readUInt16LE(pos + 8)
    const method = buf.readUInt16LE(pos + 10)
    const size = buf.readUInt32LE(pos + 24)
    const packed = buf.readUInt32LE(pos + 20)
    const nameLen = buf.readUInt16LE(pos + 28)
    const local = buf.readUInt32LE(pos + 42)
    const descriptor = (flags & 8) !== 0
    if (
      flags & ~0x808 ||
      (method !== 0 && method !== 8) ||
      buf.readUInt16LE(pos + 30) ||
      buf.readUInt16LE(pos + 32) ||
      buf.readUInt16LE(pos + 34)
    )
      unsupported()
    if (
      size > limits.maxPartBytes ||
      (total += size) > limits.maxTotalBytes ||
      (method === 0 && size !== packed) ||
      pos + 46 + nameLen > end ||
      local + 30 + nameLen + packed > cd
    )
      invalid()
    if (
      buf.readUInt32LE(local) !== 0x04034b50 ||
      buf.readUInt16LE(local + 28) ||
      buf.readUInt16LE(local + 26) !== nameLen ||
      buf.readUInt16LE(local + 6) !== flags ||
      buf.readUInt16LE(local + 8) !== method
    )
      invalid()
    let localEnd = local + 30 + nameLen + packed
    const declared = buf.subarray(pos + 16, pos + 28)
    if (descriptor) {
      if (
        localEnd + 16 > cd ||
        buf.readUInt32LE(localEnd) !== 0x08074b50 ||
        !buf.subarray(localEnd + 4, localEnd + 16).equals(declared) ||
        buf.subarray(local + 14, local + 26).some((v) => v !== 0)
      )
        invalid()
      localEnd += 16
    } else if (!buf.subarray(local + 14, local + 26).equals(declared)) invalid()
    const name = buf.toString('latin1', pos + 46, pos + 46 + nameLen)
    if (
      !buf
        .subarray(local + 30, local + 30 + nameLen)
        .equals(buf.subarray(pos + 46, pos + 46 + nameLen))
    )
      invalid()
    if (
      !/^[a-zA-Z0-9_.[\]/-]+$/.test(name) ||
      name
        .replace(/\/$/, '')
        .split('/')
        .some((p) => !p || p === '.' || p === '..') ||
      names.has(name.toLowerCase()) ||
      /(?:^|\/)_xmlsignatures\//i.test(name)
    )
      unsupported()
    if (
      (name.endsWith('/') && (size || packed)) ||
      (buf.readUInt32LE(pos + 38) & 16 && !name.endsWith('/'))
    )
      invalid()
    names.add(name.toLowerCase())
    locals.push({ start: local, end: localEnd })
    pos += 46 + nameLen
  }
  let start = 0
  for (const local of locals.sort((a, b) => a.start - b.start)) {
    if (start !== local.start) invalid()
    start = local.end
  }
  if (start !== cd || pos !== end || !names.has('[content_types].xml') || !names.has('_rels/.rels'))
    invalid()
  await assertZipInflatesWithinLimits(bytes, limits)
  await JSZip.loadAsync(bytes, { checkCRC32: true })
}
