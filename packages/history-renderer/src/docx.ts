import { HistoryError, type Json } from '@seuoffice/collab/client'
import JSZip from 'jszip'
import { Schema, Slice, type Node } from 'prosemirror-model'
import { ReplaceStep } from 'prosemirror-transform'
import { SaxesParser, type SaxesTagNS } from 'saxes'
import type { ByteHistoryAdapter } from './adapter'

const WORD = 'http://schemas.openxmlformats.org/wordprocessingml/2006/main'
const OPC = 'http://schemas.openxmlformats.org/package/2006/'
const OFFICE = 'http://schemas.openxmlformats.org/officeDocument/2006/relationships/'
const MAIN = 'word/document.xml'
const TYPES = '[Content_Types].xml'
const RELS = '_rels/.rels'
const MAIN_TYPE = 'application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml'
const PART_LIMIT = 1024 * 1024
const schema = new Schema({
  nodes: {
    doc: { content: 'docParagraph+' },
    docParagraph: { content: 'text*' },
    text: { inline: true },
  },
})

function unsupported(detail: string): never {
  throw new HistoryError('unsupported', detail)
}
function invalid(detail: string): never {
  throw new HistoryError('invalid_history', detail)
}

async function xmlPart(zip: JSZip, name: string): Promise<string> {
  const part = zip.file(name)
  if (!part) return invalid(`missing DOCX part: ${name}`)
  return new TextDecoder('utf-8', { fatal: true, ignoreBOM: true }).decode(
    await part.async('uint8array'),
  )
}

function xmlReader() {
  const parser = new SaxesParser({ xmlns: true })
  parser.on('xmldecl', ({ version, encoding }) => {
    if (version !== '1.0' || (encoding && !/^utf-8$/i.test(encoding)))
      unsupported('DOCX XML encoding')
  })
  const otherMarkup = () => unsupported('unsupported DOCX XML markup')
  for (const event of ['doctype', 'processinginstruction', 'cdata', 'comment'] as const)
    parser.on(event, otherMarkup)
  return parser
}

function manifest(xml: string, root: string, uri: string, child: (tag: SaxesTagNS) => void): void {
  const parser = xmlReader()
  let depth = 0
  parser.on('opentag', (tag) => {
    if (tag.uri !== uri || (depth === 0 && tag.local !== root) || depth > 1)
      invalid('bad DOCX package manifest')
    if (depth++ === 1) child(tag)
  })
  parser.on('closetag', () => {
    depth--
  })
  parser.on('text', (s) => {
    if (/[^\t\n\r ]/.test(s)) invalid('text in DOCX manifest')
  })
  parser.write(xml).close()
}

const properties: Readonly<Record<string, string>> = {
  'docProps/core.xml': OPC + 'relationships/metadata/core-properties',
  'docProps/app.xml': OFFICE + 'extended-properties',
  'docProps/custom.xml': OFFICE + 'custom-properties',
}

