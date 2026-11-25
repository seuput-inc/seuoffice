import { HistoryError, type Json } from '@seuoffice/collab/client'
import { mergeRPrModel, parseDocx, type Run } from '@seuoffice/docx-engine'
import JSZip from 'jszip'
import { Schema, Slice, type Mark, type Node } from 'prosemirror-model'
import { AddMarkStep, Mapping, RemoveMarkStep, ReplaceStep, type Step } from 'prosemirror-transform'
import { SaxesParser, type SaxesTagNS } from 'saxes'
import { textHasComplexScript } from '../../docx-engine/src/xml-utils'
import type { ByteHistoryAdapter } from './adapter'

const WORD = 'http://schemas.openxmlformats.org/wordprocessingml/2006/main'
const OPC = 'http://schemas.openxmlformats.org/package/2006/'
const OFFICE = 'http://schemas.openxmlformats.org/officeDocument/2006/relationships/'
const MAIN = 'word/document.xml'
const TYPES = '[Content_Types].xml'
const RELS = '_rels/.rels'
const MAIN_TYPE = 'application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml'
const PART_LIMIT = 1024 * 1024
const toggles = ['bold', 'italic', 'strike', 'underline'] as const
const fontName = /^[\p{L}\p{N}][\p{L}\p{M}\p{N} ._()-]{0,127}$/u
const styleStrings: Readonly<Record<string, RegExp>> = {
  color: /^(?:[0-9a-f]{6}|auto)$/i,
  font: fontName,
  fontAscii: fontName,
  eastAsiaFont: fontName,
  highlight:
    /^(?:black|blue|cyan|green|magenta|red|yellow|white|darkBlue|darkCyan|darkGreen|darkMagenta|darkRed|darkYellow|darkGray|lightGray|none)$/,
  shading: /^[0-9a-f]{6}$/i,
  vertAlign: /^(?:superscript|subscript)$/,
  em: /^(?:dot|comma|circle|underDot)$/,
  caps: /^(?:all|small|none)$/,
}
const styleFields = [
  'rawRPr',
  'color',
  'sizeHalfPoints',
  'font',
  'fontAscii',
  'eastAsiaFont',
  'highlight',
  'shading',
  'vertAlign',
  'em',
  'caps',
  'dstrike',
  'positionHalfPoints',
  'charSpacingTwips',
] as const
const otherStyleFields = new Set(
  (
    'eaSlotEmpty csFont charScaleEm charScaleX charScalePct kern kernHalfPoints shadingDisplay ' +
    'textOutline textEffect glow bdr boldOff italicOff vanish vanishOwn eaLang cs rtl styleId themeRFonts themeColor ' +
    'csSize csBold csItalic tblRun'
  ).split(' '),
)
type TextStep = ReplaceStep | AddMarkStep | RemoveMarkStep
const paragraphSpec = { group: 'body', content: 'text*', attrs: { docxIndex: { default: null } } }
const schema = new Schema({
  nodes: {
    doc: { content: 'body+' },
    docParagraph: paragraphSpec,
    docHeading: paragraphSpec,
    docListItem: paragraphSpec,
    text: { inline: true },
  },
  marks: {
    bold: {},
    italic: {},
    strike: {},
    underline: {},
    docTextStyle: {
      attrs: Object.fromEntries(styleFields.map((name) => [name, { default: null }])),
    },
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

const documentRels = 'word/_rels/document.xml.rels'
// The only parts the engine reads besides the body: they decide run properties and block types.
const wordParts: Readonly<Record<string, string>> = {
  'word/styles.xml': 'styles',
  'word/numbering.xml': 'numbering',
}

/**
 * Check the package entries this reader depends on. Every other part and relationship
 * (settings, theme, fonts, custom XML, thumbnails, headers, external links...) is never
 * read or written: the output ZIP carries it unchanged.
 */
async function checkPackage(zip: JSZip): Promise<void> {
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
      types.set(key.toLowerCase(), a.ContentType.value)
    },
  )
  if (types.get('/' + MAIN) !== MAIN_TYPE) unsupported('not a standard DOCX main part')
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
      if (a.Type.value !== OFFICE + 'officeDocument') return
      if (
        office ||
        (a.TargetMode && a.TargetMode.value !== 'Internal') ||
        a.Target.value.replace(/^\//, '') !== MAIN
      )
        unsupported('DOCX main part is not word/document.xml')
      office = true
    },
  )
  if (!office) invalid('missing DOCX main relationship')
  const bound = new Set<string>()
  if (zip.file(documentRels)) {
    ids.clear()
    manifest(
      await xmlPart(zip, documentRels),
      'Relationships',
      OPC + 'relationships',
      ({ local, attributes: a }) => {
        if (
          local !== 'Relationship' ||
          !a.Id?.value ||
          ids.has(a.Id.value) ||
          !a.Target?.value ||
          !a.Type?.value
        )
          invalid('invalid DOCX document relationship')
        ids.add(a.Id.value)
        const kind = Object.values(wordParts).find((k) => a.Type!.value === OFFICE + k)
        if (!kind) return
        const path = a.Target.value.startsWith('/')
          ? a.Target.value.slice(1)
          : 'word/' + a.Target.value
        if ((a.TargetMode && a.TargetMode.value !== 'Internal') || wordParts[path] !== kind)
          unsupported(`DOCX ${kind} part is not word/${kind}.xml`)
        if (
          !zip.file(path) ||
          bound.has(path) ||
          types.get('/' + path) !==
            `application/vnd.openxmlformats-officedocument.wordprocessingml.${kind}+xml`
        )
          invalid('invalid DOCX document part')
        bound.add(path)
      },
    )
  }
  for (const name of Object.keys(wordParts)) {
    if (zip.file(name) && !bound.has(name)) invalid('unbound DOCX document part')
  }
}

