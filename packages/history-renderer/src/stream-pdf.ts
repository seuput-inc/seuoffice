import { Buffer } from 'node:buffer'
import { PDFHexString } from 'pdf-lib'
import type { EditSnapshot } from '../../../apps/pdf/src/renderer/edit-state'
import { compact, profile } from './pdf-envelope'
import { array, dict, dictionary, num, ref } from './pdf-object-profile'

const MAX_FILE = 50 * 1024 * 1024
const MAX_SYNTAX = 1024 * 1024
const MAX_LINE = 32768
const MAX_PAGES = 64

type Span = { offset: number; length: number }
type Part = Span | { data: string }
type Payload = { kind: 'pixels'; span: Span } | { kind: 'paint'; text: string }
interface ScanObject {
  offset: number
  gap: string
  body: string
  payload?: Payload
}

// RGB samples admit every octet. Only their checked /Length, never a delimiter search,
// separates them from PDF syntax. The caller hashes the same chunks as it yields them.
class Source {
  pos = 0
  private chunk: Uint8Array = new Uint8Array(0)
  private at = 0
  private syntax = 0
  private readonly iterator: AsyncIterator<Uint8Array>

  constructor(
    chunks: AsyncIterable<Uint8Array>,
    readonly length: number,
  ) {
    this.iterator = chunks[Symbol.asyncIterator]()
  }

  private async refill() {
    while (this.at === this.chunk.length) {
      const next = await this.iterator.next()
      profile(!next.done && this.pos + next.value.byteLength <= this.length)
      this.chunk = next.value
      this.at = 0
    }
  }

  private consume(n: number, syntax: boolean) {
    this.at += n
    this.pos += n
    if (syntax) profile((this.syntax += n) <= MAX_SYNTAX)
  }

  async line() {
    let text = ''
    for (;;) {
      await this.refill()
      const start = this.at
      const lf = this.chunk.indexOf(10, start)
      const end = lf === -1 ? this.chunk.length : lf
      profile(text.length + end - start <= MAX_LINE)
      text += Buffer.from(this.chunk.buffer, this.chunk.byteOffset + start, end - start).toString(
        'latin1',
      )
      this.consume(end - start + (lf === -1 ? 0 : 1), true)
      if (lf !== -1) return text
    }
  }

  async dictionary() {
    let body = await this.line()
    profile(body.startsWith('<<'))
    for (;;) {
      const next = await this.line()
      if (next === 'stream' || next === 'endobj' || next === 'startxref') {
        profile(!/[^\x20-\x7e\t\n]/.test(body))
        return { body, next }
      }
      profile(body.length + 1 + next.length <= MAX_LINE)
      body += `\n${next}`
    }
  }

  async bytes(n: number, pixels: boolean) {
    profile(this.pos + n <= this.length)
    let text = ''
    while (n > 0) {
      await this.refill()
      const take = Math.min(n, this.chunk.length - this.at)
      if (!pixels)
        text += Buffer.from(this.chunk.buffer, this.chunk.byteOffset + this.at, take).toString(
          'latin1',
        )
      this.consume(take, !pixels)
      n -= take
    }
    return text
  }

  async finish() {
    profile(this.pos === this.length && this.at === this.chunk.length)
    for (;;) {
      const next = await this.iterator.next()
      if (next.done) return
      profile(next.value.byteLength === 0)
    }
  }
}

function infoFields(text: string) {
  profile(text.startsWith('<<') && text.endsWith('>>'))
  const fields = new Map<string, string>()
  const field =
    /[ \t\n]+\/(Title|Subject|Author|Keywords|Creator|Producer|CreationDate|ModDate)[ \t\n]+(<(?:[\da-fA-F]{2}){0,2048}>|\([^()\\\r\n]{0,1024}\))/y
  let at = 2
  for (;;) {
    field.lastIndex = at
    const m = field.exec(text)
    if (!m) break
    profile(!fields.has(m[1]!))
    fields.set(m[1]!, m[2]!)
    at = field.lastIndex
  }
  profile(/^[ \t\n]*>>$/.test(text.slice(at)))
  return fields
}

function streamLength(body: string) {
  profile(body.length <= 512)
  const tokens = compact(body).split(' ')
  profile(
    tokens[0] === '<<' && tokens.at(-1) === '>>' && (tokens.length === 4 || tokens.length === 16),
  )
  const fields = new Map<string, string>()
  for (let i = 1; i < tokens.length - 1; i += 2) {
    profile(!fields.has(tokens[i]!))
    fields.set(tokens[i]!, tokens[i + 1]!)
  }
  const raw = fields.get('/Length') ?? ''
  if (fields.size === 1) {
    profile(/^[1-9]\d{0,2}$/.test(raw))
    return { pixels: false, length: Number(raw) }
  }
  const w = fields.get('/Width') ?? '',
    h = fields.get('/Height') ?? ''
  profile(
    fields.get('/Type') === '/XObject' &&
      fields.get('/Subtype') === '/Image' &&
      fields.get('/ColorSpace') === '/DeviceRGB' &&
      fields.get('/BitsPerComponent') === '8' &&
      /^[1-9]\d{0,4}$/.test(w) &&
      /^[1-9]\d{0,4}$/.test(h) &&
      /^[1-9]\d{0,7}$/.test(raw),
  )
  const length = Number(raw)
  profile(Number(w) <= 65535 && Number(h) <= 65535 && Number(w) * Number(h) * 3 === length)
  return { pixels: true, length }
}

