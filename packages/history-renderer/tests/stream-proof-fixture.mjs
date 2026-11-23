import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import { closeSync, createReadStream, fsyncSync, openSync, writeSync } from 'node:fs'
import { join } from 'node:path'

export const MiB = 1024 * 1024
export const epoch = 'terrain-survey'
export const original = { title: 'Terrain survey', author: 'Survey team' }
export const turn = (id, k, was, v) => ({ id, puts: [{ b: 'rotations', k, was, v }] })
export const change = (v, client, ops) => ({ v, client, seq: v, author: client, ops })
export const header = (baseHash, version) => ({
  epoch,
  format: { name: 'pdf', schema: 2 },
  baseHash,
  version,
})
export const paint = 'q 612 0 0 792 0 0 cm /Im0 Do Q'
export const rectangle = '12 24 96 32 re f'
export const sha = () => createHash('sha256')
export async function fileHash(path) {
  const h = sha()
  for await (const b of createReadStream(path)) h.update(b)
  return h.digest('hex')
}

function elevation(x, y) {
  const ridge = y - 0.45 - 0.24 * Math.sin(x * 9)
  return Math.round(
    310 +
      1400 * Math.exp(-22 * ridge * ridge) +
      140 * Math.cos(21 * x + 7 * y) +
      95 * Math.sin(29 * y - 11 * x),
  )
}

function writeAll(fd, chunk) {
  const bytes = typeof chunk === 'string' ? Buffer.from(chunk) : chunk
  for (let at = 0; at < bytes.length;) {
    const n = writeSync(fd, bytes, at, bytes.length - at)
    assert(n > 0)
    at += n
  }
  return bytes
}

// Each page covers half of the same topographic survey. Dark contours mark 100 m elevation intervals.
export function makePdf(dir, limit) {
  const path = join(dir, 'retained.pdf')
  const fd = openSync(path, 'wx')
  const hash = sha()
  const offsets = [0]
  const images = []
  let size = 0
  const emit = (chunk) => {
    const bytes = writeAll(fd, chunk)
    hash.update(bytes)
    size += bytes.length
  }
  const begin = () => {
    offsets.push(size)
    emit(`${offsets.length - 1} 0 obj\n`)
  }
  const object = (body) => {
    begin()
    emit(`${body}\nendobj\n`)
  }
  const width = limit ? Math.floor(Math.sqrt((((limit - 8192) / 6) * 612) / 792)) : 0
  const height = limit ? Math.floor((limit - 8192) / (6 * width)) : 0
  try {
    emit('%PDF-1.7\n')
    object('<< /Type /Catalog /Pages 2 0 R >>')
    object(`<< /Type /Pages /Kids [ 3 0 R ${limit ? 6 : 5} 0 R ] /Count 2 >>`)
    for (let page = 0; page < 2; page++) {
      const id = offsets.length
      const resources = limit ? `<< /XObject << /Im0 ${id + 2} 0 R >> >>` : '<< >>'
      object(
        `<< /Type /Page /Parent 2 0 R /MediaBox [ 0 0 612 792 ] /Rotate 0 /Resources ${resources} /Contents ${id + 1} 0 R >>`,
      )
      const program = limit ? paint : rectangle
      object(`<< /Length ${program.length} >>\nstream\n${program}\nendstream`)
      if (!limit) continue
      begin()
      const byteLength = width * height * 3
      emit(
        `<< /Type /XObject /Subtype /Image /Width ${width} /Height ${height} /ColorSpace /DeviceRGB /BitsPerComponent 8 /Length ${byteLength} >>\nstream\n`,
      )
      const imageHash = sha()
      const rows = Buffer.allocUnsafe(width * 3 * 8)
      for (let top = 0; top < height; top += 8) {
        let i = 0
        for (let y = top; y < Math.min(height, top + 8); y++) {
          for (let x = 0; x < width; x++) {
            const z = elevation((page + x / (width - 1)) / 2, y / (height - 1))
            const t = Math.max(0, Math.min(1, z / 2000))
            const light = z % 100 < 5 ? 0.4 : 1
            rows[i++] = (25 + 225 * t) * light
            rows[i++] = (104 + 95 * Math.sin(Math.PI * t)) * light
            rows[i++] = (154 - 95 * t) * light
          }
        }
        const bytes = rows.subarray(0, i)
        imageHash.update(bytes)
        emit(bytes)
      }
      images.push({ width, height, byteLength, sha256: imageHash.digest('hex') })
      emit('\nendstream\nendobj\n')
    }
    object('<< /Title (Terrain survey) /Author (Survey team) >>')
    const start = size
    emit(`xref\n0 ${offsets.length}\n0000000000 65535 f \n`)
    for (const off of offsets.slice(1)) emit(`${String(off).padStart(10, '0')} 00000 n \n`)
    emit(
      `trailer\n<< /Size ${offsets.length} /Root 1 0 R /Info ${offsets.length - 1} 0 R >>\nstartxref\n${start}\n%%EOF\n`,
    )
    fsyncSync(fd)
  } finally {
    closeSync(fd)
  }
  if (limit) assert(size <= limit && size > limit - 32768)
  return { path, size, hash: hash.digest('hex'), images }
}

