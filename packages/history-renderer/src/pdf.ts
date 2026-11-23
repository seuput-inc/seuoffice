import { Buffer } from 'node:buffer'
import { HistoryError, type Json } from '@seuoffice/collab/client'
import { degrees, ParseSpeeds, PDFDocument } from 'pdf-lib'
import type { ByteHistoryAdapter } from './adapter'

function profile(condition: unknown): asserts condition {
  if (!condition)
    throw new HistoryError('unsupported', 'PDF is outside the flat, unfiltered page profile')
}

// A single classic xref table must account for every byte between the header and trailer.
// The PDF parser never sees an object until its role and its complete grammar have been checked.
function checkPages(bytes: Uint8Array): number {
  const text = Buffer.from(bytes.buffer, bytes.byteOffset, bytes.byteLength).toString('latin1')
  const tail = Math.max(0, text.length - 96)
  const end = /\nstartxref\r?\n(\d{1,7})\r?\n%%EOF[\r\n]*$/.exec(text.slice(tail))
  profile(end)
  const xref = Number(end[1])
  const table = text.slice(xref, tail + end.index)
  const head = /^xref\r?\n0 ([1-9]\d{0,2})\r?\n0000000000 65535 f(?: \n|\r\n)/.exec(table)
  profile(head)
  const size = Number(head[1])
  profile(size >= 4 && size <= 260)
  const offsets = [0]
  const entry = /(\d{10}) 00000 n(?: \n|\r\n)/y
  entry.lastIndex = head[0].length
  for (let id = 1; id < size; id++) {
    const row = entry.exec(table)
    profile(row)
    const offset = Number(row[1])
    profile(offset > offsets[id - 1]! && offset < xref)
    offsets.push(offset)
  }
  offsets.push(xref)
  const compact = (s: string) => s.replace(/[ \r\n\t]+/g, ' ').trim()
  const trailer = /^trailer << \/Size (\d+) \/Root (\d+) 0 R(?: \/Info (\d+) 0 R)? >>$/.exec(
    compact(table.slice(entry.lastIndex)),
  )
  profile(trailer && Number(trailer[1]) === size)
  profile(/^%PDF-1\.[0-7]\r?\n(?:%[\x80-\xff]{4}\r?\n)?[\r\n]*$/.test(text.slice(0, offsets[1])))
  profile(!/[^\x20-\x7e\t\r\n]/.test(text.slice(offsets[1])))

  const seen = new Set<number>()
  function body(id: number): string {
    profile(id > 0 && id < size && !seen.has(id))
    seen.add(id)
    const object = text.slice(offsets[id], offsets[id + 1])
    profile(object.length <= 32768)
    const start = /^(\d{1,3}) 0 obj\r?\n/.exec(object)
    const stop = /\r?\nendobj\r?\n[\r\n]*$/.exec(object)
    profile(start && Number(start[1]) === id && stop)
    return object.slice(start[0].length, stop.index)
  }
  if (trailer[3]) {
    const info = compact(body(Number(trailer[3])))
    profile(
      /^<< (?:\/(?:Title|Author|Subject|Keywords|Creator|Producer|CreationDate|ModDate) (?:<[\dA-Fa-f]{0,4096}>|\(D:\d{14}Z\)) ){1,8}>>$/.test(
        info,
      ),
    )
    const keys = info.match(/\/[A-Za-z]+/g)!
    profile(new Set(keys).size === keys.length)
  }
  const catalog = /^<< \/Type \/Catalog \/Pages (\d{1,3}) 0 R >>$/.exec(
    compact(body(Number(trailer[2]))),
  )
  profile(catalog)
  const treeId = Number(catalog[1])
  const tree =
    /^<< \/Type \/Pages \/Kids \[((?: \d{1,3} 0 R){1,128}) \] \/Count (\d{1,3}) >>$/.exec(
      compact(body(treeId)),
    )
  profile(tree)
  const kids = [...tree[1]!.matchAll(/ (\d+) 0 R/g)]
  profile(kids.length === Number(tree[2]))
  for (const kid of kids) {
    const page =
      /^<< \/Type \/Page \/Parent (\d{1,3}) 0 R \/Resources << >> \/MediaBox \[ 0 0 (\d{1,5}(?:\.\d{1,8})?) (\d{1,5}(?:\.\d{1,8})?) \]((?: \/(?:Rotate (?:0|90|180|270)|Contents \d{1,3} 0 R)){0,2}) >>$/.exec(
        compact(body(Number(kid[1]))),
      )
    profile(page && Number(page[1]) === treeId)
    profile([Number(page[2]), Number(page[3])].every((n) => n > 0 && n <= 14400))
    const extra = page[4]!
    const keys = extra.match(/\/[A-Za-z]+/g) ?? []
    profile(keys.length === new Set(keys).size)
    const contents = /\/Contents (\d+) 0 R/.exec(extra)
    if (contents) {
      const stream = body(Number(contents[1]))
      const start = /^<<[ \t\r\n]+\/Length[ \t\r\n]+(\d{1,4})[ \t\r\n]+>>\r?\nstream\r?\n/.exec(
        stream,
      )
      profile(start)
      const from = start[0].length
      const length = Number(start[1])
      profile(length <= 2048 && /^\r?\nendstream$/.test(stream.slice(from + length)))
      // With empty resources these filled rectangles need no font, image decoder or external object.
      const paint = compact(stream.slice(from, from + length))
      profile(paint === '' || /^(?:(?:-?\d{1,5}(?:\.\d{1,5})? ){4}re f(?: |$)){1,64}$/.test(paint))
    }
  }
  profile(seen.size === size - 1)
  return kids.length
}

