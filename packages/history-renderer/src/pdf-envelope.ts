import { Buffer } from 'node:buffer'
import { HistoryError } from '@seuoffice/collab/client'

export function profile(ok: unknown): asserts ok {
  if (!ok) throw new HistoryError('unsupported', 'outside the bounded PDF profile')
}

const maxBytes = 1024 * 1024
const maxObjects = 260
export const compact = (s: string) => s.replace(/[ \r\n\t]+/g, ' ').trim()

// Only direct scalars and short integer arrays are meaningful in these transport dictionaries.
function fields(text: string, allowed: readonly string[]) {
  profile(text.length <= 512 && !/[^\x20-\x7e\t\r\n]/.test(text))
  const src = compact(text)
  profile(src.startsWith('<< ') && src.endsWith(' >>'))
  const entry = / \/([A-Za-z]+) (\d{1,7} 0 R|\d{1,7}|\/[A-Za-z]+|\[(?: \d{1,3}){1,3} \])/y
  const result = new Map<string, string>()
  for (let at = 2; at < src.length - 3; at = entry.lastIndex) {
    entry.lastIndex = at
    const m = entry.exec(src)
    profile(m && allowed.includes(m[1]!) && !result.has(m[1]!))
    result.set(m[1]!, m[2]!)
  }
  return result
}

function integer(value: string | undefined, max: number): number {
  profile(value && /^\d{1,7}$/.test(value) && Number(value) <= max)
  return Number(value)
}

function envelope(text: string, expected?: number) {
  const start = /^(\d{1,3}) 0 obj\r?\n/.exec(text)
  const stop = /\r?\nendobj\r?\n[\r\n]*$/.exec(text)
  profile(start && stop && (expected === undefined || Number(start[1]) === expected))
  return { id: Number(start[1]), body: text.slice(start[0].length, stop.index) }
}

interface PdfStream {
  dict: Map<string, string>
  filtered: boolean
  data: string
}

export interface PdfEnvelope {
  objects: Map<number, string>
  root: number
  info?: number
}

function stream(body: string, allowed: readonly string[]): PdfStream {
  const start = /^(<<[\x20-\x7e\t\r\n]{0,512}?>>)\r?\nstream\r?\n/.exec(body)
  profile(start)
  const dict = fields(start[1]!, ['Filter', 'Length', ...allowed])
  const length = integer(dict.get('Length'), maxBytes)
  const end = start[0].length + length
  profile(/^\r?\nendstream$/.test(body.slice(end)))
  const filter = dict.get('Filter')
  profile(filter === undefined || filter === '/FlateDecode')
  return { dict, filtered: !!filter, data: body.slice(start[0].length, end) }
}

// No compressed container or page stream is passed to pdf-lib. Bound both native transform
// input per pull and actual delivered bytes, including all object streams together.
async function unpack(s: PdfStream, limit: number) {
  if (!s.filtered) {
    profile(s.data.length <= limit)
    return s.data
  }
  const packed = Buffer.from(s.data, 'latin1')
  let offset = 0
  const src = new ReadableStream<Uint8Array<ArrayBuffer>>({
    pull(controller) {
      if (offset === packed.length) return controller.close()
      const end = Math.min(offset + 256, packed.length)
      controller.enqueue(packed.subarray(offset, end))
      offset = end
    },
  })
  const reader = src.pipeThrough(new DecompressionStream('deflate')).getReader()
  const chunks: string[] = []
  let size = 0
  for (;;) {
    const { done, value } = await reader.read()
    if (done) return chunks.join('')
    if ((size += value.byteLength) > limit) {
      await reader.cancel()
      profile(false)
    }
    chunks.push(Buffer.from(value.buffer, value.byteOffset, value.byteLength).toString('latin1'))
  }
}

type Row = readonly [type: number, offsetOrContainer: number, generationOrIndex: number]