// Only the body, styles and numbering reach the engine; media and every other part do not.
async function parseBody(zip: JSZip, body: string) {
  const input = new JSZip()
  input.file(MAIN, body)
  const ns = 'application/vnd.openxmlformats-'
  const relType = ns + 'package.relationships+xml'
  const types = [
    `<Default Extension="rels" ContentType="${relType}"/>`,
    `<Override PartName="/${MAIN}" ContentType="${MAIN_TYPE}"/>`,
  ]
  const relations: string[] = []
  for (const [path, root] of Object.entries(wordParts)) {
    if (!zip.file(path)) continue
    const xml = await xmlPart(zip, path)
    const parser = xmlReader()
    let depth = 0
    parser.on('opentag', (tag) => {
      if (depth === 0 && (tag.uri !== WORD || tag.local !== root))
        unsupported(`DOCX ${root} part has another root`)
      if (++depth > 32) unsupported(`DOCX ${root} part nests deeper than 32`)
      // A picture bullet names media this reader does not load.
      if (tag.uri === WORD && tag.local === 'numPicBullet')
        unsupported('DOCX numbering has picture bullets')
    })
    parser.on('closetag', () => {
      depth--
    })
    parser.write(xml).close()
    input.file(path, xml)
    types.push(
      `<Override PartName="/${path}" ContentType="${ns}officedocument.wordprocessingml.${root}+xml"/>`,
    )
    relations.push(`<Relationship Id="${root}" Type="${OFFICE}${root}" Target="${path.slice(5)}"/>`)
  }
  input.file(TYPES, `<Types xmlns="${OPC}content-types">${types.join('')}</Types>`)
  input.file(
    RELS,
    `<Relationships xmlns="${OPC}relationships"><Relationship Id="main" Type="${OFFICE}officeDocument" Target="${MAIN}"/></Relationships>`,
  )
  input.file(
    documentRels,
    `<Relationships xmlns="${OPC}relationships">${relations.join('')}</Relationships>`,
  )
  return parseDocx(await input.generateAsync({ type: 'uint8array', compression: 'STORE' }), {
    expandAltChunks: false,
  })
}

interface Paragraph {
  start: number
  end: number
  prefixEnd: number
  text: string
  /** collapsed bookmark pairs, written at the start of the paragraph when it is rewritten */
  anchors: string
}