// apps/pdf/src/web/collab-model.ts stores a confirmed rotation as an absolute delta from the base.
// `was` is for rebasing *local* edits, not for applying the confirmed log.
type Rotation = { b: 'rotations'; k: number; was: number; v: number }
type PdfTurn = { id: string; puts: Rotation[] }
const object = (v: Json): v is { [key: string]: Json } =>
  !!v && typeof v === 'object' && !Array.isArray(v)
const turn = (n: Json | undefined): n is number => n === 0 || n === 90 || n === 180 || n === 270

function invalid(): never {
  throw new HistoryError('invalid_history', 'invalid PDF rotation change')
}

export async function openPdf(base: Uint8Array): Promise<ByteHistoryAdapter<PdfTurn>> {
  profile(base.length <= 1024 * 1024)
  const count = checkPages(base)
  const doc = await PDFDocument.load(base, {
    updateMetadata: false,
    throwOnInvalidObject: true,
    parseSpeed: ParseSpeeds.Fastest,
  })
  const pages = doc.getPages()
  profile(count === pages.length)
  const angles = pages.map((page) => page.getRotation().angle)
  const deltas = new Uint16Array(count)
  return {
    decode(json) {
      if (
        !object(json) ||
        Object.keys(json).length !== 2 ||
        typeof json.id !== 'string' ||
        !json.id.length ||
        json.id.length > 128 ||
        !Array.isArray(json.puts) ||
        json.puts.length > 5000
      )
        invalid()
      const puts = json.puts.map((p): Rotation => {
        if (!object(p) || typeof p.b !== 'string') invalid()
        if (p.b !== 'rotations')
          throw new HistoryError('unsupported', 'only PDF page rotations are supported')
        if (
          Object.keys(p).length !== 4 ||
          typeof p.k !== 'number' ||
          !Number.isInteger(p.k) ||
          p.k < 0 ||
          p.k >= count ||
          !turn(p.was) ||
          !turn(p.v)
        )
          invalid()
        return { b: p.b, k: p.k, was: p.was, v: p.v }
      })
      return { id: json.id, puts }
    },
    encode: (op) => op,
    integrate(remote, local) {
      if (local.length) invalid()
      for (const op of remote) for (const { k, v } of op.puts) deltas[k] = v
      return []
    },
    captureUndo(op) {
      const touched = new Map<number, number>()
      for (const { k, v } of op.puts) touched.set(k, v)
      const inverse: Rotation[] = []
      for (const [k, was] of touched) {
        const v = deltas[k]!
        if (was !== v) inverse.push({ b: 'rotations', k, was, v })
      }
      if (!inverse.length) throw new HistoryError('unsupported', 'rotation did not change the page')
      return (later) =>
        later.some((change) => change.puts.some(({ k }) => touched.has(k)))
          ? null
          : [{ id: crypto.randomUUID(), puts: inverse }]
    },
    async produceFile() {
      if (!deltas.some(Boolean)) return { fileName: 'history.pdf', bytes: base }
      for (const [i, page] of pages.entries()) {
        const angle = (angles[i]! + deltas[i]!) % 360
        if (page.getRotation().angle !== angle) page.setRotation(degrees(angle))
      }
      const bytes = await doc.save({
        useObjectStreams: false,
        updateFieldAppearances: false,
        objectsPerTick: Infinity,
      })
      return { fileName: 'history.pdf', bytes }
    },
  }
}