/** One complete revision, contiguous generation-zero document objects, no incremental or hybrid xref. */
export async function readPdfEnvelope(bytes: Uint8Array): Promise<PdfEnvelope> {
  profile(bytes.length <= maxBytes)
  const text = Buffer.from(bytes.buffer, bytes.byteOffset, bytes.byteLength).toString('latin1')
  const tail = Math.max(0, text.length - 96)
  const end = /\nstartxref\r?\n(\d{1,7})\r?\n%%EOF[\r\n]*$/.exec(text.slice(tail))
  profile(end)
  const xref = Number(end[1])
  const stop = tail + end.index
  profile(xref > 0 && xref < stop)
  const table = text.slice(xref, stop)
  let budget = maxBytes
  const decoded = async (s: PdfStream, limit: number) => {
    const data = await unpack(s, Math.min(limit, budget))
    budget -= data.length
    return data
  }
  const rows: Row[] = []
  let trailer: Map<string, string>
  let xrefId: number | undefined
  if (table.startsWith('xref')) {
    profile(table.length <= 8192)
    const head = /^xref\r?\n0 ([1-9]\d{0,2})\r?\n0000000000 65535 f(?: \n|\r\n)/.exec(table)
    profile(head)
    const size = integer(head[1], maxObjects)
    const entry = /(\d{10}) 00000 n(?: \n|\r\n)/y
    entry.lastIndex = head[0].length
    rows.push([0, 0, 65535])
    for (let id = 1; id < size; id++) {
      const m = entry.exec(table)
      profile(m)
      rows.push([1, Number(m[1]), 0])
    }
    const rest = compact(table.slice(entry.lastIndex))
    profile(rest.startsWith('trailer '))
    trailer = fields(rest.slice(8), ['Size', 'Root', 'Info'])
  } else {
    const obj = envelope(table)
    xrefId = obj.id
    const s = stream(obj.body, ['Type', 'Size', 'Root', 'Info', 'Index', 'W'])
    trailer = s.dict
    const size = integer(trailer.get('Size'), maxObjects)
    profile(
      trailer.get('Type') === '/XRef' &&
        (!trailer.has('Index') || trailer.get('Index') === `[ 0 ${size} ]`),
    )
    const widths = /^\[ 1 ([1-4]) ([1-2]) \]$/.exec(trailer.get('W') ?? '')
    profile(widths)
    const second = Number(widths[1]),
      third = Number(widths[2])
    const expected = size * (1 + second + third)
    const data = Buffer.from(await decoded(s, expected), 'latin1')
    profile(data.length === expected)
    for (let p = 0; p < expected; p += 1 + second + third)
      rows.push([data[p]!, data.readUIntBE(p + 1, second), data.readUIntBE(p + 1 + second, third)])
    profile(rows[0]?.[0] === 0 && rows[0][1] === 0 && rows[0][2] === 65535)
    profile(rows[xrefId]?.[0] === 1 && rows[xrefId]![1] === xref)
  }
  profile(rows.length >= 4 && integer(trailer.get('Size'), maxObjects) === rows.length)
  const ref = (name: string) => {
    const m = /^(\d{1,3}) 0 R$/.exec(trailer.get(name) ?? '')
    profile(m && Number(m[1]) > 0 && Number(m[1]) < rows.length)
    return Number(m[1])
  }
  const root = ref('Root'),
    info = trailer.has('Info') ? ref('Info') : undefined
  const direct: { id: number; offset: number }[] = []
  for (let id = 1; id < rows.length; id++) {
    const [type, pos, generation] = rows[id]!
    if (type === 2) {
      profile(pos > 0 && pos < rows.length && generation < rows.length)
    } else {
      profile(type === 1 && generation === 0 && (id === xrefId || (pos > 0 && pos < xref)))
      direct.push({ id, offset: pos })
    }
  }
  direct.sort((a, b) => a.offset - b.offset)
  profile(
    direct.length &&
      /^%PDF-1\.[0-7]\r?\n(?:%[\x80-\xff]{4}\r?\n)?[\r\n]*$/.test(text.slice(0, direct[0]!.offset)),
  )
  const objects = new Map<number, string>()
  let containers = 0
  for (const [i, { id, offset }] of direct.entries()) {
    if (id === xrefId) continue
    const { body } = envelope(text.slice(offset, direct[i + 1]?.offset ?? xref), id)
    if (!/^<<[\x20-\x7e\t\r\n]{0,512}?>>\r?\nstream\r?\n/.test(body)) {
      profile(body.length <= 32768)
      objects.set(id, body)
      continue
    }
    const s = stream(body, ['Type', 'N', 'First'])
    if (s.dict.has('Type')) {
      profile(xrefId !== undefined && s.dict.get('Type') === '/ObjStm')
      containers++
      const n = integer(s.dict.get('N'), rows.length - 1),
        first = integer(s.dict.get('First'), rows.length * 12)
      profile(n > 0 && first > 0)
      const raw = await decoded(s, maxBytes)
      const header = raw.slice(0, first)
      const pair = /(\d{1,3}) (\d{1,7})[ \r\n]+/y
      const items: { key: number; start: number }[] = []
      for (let j = 0; j < n; j++) {
        const m = pair.exec(header)
        profile(m)
        const key = Number(m[1]),
          start = first + Number(m[2]),
          row = rows[key]
        profile(row && row[0] === 2 && row[1] === id && row[2] === j)
        profile((j === 0 ? start === first : start > items[j - 1]!.start) && start < raw.length)
        items.push({ key, start })
      }
      profile(pair.lastIndex === first)
      for (const [j, { key, start }] of items.entries()) {
        const body = raw.slice(start, items[j + 1]?.start ?? raw.length)
        profile(
          body.length <= 32768 && !/[^\x20-\x7e\t\r\n]/.test(body) && !/\nstream\r?\n/.test(body),
        )
        objects.set(key, body.trimEnd())
      }
    } else {
      profile(!s.dict.has('N') && !s.dict.has('First'))
      const paint = await decoded(s, 2048)
      objects.set(id, `<< /Length ${paint.length} >>\nstream\n${paint}\nendstream`)
    }
  }
  profile(objects.size + containers + (xrefId === undefined ? 0 : 1) === rows.length - 1)
  return { objects, root, info }
}

/** pdf-lib receives only checked, uncompressed objects, never a second interpretation of input framing. */
export function classicPdf({ objects, root, info }: PdfEnvelope) {
  const parts = ['%PDF-1.7\n\n']
  let pos = parts[0]!.length
  const xref = [`xref\n0 ${objects.size + 1}\n0000000000 65535 f \n`]
  for (let id = 1; id <= objects.size; id++) {
    const body = objects.get(id)
    profile(body !== undefined)
    xref.push(`${String(pos).padStart(10, '0')} 00000 n \n`)
    const obj = `${id} 0 obj\n${body}\nendobj\n\n`
    pos += obj.length
    parts.push(obj)
  }
  parts.push(
    xref.join(''),
    `trailer\n<< /Size ${objects.size + 1} /Root ${root} 0 R${info ? ` /Info ${info} 0 R` : ''} >>\n\nstartxref\n${pos}\n%%EOF`,
  )
  const text = parts.join('')
  profile(text.length <= maxBytes)
  return Buffer.from(text, 'latin1')
}