async function checkPackage(zip: JSZip): Promise<void> {
  // Styles/settings can change even a bare paragraph's editor node or hide its text.
  for (const entry of Object.values(zip.files)) {
    if (
      !entry.dir &&
      ![MAIN, TYPES, RELS].includes(entry.name) &&
      !Object.hasOwn(properties, entry.name)
    )
      unsupported('DOCX parts outside the plain document profile')
  }
  const types = new Map<string, string>()
  manifest(
    await xmlPart(zip, TYPES),
    'Types',
    OPC + 'content-types',
    ({ local, attributes: a }) => {
      const key =
        local === 'Override'
          ? a.PartName?.value
          : local === 'Default'
            ? a.Extension?.value
            : undefined
      if (!key || !a.ContentType?.value || types.has(key.toLowerCase()))
        invalid('ambiguous DOCX content type')
      if (
        local === 'Default'
          ? !/^[A-Za-z0-9]+$/.test(key)
          : !key.startsWith('/') || !zip.file(key.slice(1))
      )
        invalid('invalid DOCX content type target')
      types.set(key.toLowerCase(), a.ContentType.value)
    },
  )
  if (types.get('/' + MAIN) !== MAIN_TYPE) unsupported('not a standard DOCX main part')
  if (
    (types.get('/' + RELS) ?? types.get('rels')) !==
    'application/vnd.openxmlformats-package.relationships+xml'
  )
    invalid('invalid DOCX relationship content type')
  let office = false
  const ids = new Set<string>()
  manifest(
    await xmlPart(zip, RELS),
    'Relationships',
    OPC + 'relationships',
    ({ local, attributes: a }) => {
      if (
        local !== 'Relationship' ||
        !a.Id?.value ||
        ids.has(a.Id.value) ||
        !a.Type?.value ||
        !a.Target?.value
      )
        invalid('invalid DOCX package relationship')
      ids.add(a.Id.value)
      const target = a.Target.value.replace(/^\//, '')
      if (a.TargetMode && a.TargetMode.value !== 'Internal')
        unsupported('external DOCX package relationship')
      if (a.Type.value === OFFICE + 'officeDocument') {
        if (office || target !== MAIN) unsupported('ambiguous DOCX main part')
        office = true
      } else if (!Object.hasOwn(properties, target) || properties[target] !== a.Type.value) {
        unsupported('unsupported DOCX package relationship')
      }
      if (!zip.file(target)) invalid('missing DOCX relationship target')
    },
  )
  if (!office) invalid('missing DOCX main relationship')
}

interface Paragraph {
  start: number
  end: number
  text: string
}

// XML 1.0 scalars only. Docs normalizes line breaks and may decode a literal numeric reference again.
function plainText(s: string): boolean {
  return !/[^\u0020-\uD7FF\uE000-\uFFFD\u{10000}-\u{10FFFF}]|&#/u.test(s)
}

function paragraphs(xml: string): Paragraph[] {
  const parser = xmlReader()
  const stack: string[] = []
  const result: Paragraph[] = []
  let paragraph: Paragraph | undefined
  let hasBody = false
  let hasSection = false
  let preserve = false
  parser.on('opentag', (tag) => {
    const parent = stack.at(-1)
    const name = tag.local
    if (tag.prefix !== 'w' || tag.uri !== WORD) unsupported('DOCX body namespace')
    const attrs = Object.keys(tag.attributes)
    if (!parent && name === 'document') {
      if (attrs.some((k) => !k.startsWith('xmlns:'))) unsupported('DOCX document attributes')
    } else if (parent === 'document' && name === 'body' && !hasBody && !attrs.length) {
      hasBody = true
    } else if (parent === 'body' && name === 'p' && !hasSection && !attrs.length) {
      if (result.length >= 4096) unsupported('too many DOCX paragraphs')
      paragraph = { start: xml.lastIndexOf('<', parser.position - 1), end: 0, text: '' }
      result.push(paragraph)
    } else if (parent === 'p' && name === 'r' && !attrs.length) {
      // Bare runs join into one unmarked ProseMirror text node.
    } else if (parent === 'r' && name === 't' && attrs.every((k) => k === 'xml:space')) {
      const space = tag.attributes['xml:space']?.value
      if (space !== undefined && space !== 'preserve' && space !== 'default')
        invalid('invalid xml:space')
      preserve = space === 'preserve'
    } else if (parent === 'body' && name === 'sectPr' && !hasSection && !attrs.length) {
      hasSection = true
    } else if (!(
      parent === 'sectPr' &&
      (name === 'pgSz' || name === 'pgMar') &&
      attrs.every((k) => k.startsWith('w:'))
    )) {
      unsupported('DOCX body is not plain paragraphs')
    }
    stack.push(name)
  })
  parser.on('text', (s) => {
    if (stack.at(-1) === 't' && paragraph) {
      if (!plainText(s)) unsupported('nonliteral DOCX text')
      // build_run.rs uses wt_text: without xml:space, leading and trailing ASCII spaces disappear.
      let left = 0,
        right = s.length
      if (!preserve) {
        while (left < right && s[left] === ' ') left++
        while (right > left && s[right - 1] === ' ') right--
      }
      paragraph.text += s.slice(left, right)
    } else if (/[^\t\n\r ]/.test(s)) invalid('text outside a DOCX run')
  })
  parser.on('closetag', () => {
    if (stack.pop() === 'p' && paragraph) {
      paragraph.end = parser.position
      paragraph = undefined
    }
  })
  parser.write(xml).close()
  if (!result.length) unsupported('no plain DOCX paragraph')
  return result
}

function object(value: Json | undefined): value is Record<string, Json> {
  return !!value && typeof value === 'object' && !Array.isArray(value)
}

function fields(value: Record<string, Json>, allowed: readonly string[]): void {
  if (Object.keys(value).some((key) => !allowed.includes(key)))
    unsupported('unknown DOCX wire field')
}

function decode(json: Json): ReplaceStep {
  if (!object(json) || typeof json.kind !== 'string') return invalid('not a DOCX op')
  if (json.kind !== 'steps') return unsupported('not a DOCX body edit')
  fields(json, ['kind', 'steps'])
  if (!Array.isArray(json.steps) || !json.steps.length) return invalid('missing DOCX steps')
  if (json.steps.length !== 1) return unsupported('compound DOCX edit')
  const step = json.steps[0]
  if (!object(step) || typeof step.stepType !== 'string') return invalid('missing DOCX step type')
  if (step.stepType !== 'replace') return unsupported('not a plain text replace')
  fields(step, ['stepType', 'from', 'to', 'slice', 'structure'])
  if (step.structure !== undefined && typeof step.structure !== 'boolean')
    return invalid('invalid DOCX structure flag')
  if (step.structure) return unsupported('structural DOCX replace')
  if (
    typeof step.from !== 'number' ||
    typeof step.to !== 'number' ||
    !Number.isSafeInteger(step.from) ||
    !Number.isSafeInteger(step.to) ||
    step.from < 0 ||
    step.to < step.from
  )
    return invalid('invalid DOCX replace range')
  const slice = step.slice
  if (slice !== undefined) {
    if (!object(slice) || !Array.isArray(slice.content)) return invalid('invalid DOCX slice')
    fields(slice, ['content', 'openStart', 'openEnd'])
    for (const open of [slice.openStart, slice.openEnd]) {
      if (open === undefined) continue
      if (typeof open !== 'number' || !Number.isSafeInteger(open) || open < 0)
        return invalid('invalid DOCX slice depth')
      if (open !== 0) return unsupported('open DOCX text slice')
    }
    if (slice.content.length !== 1) return unsupported('not a single DOCX text node')
    const node = slice.content[0]
    if (!object(node) || typeof node.type !== 'string') return invalid('invalid DOCX node')
    if (node.type !== 'text') return unsupported('not unmarked DOCX text')
    fields(node, ['type', 'text'])
    if (typeof node.text !== 'string' || !node.text) return invalid('empty DOCX text node')
    if (!plainText(node.text)) return unsupported('text has no literal DOCX representation')
  }
  return new ReplaceStep(step.from, step.to, Slice.fromJSON(schema, slice))
}

function checkRange(doc: Node, step: ReplaceStep): void {
  if (step.to > doc.content.size) invalid('DOCX replace past end of document')
  const from = doc.resolve(step.from)
  const to = doc.resolve(step.to)
  if (!from.parent.isTextblock || !from.sameParent(to))
    unsupported('DOCX replace crosses paragraph structure')
  for (const pos of [from, to]) {
    if (
      pos.textOffset &&
      /^[\uDC00-\uDFFF]$/.test(pos.parent.child(pos.index()).text!.charAt(pos.textOffset))
    )
      unsupported('DOCX replace splits a Unicode scalar')
  }
}

/** DOCX/schema 4. Shared ZIP admission must precede this reader. */
export async function openDocx(bytes: Uint8Array): Promise<ByteHistoryAdapter<ReplaceStep>> {
  const zip = await JSZip.loadAsync(bytes)
  await checkPackage(zip)
  const xml = await xmlPart(zip, MAIN)
  const source = paragraphs(xml)
  const base = schema.node(
    'doc',
    null,
    source.map(({ text }) =>
      schema.node('docParagraph', null, text ? schema.text(text) : undefined),
    ),
  )
  let doc = base
  return {
    decode,
    encode: (step) => ({ kind: 'steps', steps: [step.toJSON() as Json] }),
    integrate(remote, local) {
      if (local.length) invalid('DOCX history has pending ops')
      for (const step of remote) {
        checkRange(doc, step)
        if (doc.content.size - (step.to - step.from) + step.slice.size > PART_LIMIT)
          unsupported('DOCX text exceeds part limit')
        const result = step.apply(doc)
        if (!result.doc) return invalid('DOCX replace failed')
        doc = result.doc
      }
      return []
    },
    captureUndo(step) {
      checkRange(doc, step)
      const inverse = step.invert(doc)
      if (inverse.slice.eq(step.slice)) return unsupported('no DOCX text change to undo')
      return (later) => {
        let mapped = inverse
        for (const edit of later) {
          // The browser protects the gap left by a deletion, but permits typing beside an insertion.
          const touches =
            mapped.from === mapped.to
              ? edit.from <= mapped.to && edit.to >= mapped.from
              : edit.from < mapped.to && edit.to > mapped.from
          if (touches) return null
          const next = mapped.map(edit.getMap())
          if (!next) return null
          mapped = next
        }
        return [mapped]
      }
    },
    async produceFile() {
      const pieces: string[] = []
      let end = 0
      doc.forEach((node, _offset, index) => {
        if (node.eq(base.child(index))) return
        const text = node.textContent
        if (!plainText(text)) unsupported('DOCX text would change when reopened')
        const part = source[index]!
        const escaped = text.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
        pieces.push(
          xml.slice(end, part.start),
          `<w:p><w:r><w:t xml:space="preserve">${escaped}</w:t></w:r></w:p>`,
        )
        end = part.end
      })
      const fileName = 'history.docx'
      if (!pieces.length) return { bytes, fileName }
      pieces.push(xml.slice(end))
      const data = new TextEncoder().encode(pieces.join(''))
      if (data.length > PART_LIMIT) unsupported('DOCX XML exceeds part limit')
      zip.file(MAIN, data, { date: zip.file(MAIN)!.date, createFolders: false })
      return {
        fileName,
        bytes: await zip.generateAsync({ type: 'uint8array', compression: 'DEFLATE' }),
      }
    },
  }
}