const row = (pos: number) => `${String(pos).padStart(10, '0')} 00000 n `

/** Classic LF PDF with a flat page tree and one distinct raw RGB scan drawn per page. */
export async function openStreamPdf(
  readBase: AsyncIterable<Uint8Array>,
  baseLength: number,
): Promise<{
  pageCount: number
  produce(state: EditSnapshot): Promise<Part[]>
}> {
  profile(Number.isSafeInteger(baseLength) && baseLength > 0 && baseLength <= MAX_FILE)
  const source = new Source(readBase, baseLength)
  let header = await source.line()
  profile(/^%PDF-1\.[0-7]$/.test(header))
  header += '\n'
  const objects: ScanObject[] = []
  let xrefGap = ''
  let xref: number
  for (;;) {
    let gaps = 0
    let line = await source.line()
    if (!objects.length && header.indexOf('\n%') === -1 && /^%[\x80-\xff]{4}$/.test(line)) {
      header += `${line}\n`
      line = await source.line()
    }
    while (line === '') {
      profile(++gaps <= MAX_LINE)
      line = await source.line()
    }
    const offset = source.pos - line.length - 1
    const gap = '\n'.repeat(gaps)
    if (line === 'xref') {
      xref = offset
      xrefGap = gap
      break
    }
    profile(objects.length < MAX_PAGES * 3 + 3 && line === `${objects.length + 1} 0 obj`)
    const { body, next } = await source.dictionary()
    const obj: ScanObject = { offset, gap, body }
    line = next
    if (line === 'stream') {
      const { pixels, length } = streamLength(body)
      if (pixels) {
        obj.payload = { kind: 'pixels', span: { offset: source.pos, length } }
        await source.bytes(length, true)
      } else {
        const text = await source.bytes(length, false)
        profile(!/[^\x20-\x7e\t\r\n]/.test(text))
        obj.payload = { kind: 'paint', text }
      }
      profile((await source.line()) === '' && (await source.line()) === 'endstream')
      line = await source.line()
    }
    profile(line === 'endobj')
    objects.push(obj)
  }
  const size = objects.length + 1
  profile((await source.line()) === `0 ${size}` && (await source.line()) === '0000000000 65535 f ')
  for (const obj of objects) profile((await source.line()) === row(obj.offset))
  let beforeTrailer = await source.line()
  const afterRows = beforeTrailer === '' ? '\n' : ''
  if (afterRows) beforeTrailer = await source.line()
  profile(beforeTrailer === 'trailer')
  const { body: trailer, next: startKeyword } = await source.dictionary()
  const t = dict(dictionary(trailer), '/Size', '/Root', '/Info')
  profile(t.get('/Size') === String(size))
  const root = ref(t.get('/Root'))
  const info = t.has('/Info') ? ref(t.get('/Info')) : undefined
  profile(startKeyword === 'startxref' && (await source.line()) === String(xref))
  const tailSize = baseLength - source.pos
  profile(tailSize === 5 || tailSize === 6)
  const eof = await source.bytes(tailSize, false)
  profile(eof === '%%EOF' || eof === '%%EOF\n')
  await source.finish()

  const seen = new Set<number>()
  function claim(id: number, kind?: Payload['kind']) {
    const obj = objects[id - 1]
    profile(obj && !seen.has(id) && obj.payload?.kind === kind)
    seen.add(id)
    return obj
  }
  const catalog = dict(dictionary(claim(root).body), '/Type', '/Pages')
  profile(catalog.get('/Type') === '/Catalog')
  const treeId = ref(catalog.get('/Pages'))
  const tree = dict(dictionary(claim(treeId).body), '/Type', '/Count', '/Kids')
  const kids = array(tree.get('/Kids'), 1, MAX_PAGES).map(ref)
  profile(tree.get('/Type') === '/Pages' && tree.get('/Count') === String(kids.length))
  const pages = kids.map((id) => {
    const page = dict(
      dictionary(claim(id).body),
      '/Type',
      '/Parent',
      '/MediaBox',
      '/Rotate',
      '/Resources',
      '/Contents',
      '/Annots',
    )
    if (page.has('/Annots')) array(page.get('/Annots'), 0, 0)
    profile(page.get('/Type') === '/Page' && ref(page.get('/Parent')) === treeId)
    const box = array(page.get('/MediaBox'), 4, 4)
    profile(
      box[0] === '0' && box[1] === '0' && num(box[2], 0, 14400) > 0 && num(box[3], 0, 14400) > 0,
    )
    const rotation = page.get('/Rotate') ?? '0'
    profile(rotation === '0' || rotation === '90' || rotation === '180' || rotation === '270')
    const resources = dict(page.get('/Resources'), '/XObject', '/Font', '/ExtGState')
    if (resources.has('/Font')) dict(resources.get('/Font'))
    if (resources.has('/ExtGState')) dict(resources.get('/ExtGState'))
    const images = resources.get('/XObject')
    profile(images instanceof Map && images.size === 1)
    const [name, target] = images.entries().next().value!
    const imageId = ref(target)
    claim(imageId, 'pixels')
    const contents = page.get('/Contents')
    const contentId = ref(Array.isArray(contents) ? array(contents, 1, 1)[0] : contents)
    const paint = claim(contentId, 'paint').payload
    profile(
      paint?.kind === 'paint' &&
        compact(paint.text) === `q ${box[2]} 0 0 ${box[3]} 0 0 cm ${name} Do Q`,
    )
    return {
      id,
      rotation: Number(rotation),
      width: box[2],
      height: box[3],
      name,
      imageId,
      contentId,
    }
  })
  const originalInfo = info === undefined ? new Map<string, string>() : infoFields(claim(info).body)
  profile(seen.size === objects.length)

  return {
    pageCount: pages.length,
    async produce(state) {
      profile(
        !state.markups.length &&
          !state.annotDeletes.length &&
          !state.noteEdits.length &&
          !state.drawings.length &&
          !state.textEdits.length &&
          !state.textInserts.length &&
          !state.imageEdits.length &&
          !state.stampCfg &&
          !state.formEdits.size &&
          !state.formFields.length &&
          !state.formWidgetEdits.size &&
          !state.blankPages.length &&
          !state.deleted.size &&
          !state.order,
      )
      const { rotations, metadata } = state
      const edits = new Map<number, string>()
      for (const [index, delta] of rotations) {
        const page = pages[index]
        profile(page && Number.isInteger(delta) && delta % 90 === 0)
        const angle = (((page.rotation + delta) % 360) + 360) % 360
        if (angle === page.rotation) continue
        edits.set(
          page.id,
          `<< /Type /Page /Parent ${treeId} 0 R /MediaBox [ 0 0 ${page.width} ${page.height} ] /Rotate ${angle} /Resources << /XObject << ${page.name} ${page.imageId} 0 R >> >> /Contents ${page.contentId} 0 R >>`,
        )
      }
      let infoId = info
      if (metadata) {
        const fields = new Map(originalInfo)
        for (const [name, key] of [
          ['Title', 'title'],
          ['Subject', 'subject'],
          ['Author', 'author'],
          ['Keywords', 'keywords'],
        ] as const) {
          let value = metadata[key]
          if (value === undefined) continue
          profile(typeof value === 'string' && value.length <= 1023)
          if (key === 'keywords')
            value = value
              .split(/[,，;；]/)
              .map((s) => s.trim())
              .filter(Boolean)
              .join(' ')
          fields.set(name, PDFHexString.fromText(value).toString())
        }
        if (fields.size) {
          infoId ??= size
          const body = `<< ${Array.from(fields, ([k, v]) => `/${k} ${v}`).join(' ')} >>`
          profile(body.length <= MAX_LINE)
          edits.set(infoId, body)
        }
      }
      const parts: Part[] = []
      const literals: string[] = []
      const offsets: number[] = []
      let pos = 0,
        syntax = 0
      function emit(text: string) {
        syntax += text.length
        pos += text.length
        profile(pos <= MAX_FILE && syntax <= MAX_SYNTAX)
        literals.push(text)
      }
      function flush() {
        if (!literals.length) return
        parts.push({ data: Buffer.from(literals.join(''), 'latin1').toString('base64') })
        literals.length = 0
      }
      emit(header)
      for (const [i, obj] of objects.entries()) {
        emit(obj.gap)
        offsets.push(pos)
        emit(`${i + 1} 0 obj\n${edits.get(i + 1) ?? obj.body}\n`)
        if (obj.payload) {
          emit('stream\n')
          if (obj.payload.kind === 'pixels') {
            flush()
            parts.push(obj.payload.span)
            pos += obj.payload.span.length
          } else emit(obj.payload.text)
          emit('\nendstream\n')
        }
        emit('endobj\n')
      }
      if (infoId === size) {
        offsets.push(pos)
        emit(`${size} 0 obj\n${edits.get(size)}\nendobj\n`)
      }
      emit(xrefGap)
      const start = pos
      const resultSize = offsets.length + 1
      emit(`xref\n0 ${resultSize}\n0000000000 65535 f \n`)
      for (const offset of offsets) emit(`${row(offset)}\n`)
      const resultTrailer =
        infoId === info
          ? trailer
          : `<< /Size ${resultSize} /Root ${root} 0 R /Info ${infoId} 0 R >>`
      emit(`${afterRows}trailer\n${resultTrailer}\nstartxref\n${start}\n${eof}`)
      flush()
      return parts
    },
  }
}