// A surveyor saves the active cross-section to Info. Each edit carries measured heights, not filler.
function transect(n) {
  const y0 = (n % 251) / 250
  const y1 = (Math.floor(n / 251) % 251) / 250
  const samples = []
  for (let i = 0; i < 180; i++) {
    const x = i / 179
    samples.push(elevation(x, y0 + x * (y1 - y0)))
  }
  const meta = {
    title: `Terrain survey, cross-section ${n + 1}`,
    subject: `West ${y0.toFixed(3)} to east ${y1.toFixed(3)}, 180 equally spaced elevation samples (m): ${samples.join(' ')}`,
  }
  assert(meta.subject.length <= 1023)
  return { id: `section-${n + 1}`, puts: [{ b: 'metadata', v: meta }] }
}

export function nextSurveyBatch(log) {
  return change(
    log.version + 1,
    'Surveyor',
    Array.from({ length: 32 }, (_, i) => transect(log.sections + i)),
  )
}

export function makeHistory(dir, pdf, limit) {
  const path = join(dir, 'retained.ndjson')
  const fd = openSync(path, 'wx')
  let version = 0,
    ops = 0,
    framedBytes = 0,
    contentBytes = 0,
    sections = 0
  let lastMeta, prefixMeta
  const append = (client, batch) => {
    const line = JSON.stringify(change(version + 1, client, batch))
    assert(Buffer.byteLength(line) <= 64 * 1024)
    writeAll(fd, line + '\n')
    contentBytes += Buffer.byteLength(line)
    framedBytes += 4 + Buffer.byteLength(line)
    ops += batch.length
    version++
  }
  const cost = (v, payloadBytes) =>
    Buffer.byteLength(JSON.stringify(header(pdf.hash, v))) +
    ',"changes":[]'.length +
    Math.max(0, v - 1) +
    payloadBytes
  try {
    const first = transect(sections++)
    prefixMeta = first.puts[0].v
    append('Alice', [first, turn('rotate-west', 0, 0, 90)])
    append('Bob', [turn('rotate-east', 1, 0, 270)])
    let done = false
    while (!done) {
      const batch = []
      let bytes = Buffer.byteLength(JSON.stringify(change(version + 1, 'Surveyor', [])))
      for (let j = 0; j < 32; j++) {
        const op = transect(sections)
        const add = Buffer.byteLength(JSON.stringify(op)) + (j ? 1 : 0)
        // The full JSON request, an undo selector, and its appended compensation must all fit.
        if (cost(version + 1, contentBytes + bytes + add) > limit - 512) {
          done = true
          break
        }
        sections++
        lastMeta = op.puts[0].v
        bytes += add
        batch.push(op)
      }
      if (batch.length) append('Surveyor', batch)
    }
    fsyncSync(fd)
  } finally {
    closeSync(fd)
  }
  const jsonBytes = cost(version, contentBytes)
  assert(lastMeta && jsonBytes <= limit - 512 && jsonBytes > limit - 2048)
  return { path, version, ops, sections, framedBytes, jsonBytes, prefixMeta, lastMeta }
}