// XML 1.0 scalars only. Docs normalizes line breaks and may decode a literal numeric reference again.
function plainText(s: string): boolean {
  return !/[^\u0020-\uD7FF\uE000-\uFFFD\u{10000}-\u{10FFFF}]|&#/u.test(s)
}

/**
 * Locate the body's top-level paragraphs. The root, the opening tag of each paragraph, its
 * pPr and the trailing sectPr are copied verbatim and never regenerated, so their content is
 * not interpreted here; runs are, because an edited paragraph is written again from them.
 */
function paragraphs(xml: string): Paragraph[] {
  const parser = xmlReader()
  const stack: string[] = []
  const result: Paragraph[] = []
  let paragraph: Paragraph | undefined
  let opaque = 0
  let hasBody = false
  let hasSection = false
  let preserve = false
  let hasRuns = false
  let hasPPr = false
  let hasRPr = false
  let runStart = 0
  let bookmark: { id: string; start: number } | undefined
  // Spell-check marks and the cached page break position: Word recomputes both, so an
  // edited paragraph may be written without them.
  const dropped = new Set(['proofErr', 'lastRenderedPageBreak'])
  parser.on('opentag', (tag) => {
    if (opaque) {
      opaque++
      return
    }
    const parent = stack.at(-1)
    const name = tag.local
    if (tag.uri !== WORD) unsupported(`DOCX body element ${tag.name} has no plain-text position`)
    const attrs = Object.keys(tag.attributes).filter(
      (k) => k !== 'xmlns' && !k.startsWith('xmlns:'),
    )
    const id = tag.attributes['w:id']?.value
    if (bookmark && !(parent === 'p' && name === 'bookmarkEnd' && id === bookmark.id))
      unsupported('DOCX bookmark spans text')
    if (!parent && name === 'document') {
      // mc:Ignorable and other root attributes stay in the copied prefix.
    } else if (parent === 'document' && name === 'body' && !hasBody && !attrs.length) {
      hasBody = true
    } else if (parent === 'body' && name === 'p' && !hasSection) {
      if (result.length >= 4096) unsupported('too many DOCX paragraphs')
      paragraph = {
        start: xml.lastIndexOf('<', parser.position - 1),
        prefixEnd: parser.position,
        end: 0,
        text: '',
        anchors: '',
      }
      hasRuns = hasPPr = false
      result.push(paragraph)
    } else if (parent === 'p' && name === 'pPr' && !hasPPr && !hasRuns) {
      hasPPr = true
      opaque = 1
    } else if (parent === 'body' && name === 'sectPr' && !hasSection) {
      hasSection = true
      opaque = 1
    } else if (
      parent === 'p' &&
      name === 'r' &&
      attrs.every((k) => k === 'w:rsidR' || k === 'w:rsidRPr' || k === 'w:rsidDel')
    ) {
      hasRuns = true
      hasRPr = false
      runStart = paragraph!.text.length
    } else if (
      parent === 'r' &&
      name === 'rPr' &&
      !hasRPr &&
      paragraph!.text.length === runStart &&
      !attrs.length
    ) {
      // The engine keeps the whole rPr in Run.rawRPr; mergeRPrModel writes it back.
      hasRPr = true
      opaque = 1
    } else if (dropped.has(name) && (parent === 'p' || parent === 'r')) {
      opaque = 1
    } else if (parent === 'p' && name === 'bookmarkStart' && id) {
      // Word's _GoBack is such a pair. The editor, too, writes bookmarks at the paragraph start.
      bookmark = { id, start: xml.lastIndexOf('<', parser.position - 1) }
      opaque = 1
    } else if (parent === 'p' && name === 'bookmarkEnd' && bookmark) {
      opaque = 1
    } else if (parent === 'r' && name === 't' && attrs.every((k) => k === 'xml:space')) {
      const space = tag.attributes['xml:space']?.value
      if (space !== undefined && space !== 'preserve' && space !== 'default')
        invalid('invalid xml:space')
      preserve = space === 'preserve'
    } else {
      unsupported(`DOCX ${parent ?? 'part'} element w:${name} is outside plain paragraph text`)
    }
    stack.push(name)
  })
  parser.on('text', (s) => {
    if (opaque) return
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
    if (opaque && --opaque) return
    const name = stack.pop()
    if (name === 'r' && hasRPr && paragraph!.text.length === runStart)
      unsupported('empty styled DOCX run')
    if (name === 'pPr') paragraph!.prefixEnd = parser.position
    if (name === 'bookmarkEnd') {
      paragraph!.anchors += xml.slice(bookmark!.start, parser.position)
      bookmark = undefined
    }
    if (name === 'p' && paragraph) {
      if (bookmark) unsupported('DOCX bookmark spans paragraphs')
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

function readMark(json: Json | undefined, rawStyles: ReadonlySet<string>): Mark {
  if (!object(json) || typeof json.type !== 'string') return invalid('invalid DOCX mark')
  if (!Object.hasOwn(schema.marks, json.type)) return unsupported('unknown DOCX mark')
  fields(json, ['type', 'attrs'])
  const attrs = json.attrs === undefined ? {} : json.attrs
  if (!object(attrs)) return invalid('invalid DOCX mark attributes')
  if (json.type !== 'docTextStyle') fields(attrs, [])
  else {
    for (const [key, value] of Object.entries(attrs)) {
      if (key === 'rawRPr') {
        if (value !== null && (typeof value !== 'string' || !rawStyles.has(value)))
          unsupported('DOCX raw run properties not present in base')
      } else if (Object.hasOwn(styleStrings, key)) {
        if (value !== null && (typeof value !== 'string' || !styleStrings[key]!.test(value)))
          unsupported('unsupported DOCX character style')
      } else if (key === 'sizeHalfPoints') {
        if (
          value !== null &&
          (typeof value !== 'number' || !Number.isInteger(value) || value < 2 || value > 3276)
        )
          unsupported('unsupported DOCX font size')
      } else if (key === 'positionHalfPoints' || key === 'charSpacingTwips') {
        if (
          value !== null &&
          (typeof value !== 'number' || !Number.isInteger(value) || Math.abs(value) > 31680)
        )
          unsupported('unsupported DOCX character spacing')
      } else if (key === 'dstrike') {
        if (value !== null && typeof value !== 'boolean')
          unsupported('unsupported DOCX double strike')
      } else if (value !== null || !otherStyleFields.has(key))
        unsupported('unsupported DOCX text style')
    }
    if (!styleFields.some((key) => attrs[key] != null)) unsupported('empty DOCX text style')
  }
  return schema.mark(json.type, attrs)
}

function decode(json: Json, rawStyles: ReadonlySet<string>): TextStep[] {
  if (!object(json) || typeof json.kind !== 'string') return invalid('not a DOCX op')
  if (json.kind !== 'steps') return unsupported('not a DOCX body edit')
  fields(json, ['kind', 'steps'])
  if (!Array.isArray(json.steps) || !json.steps.length) return invalid('missing DOCX steps')
  return json.steps.map((step) => readStep(step, rawStyles))
}

function readStep(step: Json, rawStyles: ReadonlySet<string>): TextStep {
  if (!object(step) || typeof step.stepType !== 'string') return invalid('missing DOCX step type')
  if (!['replace', 'addMark', 'removeMark'].includes(step.stepType))
    return unsupported('not a DOCX text edit')
  if (
    typeof step.from !== 'number' ||
    typeof step.to !== 'number' ||
    !Number.isSafeInteger(step.from) ||
    !Number.isSafeInteger(step.to) ||
    step.from < 0 ||
    step.to < step.from
  )
    return invalid('invalid DOCX replace range')
  if (step.stepType !== 'replace') {
    fields(step, ['stepType', 'from', 'to', 'mark'])
    const mark = readMark(step.mark, rawStyles)
    return step.stepType === 'addMark'
      ? new AddMarkStep(step.from, step.to, mark)
      : new RemoveMarkStep(step.from, step.to, mark)
  }
  fields(step, ['stepType', 'from', 'to', 'slice', 'structure'])
  if (step.structure !== undefined && typeof step.structure !== 'boolean')
    return invalid('invalid DOCX structure flag')
  if (step.structure) return unsupported('structural DOCX replace')
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
    for (const node of slice.content) {
      if (!object(node) || typeof node.type !== 'string') return invalid('invalid DOCX node')
      if (node.type !== 'text') return unsupported('not DOCX text')
      fields(node, ['type', 'text', 'marks'])
      if (typeof node.text !== 'string' || !node.text) return invalid('empty DOCX text node')
      if (!plainText(node.text)) return unsupported('text has no literal DOCX representation')
      if (node.marks !== undefined) {
        if (!Array.isArray(node.marks)) return invalid('invalid DOCX marks')
        const seen = new Set<string>()
        for (const wire of node.marks) {
          const mark = readMark(wire, rawStyles)
          if (seen.has(mark.type.name)) return invalid('duplicate DOCX mark')
          seen.add(mark.type.name)
        }
      }
    }
  }
  return new ReplaceStep(step.from, step.to, Slice.fromJSON(schema, slice))
}

function checkRange(doc: Node, step: TextStep): void {
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

function textStep(step: Step): TextStep {
  if (step instanceof AddMarkStep || step instanceof RemoveMarkStep || step instanceof ReplaceStep)
    return step
  return unsupported('DOCX inverse is not a text step')
}

function apply(doc: Node, step: TextStep): Node {
  checkRange(doc, step)
  if (
    step instanceof ReplaceStep &&
    doc.content.size - (step.to - step.from) + step.slice.size > PART_LIMIT
  )
    unsupported('DOCX text exceeds part limit')
  return step.apply(doc).doc ?? invalid('DOCX step failed')
}

const runFields = new Set<string>([
  'text',
  'effective',
  'fontCs',
  'csFont',
  ...styleFields,
  ...toggles,
])

function textNode(run: Run, rawStyles: Set<string>): Node {
  if (
    !run.text ||
    !plainText(run.text) ||
    Object.keys(run).some((key) => !runFields.has(key)) ||
    run.effective?.cs ||
    (run.csFont !== undefined && textHasComplexScript(run.text)) ||
    run.effective?.styleXor?.length ||
    toggles.some((key) => run[key] === false)
  )
    unsupported('DOCX run has no lossless text projection')
  const marks = toggles.filter((key) => run[key]).map((key) => schema.mark(key))
  if (styleFields.some((key) => run[key] !== undefined)) {
    if (run.rawRPr) rawStyles.add(run.rawRPr)
    const attrs = Object.fromEntries(styleFields.map((key) => [key, run[key] ?? null]))
    marks.push(readMark({ type: 'docTextStyle', attrs }, rawStyles))
  }
  return schema.text(run.text, marks)
}

function runProperties(marks: readonly Mark[]): string {
  if (!marks.length) return ''
  const run: Run = { text: '' }
  for (const mark of marks) {
    if (mark.type.name === 'docTextStyle') {
      for (const key of styleFields) {
        if (mark.attrs[key] != null) Reflect.set(run, key, mark.attrs[key])
      }
    } else {
      for (const key of toggles) if (mark.type.name === key) run[key] = true
    }
  }
  return mergeRPrModel(run.rawRPr ?? '', run, false)
}

async function readBody(zip: JSZip) {
  await checkPackage(zip)
  const xml = await xmlPart(zip, MAIN)
  const source = paragraphs(xml)
  const parsed = await parseBody(
    zip,
    xml.slice(0, source[source.length - 1]!.end) + '</w:body></w:document>',
  )
  const blocks = parsed.blocks.filter((block) => !block.hidden)
  if (source.length !== blocks.length) unsupported('DOCX editor blocks differ from its paragraphs')
  const rawStyles = new Set<string>()
  const base = schema.node(
    'doc',
    null,
    blocks.map((block, index) => {
      const part = source[index]!
      const anchor = parsed.extras.elements[index]
      if (
        block.docxIndex !== index ||
        anchor?.name !== 'w:p' ||
        anchor.start !== part.start ||
        anchor.end !== part.end
      )
        unsupported('ambiguous DOCX paragraph anchor')
      const type =
        block.type === 'paragraph'
          ? 'docParagraph'
          : block.type === 'heading'
            ? 'docHeading'
            : block.type === 'listItem'
              ? 'docListItem'
              : unsupported('DOCX block is not a text block')
      const node = schema.node(
        type,
        { docxIndex: index },
        (block.runs ?? []).map((run) => textNode(run, rawStyles)),
      )
      if (node.textContent !== part.text)
        unsupported('DOCX editor text does not match literal runs')
      return node
    }),
  )
  return { xml, source, base, rawStyles }
}

/** DOCX/schema 4. Shared ZIP admission must precede this reader. */
export async function openDocx(bytes: Uint8Array): Promise<ByteHistoryAdapter<TextStep[]>> {
  const zip = await JSZip.loadAsync(bytes)
  const { xml, source, base, rawStyles } = await readBody(zip)
  let doc = base
  return {
    decode: (json) => decode(json, rawStyles),
    encode: (steps) => ({ kind: 'steps', steps: steps.map((step) => step.toJSON() as Json) }),
    integrate(remote, local) {
      if (local.length) invalid('DOCX history has pending ops')
      for (const steps of remote) for (const step of steps) doc = apply(doc, step)
      return []
    },
    captureUndo(steps) {
      const mapping = new Mapping()
      const inverses: TextStep[] = []
      let left = Infinity
      let right = -Infinity
      let selected = doc
      for (const step of steps) {
        const next = apply(selected, step)
        inverses.push(textStep(step.invert(selected)))
        selected = next
        const map = step.getMap()
        mapping.appendMap(map)
        left = Math.min(step.from, map.map(left, -1))
        right = Math.max(
          step instanceof ReplaceStep ? step.from + step.slice.size : step.to,
          map.map(right, 1),
        )
      }
      let restored = selected
      for (let i = inverses.length - 1; i >= 0; i--) restored = apply(restored, inverses[i]!)
      if (doc.eq(selected) || !doc.eq(restored)) unsupported('DOCX target has no lossless inverse')
      return (later) => {
        for (const edits of later)
          for (const edit of edits) {
            // Match the browser's protected gap for deletions and compound transactions.
            if (
              left === right || inverses.length > 1
                ? edit.from <= right && edit.to >= left
                : edit.from < right && edit.to > left
            )
              return null
            const map = edit.getMap()
            mapping.appendMap(map)
            left = map.map(left, 1)
            right = map.map(right, -1)
          }
        const undo: TextStep[] = []
        for (let i = inverses.length - 1; i >= 0; i--) {
          const mapped = inverses[i]!.map(mapping.slice(i + 1))
          if (!mapped) return null
          undo.push(textStep(mapped))
          mapping.appendMap(mapped.getMap(), i)
        }
        return [undo]
      }
    },
    async produceFile() {
      const pieces: string[] = []
      let end = 0
      let runBytes = 0
      doc.forEach((node, _offset, index) => {
        if (node.eq(base.child(index))) return
        const text = node.textContent
        if (!plainText(text)) unsupported('DOCX text would change when reopened')
        const part = source[index]!
        const prefix = xml.slice(end, part.prefixEnd)
        pieces.push(part.prefixEnd === part.end ? prefix.slice(0, -2) + '>' : prefix, part.anchors)
        node.forEach((leaf) => {
          const escaped = leaf
            .text!.replace(/&/g, '&amp;')
            .replace(/</g, '&lt;')
            .replace(/>/g, '&gt;')
          const run = `<w:r>${runProperties(leaf.marks)}<w:t xml:space="preserve">${escaped}</w:t></w:r>`
          if ((runBytes += run.length) > PART_LIMIT) unsupported('DOCX XML exceeds part limit')
          pieces.push(run)
        })
        pieces.push('</w:p>')
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
