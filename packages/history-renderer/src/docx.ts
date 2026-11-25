import { HistoryError, type Json } from '@seuoffice/collab/client'
import {
  mergeRPrModel,
  parseDocx,
  symbolPuaChar,
  withheldLabelOf,
  type Block,
  type EffectiveRPr,
  type Run,
  type TableCell,
} from '@seuoffice/docx-engine'
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
const flag = (value: Json) => typeof value === 'boolean'
const word = (pattern: RegExp) => (value: Json) => typeof value === 'string' && pattern.test(value)
const jsonObject = (keys: readonly string[], value: (v: Json) => boolean) => (json: Json) => {
  if (typeof json !== 'string' || json.length > 512) return false
  try {
    const parsed: unknown = JSON.parse(json)
    return (
      !!parsed &&
      typeof parsed === 'object' &&
      Object.entries(parsed).every(([k, v]) => keys.includes(k) && value(v as Json))
    )
  } catch {
    return false
  }
}
/**
 * Attributes the editor copies from a run into docTextStyle besides styleFields (convert.ts
 * runMarks). They take part in mark equality; the editor saves all but the display ones
 * (kern, eaSlotEmpty, shadingDisplay, tblRun) back into the run (inlineToRuns).
 */
const carriedFields: Readonly<Record<string, (value: Json) => boolean>> = {
  eaLang: word(/^[A-Za-z]{1,8}(?:-[A-Za-z0-9]{1,8}){0,3}$/),
  styleId: word(/^[\p{L}\p{N}_ .-]{1,253}$/u),
  themeColor: word(/^[A-Za-z0-9]{1,32}$/),
  themeRFonts: jsonObject(['font', 'fontAscii'], (v) => typeof v === 'string' && fontName.test(v)),
  kernHalfPoints: (v) => typeof v === 'number' && Number.isInteger(v) && v >= 0 && v <= 3276,
  vanish: flag,
  vanishOwn: flag,
  rtl: flag,
  kern: flag,
  eaSlotEmpty: flag,
  // An explicit off toggle (w:b w:val="0"): the editor paints it; its rawRPr keeps it.
  boldOff: flag,
  italicOff: flag,
  shadingDisplay: word(/^[0-9a-f]{6}$/i),
  tblRun: jsonObject(
    ['sizeHalfPoints', 'fontAscii', 'caps', 'charSpacingTwips'],
    (v) =>
      (typeof v === 'number' && Number.isInteger(v) && Math.abs(v) <= 31680) ||
      (typeof v === 'string' && (fontName.test(v) || /^(?:all|small|none)$/.test(v))),
  ),
}
const markFields = [...styleFields, ...Object.keys(carriedFields)]
// Editor attributes this reader never carries: their runs are not rewritten, so they stay null.
const otherStyleFields = new Set(
  (
    'csFont charScaleEm charScaleX charScalePct textOutline textEffect glow bdr cs ' +
    'csSize csBold csItalic'
  ).split(' '),
)
type TextStep = ReplaceStep | AddMarkStep | RemoveMarkStep
// The Docs editor's document shape (renderer/editor/extensions.ts), down to what decides positions.
// `source` names the XML paragraph a text block writes back to; null when it cannot be rewritten,
// `locked` then says why and its content is a placeholder of the editor's size.
const paragraphSpec = {
  group: 'block',
  content: 'inline*',
  attrs: { source: { default: null }, locked: { default: null } },
}
const schema = new Schema({
  nodes: {
    doc: { content: 'block+' },
    docParagraph: paragraphSpec,
    docHeading: paragraphSpec,
    docListItem: paragraphSpec,
    docTable: { group: 'block', content: 'docTableRow+' },
    docTableRow: { content: 'docTableCell+' },
    docTableCell: { content: 'block+' },
    // docProtected, docNestedTable and docCellBoxes: atoms the editor sizes as one position
    docAtom: { group: 'block', atom: true },
    text: { group: 'inline', inline: true },
    hardBreak: {
      group: 'inline',
      inline: true,
      attrs: Object.fromEntries(
        ['pageBreak', 'colBreak', 'wrapBreak', 'mathBreak'].map((name) => [
          name,
          { default: false },
        ]),
      ),
    },
  },
  marks: {
    bold: {},
    italic: {},
    strike: {},
    underline: {},
    docTextStyle: {
      attrs: Object.fromEntries(markFields.map((name) => [name, { default: null }])),
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

/**
 * Read the package as the Docs editor does (parseDocx of the whole file), so its blocks, and with
 * them every position, are the editor's. Styles and numbering are checked first: the engine
 * resolves every run through them.
 */
async function parseBody(zip: JSZip, bytes: Uint8Array) {
  for (const [path, root] of Object.entries(wordParts)) {
    if (!zip.file(path)) continue
    const parser = xmlReader()
    let depth = 0
    parser.on('opentag', (tag) => {
      if (depth === 0 && (tag.uri !== WORD || tag.local !== root))
        unsupported(`DOCX ${root} part has another root`)
      if (++depth > 32) unsupported(`DOCX ${root} part nests deeper than 32`)
      // A picture bullet sizes list markers from media.
      if (tag.uri === WORD && tag.local === 'numPicBullet')
        unsupported('DOCX numbering has picture bullets')
    })
    parser.on('closetag', () => {
      depth--
    })
    parser.write(await xmlPart(zip, path)).close()
  }
  // scanBody has refused every w:altChunk; no imported package reaches the engine.
  return parseDocx(bytes, { expandAltChunks: false })
}

interface Paragraph {
  start: number
  end: number
  prefixEnd: number
  /** the text the engine reads: w:t, a tab as \t, a break as \n, \f (page), \v (column) or \u001e */
  text: string
  /** collapsed bookmark pairs, written at the start of the paragraph when it is rewritten */
  anchors: string
  /** why this paragraph cannot be written again from its runs */
  problem?: string
}
interface Cell {
  paragraphs: Paragraph[]
  problem?: string
}
interface BodyXml {
  /** paragraphs of the body and of its tables' cells, by the offset of their opening tag */
  paragraphs: Map<number, Paragraph>
  /** the body's tables, by offset: rows of cells */
  tables: Map<number, Cell[][]>
}

// XML 1.0 scalars, and tabs. Docs normalizes line breaks and may decode a literal numeric reference again.
function plainText(s: string): boolean {
  return !/[^\t\u0020-\uD7FF\uE000-\uFFFD\u{10000}-\u{10FFFF}]|&#/u.test(s)
}

// w:br by its w:type, as the engine reads it; with w:clear="all" a text wrapping break is \u001e
const breaks: Readonly<Record<string, string>> = { page: '\f', column: '\v', textWrapping: '\n' }

/**
 * Locate the paragraphs of the body and of its tables' cells. The root, the opening tag of each
 * paragraph, its pPr, each run's rPr, tables and every other element are copied verbatim and not
 * interpreted here; runs are read because an edited paragraph is written again from them.
 */
function scanBody(xml: string): BodyXml {
  const parser = xmlReader()
  const stack: string[] = []
  const result: BodyXml = { paragraphs: new Map(), tables: new Map() }
  let paragraph: Paragraph | undefined
  let table: Cell[][] | undefined
  let cell: Cell | undefined
  let opaque = 0
  let preserve = false
  let hasRuns = false
  let hasPPr = false
  let hasRPr = false
  let runStart = 0
  let bookmark: { id: string; start: number } | undefined
  // Spell-check marks and the cached page break position: Word recomputes both, so an
  // edited paragraph may be written without them.
  const dropped = new Set(['proofErr', 'lastRenderedPageBreak'])
  const start = () => xml.lastIndexOf('<', parser.position - 1)
  parser.on('opentag', (tag) => {
    // The editor expands an imported chunk into blocks; its package is a ZIP inside the ZIP
    // the archive gate never measured, so it is refused before the engine reads anything.
    if (tag.uri === WORD && tag.local === 'altChunk')
      unsupported('a DOCX body with an imported chunk (w:altChunk)')
    if (opaque) {
      if (++opaque + stack.length > 512) unsupported('DOCX body nests deeper than 512')
      return
    }
    const parent = stack.at(-1)
    const name = tag.uri === WORD ? tag.local : `{${tag.uri}}${tag.local}`
    const attrs = Object.keys(tag.attributes).filter(
      (k) => k !== 'xmlns' && !k.startsWith('xmlns:'),
    )
    stack.push(name)
    const id = tag.attributes['w:id']?.value
    if (!paragraph) {
      if (!parent) {
        if (name !== 'document') unsupported('not a WordprocessingML document')
      } else if (parent === 'document') {
        if (name !== 'body') opaque = 1
      } else if (name === 'p' && (parent === 'body' || (parent === 'tc' && cell))) {
        if (result.paragraphs.size >= 16384) unsupported('too many DOCX paragraphs')
        paragraph = { start: start(), prefixEnd: parser.position, end: 0, text: '', anchors: '' }
        // Rewritten runs are spelled with the w prefix the paragraph's own scope binds.
        if (tag.prefix !== 'w') paragraph.problem = 'a DOCX paragraph without the w prefix'
        hasRuns = hasPPr = false
        result.paragraphs.set(paragraph.start, paragraph)
        cell?.paragraphs.push(paragraph)
      } else if (name === 'tbl' && parent === 'body') {
        table = []
        result.tables.set(start(), table)
      } else if (name === 'tr' && parent === 'tbl' && table && stack.length === 4) {
        table.push([])
      } else if (name === 'tc' && parent === 'tr' && table && stack.length === 5) {
        cell = { paragraphs: [] }
        table.at(-1)!.push(cell)
      } else {
        // A nested table is its own atom in the editor; a content control or other wrapper in a
        // cell holds paragraphs the editor reads where this scan does not look.
        if (parent === 'tc' && cell && name !== 'tcPr' && name !== 'tbl')
          cell.problem ??= `w:${name} in a cell`
        opaque = 1
      }
      return
    }
    const refuse = (problem: string) => {
      paragraph!.problem ??= problem
      opaque = 1
    }
    if (bookmark && !(parent === 'p' && name === 'bookmarkEnd' && id === bookmark.id))
      refuse('a DOCX bookmark spans text')
    else if (parent === 'p' && name === 'pPr' && !hasPPr && !hasRuns) {
      hasPPr = true
      opaque = 1
    } else if (
      parent === 'p' &&
      name === 'r' &&
      attrs.every((k) => k === 'w:rsidR' || k === 'w:rsidRPr' || k === 'w:rsidDel')
    ) {
      hasRuns = true
      hasRPr = false
      runStart = paragraph.text.length
    } else if (
      parent === 'r' &&
      name === 'rPr' &&
      !hasRPr &&
      paragraph.text.length === runStart &&
      !attrs.length
    ) {
      // The engine keeps the whole rPr in Run.rawRPr; mergeRPrModel writes it back.
      hasRPr = true
      opaque = 1
    } else if (dropped.has(name) && (parent === 'p' || parent === 'r')) {
      opaque = 1
    } else if (parent === 'p' && name === 'bookmarkStart' && id) {
      // Word's _GoBack is such a pair. The editor, too, writes bookmarks at the paragraph start.
      bookmark = { id, start: start() }
      opaque = 1
    } else if (parent === 'p' && name === 'bookmarkEnd' && bookmark) {
      opaque = 1
    } else if (parent === 'r' && name === 't' && attrs.every((k) => k === 'xml:space')) {
      const space = tag.attributes['xml:space']?.value
      if (space !== undefined && space !== 'preserve' && space !== 'default')
        invalid('invalid xml:space')
      preserve = space === 'preserve'
    } else if (parent === 'r' && name === 'tab' && !attrs.length) {
      paragraph.text += '\t'
    } else if (parent === 'r' && name === 'cr' && !attrs.length) {
      // A carriage return reads, and is written back, as a line break.
      paragraph.text += '\n'
    } else if (
      parent === 'r' &&
      name === 'br' &&
      attrs.every((k) => k === 'w:type' || k === 'w:clear')
    ) {
      const type = tag.attributes['w:type']?.value ?? 'textWrapping'
      const clear = tag.attributes['w:clear']?.value
      if (clear === undefined && Object.hasOwn(breaks, type)) paragraph.text += breaks[type]
      else if (clear === 'all' && type === 'textWrapping') paragraph.text += '\u001e'
      else refuse('a DOCX break of another kind')
    } else refuse(`w:${name} in a DOCX paragraph`)
  })
  parser.on('text', (s) => {
    if (opaque || !paragraph) return
    if (stack.at(-1) === 't') {
      if (!plainText(s)) paragraph.problem ??= 'nonliteral DOCX text'
      // build_run.rs uses wt_text: without xml:space, leading and trailing ASCII spaces disappear.
      let left = 0,
        right = s.length
      if (!preserve) {
        while (left < right && s[left] === ' ') left++
        while (right > left && s[right - 1] === ' ') right--
      }
      paragraph.text += s.slice(left, right)
    } else if (/[^\t\n\r ]/.test(s)) paragraph.problem ??= 'text outside a DOCX run'
  })
  parser.on('closetag', () => {
    if (opaque && --opaque) return
    const name = stack.pop()
    if (!paragraph) {
      if (name === 'tc') cell = undefined
      else if (name === 'tbl' && stack.length === 2) table = undefined
      return
    }
    if (name === 'r' && hasRPr && paragraph.text.length === runStart)
      paragraph.problem ??= 'an empty styled DOCX run'
    if (name === 'pPr') paragraph.prefixEnd = parser.position
    if (name === 'bookmarkEnd' && bookmark) {
      paragraph.anchors += xml.slice(bookmark.start, parser.position)
      bookmark = undefined
    }
    if (name === 'p') {
      if (bookmark) paragraph.problem ??= 'a DOCX bookmark spans paragraphs'
      bookmark = undefined
      paragraph.end = parser.position
      paragraph = undefined
    }
  })
  parser.write(xml).close()
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
      } else if (Object.hasOwn(carriedFields, key)) {
        if (value !== null && !carriedFields[key]!(value))
          unsupported(`unsupported DOCX text style ${key}`)
      } else if (value !== null || !otherStyleFields.has(key))
        unsupported(`unsupported DOCX text style ${key}`)
    }
    if (!markFields.some((key) => attrs[key] != null)) unsupported('empty DOCX text style')
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
      if (node.type === 'hardBreak') {
        // Shift+Enter, or a page or column break typed into the paragraph.
        fields(node, ['type', 'attrs', 'marks'])
        const attrs = node.attrs ?? {}
        if (!object(attrs)) return invalid('invalid DOCX break')
        fields(attrs, ['pageBreak', 'colBreak', 'wrapBreak', 'mathBreak'])
        if (Object.values(attrs).some((on) => typeof on !== 'boolean'))
          return invalid('invalid DOCX break')
        if (attrs.mathBreak || Object.values(attrs).filter(Boolean).length > 1)
          return unsupported('DOCX break of an equation or of two kinds')
      } else if (node.type === 'text') {
        fields(node, ['type', 'text', 'marks'])
        if (typeof node.text !== 'string' || !node.text) return invalid('empty DOCX text node')
        if (!plainText(node.text)) return unsupported('text has no literal DOCX representation')
      } else return unsupported('not DOCX text')
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
  if (from.parent.attrs.source === null)
    unsupported(`cannot rewrite ${String(from.parent.attrs.locked)}`)
  for (const pos of [from, to]) {
    const leaf = pos.textOffset ? pos.parent.child(pos.index()) : undefined
    if (leaf && /^[\uDC00-\uDFFF]$/.test(leaf.text!.charAt(pos.textOffset)))
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
  'eastAsiaLang',
  'kernHalfPoints',
  'vanish',
  'vanishOwn',
  'rtl',
  'styleId',
  'themeRFonts',
  'themeColor',
  'eaSlotEmpty',
  'shadingDisplay',
  ...styleFields,
  ...toggles,
])

// The editor's hardBreak for each break character of a run (convert.ts runsToInline).
const breakAttrs: Readonly<Record<string, Record<string, boolean>>> = {
  '\n': {},
  '\f': { pageBreak: true },
  '\v': { colBreak: true },
  '\u001e': { wrapBreak: true },
}

// The break characters the engine puts in a run's text for w:br and w:cr.
// eslint-disable-next-line no-control-regex
const breakChar = /([\n\f\v\u001e])/

/** Why a run cannot be written again from its text and marks, if it cannot */
function lossy(run: Run): string | undefined {
  if (
    !plainText(run.text.replace(new RegExp(breakChar.source, 'g'), '')) ||
    Object.keys(run).some((key) => !runFields.has(key)) ||
    run.effective?.cs ||
    (run.csFont !== undefined && textHasComplexScript(run.text)) ||
    run.effective?.styleXor?.length
  )
    return 'a DOCX run with no lossless text projection'
  // The editor marks a withheld run with its own mark, which this reader does not carry.
  if (withheldLabelOf(run.rawRPr) !== null) return 'a DOCX run withheld from AI'
}

/** convert.ts tableStyleRunAttr: run properties a table style resolves, display only */
function tableStyleRun(eff: EffectiveRPr | undefined): string | null {
  if (!eff) return null
  const by = (k: string) => eff.source[k] === 'tableStyle'
  const t: Record<string, unknown> = {}
  if (by('sizeHalfPoints')) t.sizeHalfPoints = eff.sizeHalfPoints
  if (by('ascii') && eff.fonts.ascii) t.fontAscii = eff.fonts.ascii
  if (by('caps')) t.caps = eff.caps
  if (by('charSpacingTwips')) t.charSpacingTwips = eff.charSpacingTwips
  return Object.keys(t).length > 0 ? JSON.stringify(t) : null
}

/** The marks the editor gives a run this reader accepts (convert.ts runMarks) */
function runMarks(run: Run, rawStyles: ReadonlySet<string>): Mark[] {
  const marks = toggles.filter((key) => run[key]).map((key) => schema.mark(key))
  if (
    run.color ||
    run.sizeHalfPoints ||
    run.font ||
    run.fontAscii ||
    run.charSpacingTwips !== undefined ||
    run.kernHalfPoints !== undefined ||
    run.highlight ||
    run.shading ||
    run.shadingDisplay ||
    run.dstrike ||
    run.positionHalfPoints ||
    run.vertAlign ||
    run.em ||
    run.caps ||
    run.vanish ||
    run.bold === false ||
    run.italic === false ||
    run.eastAsiaLang ||
    run.rtl !== undefined ||
    run.styleId ||
    run.rawRPr
  ) {
    const attrs: Record<string, Json> = Object.fromEntries(
      styleFields.map((key) => [key, run[key] ?? null]),
    )
    Object.assign(attrs, {
      eaLang: run.eastAsiaLang ?? null,
      styleId: run.styleId ?? null,
      themeColor: run.themeColor ?? null,
      themeRFonts: run.themeRFonts ? JSON.stringify(run.themeRFonts) : null,
      kernHalfPoints: run.kernHalfPoints ?? null,
      kern:
        run.kernHalfPoints === undefined
          ? null
          : run.kernHalfPoints > 0 &&
            (run.sizeHalfPoints === undefined || run.sizeHalfPoints >= run.kernHalfPoints),
      vanish: run.vanish ?? null,
      vanishOwn: run.vanishOwn ?? null,
      rtl: run.rtl ?? null,
      eaSlotEmpty: run.eaSlotEmpty ?? null,
      boldOff: run.bold === false || null,
      italicOff: run.italic === false || null,
      shadingDisplay: run.shadingDisplay ?? null,
      tblRun: tableStyleRun(run.effective),
    })
    marks.push(readMark({ type: 'docTextStyle', attrs }, rawStyles))
  }
  return marks
}

/**
 * The inline content the editor makes of `runs` (convert.ts runsToInline): its size in
 * positions always, its nodes when every run is plain text, tabs and breaks.
 */
function inlineOf(runs: readonly Run[], rawStyles: Set<string>) {
  const nodes: Node[] = []
  let size = 0
  let problem: string | undefined
  for (const run of runs) {
    if (run.rawRPr) rawStyles.add(run.rawRPr)
    if (run.math || run.ruby || run.noteRef) {
      size += run.math?.breakAfter ? 2 : 1
      problem ??= 'an equation, ruby text or a note reference'
      continue
    }
    if (run.sym) {
      // The editor shows the private-use glyph instead when the symbol font covers it.
      const glyph = symbolPuaChar(run.sym.char)
      if (glyph && glyph.length !== run.text.length)
        unsupported('a DOCX symbol whose editor length depends on installed fonts')
      problem ??= 'a symbol character'
    }
    size += run.text.length + (run.image ? 1 : 0) + (run.xeTerm !== undefined ? 1 : 0)
    problem ??= lossy(run)
    if (problem) continue
    try {
      const marks = runMarks(run, rawStyles)
      for (const segment of run.text.split(breakChar)) {
        if (!segment) continue
        const attrs = breakAttrs[segment]
        nodes.push(
          attrs ? schema.node('hardBreak', attrs, undefined, marks) : schema.text(segment, marks),
        )
      }
    } catch (error) {
      if (!(error instanceof HistoryError)) throw error
      problem = error.message
    }
  }
  return { nodes, size, problem }
}

/** A text block's text as the engine reads it back from the XML */
function literal(node: Node): string {
  let text = ''
  node.forEach((leaf) => {
    text += leaf.isText
      ? leaf.text!
      : leaf.attrs.pageBreak
        ? '\f'
        : leaf.attrs.colBreak
          ? '\v'
          : leaf.attrs.wrapBreak
            ? '\u001e'
            : '\n'
  })
  return text
}

function leafXml(leaf: Node): string {
  if (!leaf.isText) {
    const { pageBreak, colBreak, wrapBreak } = leaf.attrs
    return pageBreak
      ? '<w:br w:type="page"/>'
      : colBreak
        ? '<w:br w:type="column"/>'
        : wrapBreak
          ? '<w:br w:type="textWrapping" w:clear="all"/>'
          : '<w:br/>'
  }
  return leaf
    .text!.split('\t')
    .map((part) =>
      part
        ? `<w:t xml:space="preserve">${part.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')}</w:t>`
        : '',
    )
    .join('<w:tab/>')
}

/** The rPr the editor writes for text with these marks (convert.ts inlineToRuns, then generate) */
function runProperties(marks: readonly Mark[]): string {
  if (!marks.length) return ''
  const run: Run = { text: '' }
  for (const mark of marks) {
    if (mark.type.name !== 'docTextStyle') {
      for (const key of toggles) if (mark.type.name === key) run[key] = true
      continue
    }
    const a = mark.attrs
    if (a.color) run.color = a.color
    if (a.sizeHalfPoints) run.sizeHalfPoints = a.sizeHalfPoints
    if (a.font) run.font = a.font
    if (a.fontAscii) run.fontAscii = a.fontAscii
    if (a.eastAsiaFont) run.eastAsiaFont = a.eastAsiaFont
    if (a.charSpacingTwips != null) run.charSpacingTwips = a.charSpacingTwips
    if (a.kernHalfPoints != null) run.kernHalfPoints = a.kernHalfPoints
    if (a.highlight) run.highlight = a.highlight
    if (a.shading) run.shading = a.shading
    if (a.dstrike != null) run.dstrike = Boolean(a.dstrike)
    if (a.positionHalfPoints) run.positionHalfPoints = a.positionHalfPoints
    if (a.vertAlign) run.vertAlign = a.vertAlign
    if (a.em) run.em = a.em
    if (a.vanish) run.vanish = true
    if (a.vanishOwn != null) run.vanishOwn = Boolean(a.vanishOwn)
    if (a.caps) run.caps = a.caps
    if (a.eaLang) run.eastAsiaLang = a.eaLang
    if (a.rtl != null) run.rtl = Boolean(a.rtl)
    if (a.styleId) run.styleId = a.styleId
    if (a.rawRPr) run.rawRPr = a.rawRPr
    if (a.themeRFonts) run.themeRFonts = JSON.parse(a.themeRFonts)
    if (a.themeColor) run.themeColor = a.themeColor
  }
  return mergeRPrModel(run.rawRPr ?? '', run, false)
}

/** Text blocks the renderer may write back, in document order */
function editable(root: Node): Node[] {
  const out: Node[] = []
  root.descendants((node) => {
    if (!node.isTextblock) return true
    if (node.attrs.source !== null) out.push(node)
    return false
  })
  return out
}

const textTypes: Readonly<Record<string, string>> = {
  paragraph: 'docParagraph',
  heading: 'docHeading',
  listItem: 'docListItem',
}

/**
 * The Docs editor's document (convert.ts blocksToPmDoc) with the positions it has. Paragraphs
 * that are plain text, tabs and breaks in the XML hold their runs and can be written back;
 * everything else keeps only its size.
 */
async function readBody(zip: JSZip, bytes: Uint8Array) {
  await checkPackage(zip)
  const xml = await xmlPart(zip, MAIN)
  const body = scanBody(xml)
  const parsed = await parseBody(zip, bytes)
  const { elements } = parsed.extras
  const rawStyles = new Set<string>()
  const sources: Paragraph[] = []
  const owners = new Map<number | null, number>()
  for (const block of parsed.blocks)
    owners.set(block.docxIndex, (owners.get(block.docxIndex) ?? 0) + 1)
  const paragraphAt = (index: number | null) => {
    const element = index === null || owners.get(index) !== 1 ? undefined : elements[index]
    const part = element?.name === 'w:p' ? body.paragraphs.get(element.start) : undefined
    return part?.end === element?.end ? part : undefined
  }
  function textBlock(
    type: string,
    runs: readonly Run[],
    part: Paragraph | undefined,
    why?: string,
  ) {
    const inline = inlineOf(runs, rawStyles)
    let locked = why ?? part?.problem ?? inline.problem
    if (!part) locked ??= 'a DOCX paragraph the body XML does not hold as such'
    if (!locked) {
      const node = schema.node(type, { source: sources.length }, inline.nodes)
      if (literal(node) === part!.text) {
        sources.push(part!)
        return node
      }
      locked = 'DOCX runs the editor reads differently from the XML'
    }
    // A placeholder of the editor's size: positions after it stay right, and no step may enter it.
    return schema.node(
      type,
      { locked },
      inline.size ? schema.text('-'.repeat(inline.size)) : undefined,
    )
  }
  function table(block: Block) {
    const model = block.table ?? { rows: [[{ paras: [''] }]] }
    const element = block.docxIndex === null ? undefined : elements[block.docxIndex]
    const xmlRows = element?.name === 'w:tbl' ? body.tables.get(element.start) : undefined
    const aligned =
      owners.get(block.docxIndex) === 1 &&
      xmlRows?.length === model.rows.length &&
      xmlRows.every((row, r) => row.length === model.rows[r]!.length)
    // convert.ts tableModelToPmNode: a merged-away cell has no node where a rowspan covers it.
    const columns = model.rows.map((row) => {
      let column = 0
      return row.map((cell) => (column += cell.colSpan ?? 1) - (cell.colSpan ?? 1))
    })
    const covered = model.rows.map(() => new Set<number>())
    model.rows.forEach((row, r) =>
      row.forEach((cell, c) => {
        if (cell.vMerge !== 'restart') return
        const at = columns[r]![c]!
        for (let below = r + 1; below < model.rows.length; below++) {
          const index = columns[below]!.indexOf(at)
          if (index === -1 || model.rows[below]![index]!.vMerge !== 'continue') break
          for (let column = at; column < at + (cell.colSpan ?? 1); column++)
            covered[below]!.add(column)
        }
      }),
    )
    return schema.node(
      'docTable',
      null,
      model.rows.map((row, r) =>
        schema.node(
          'docTableRow',
          null,
          row.flatMap((cell, c) => {
            if (cell.vMerge === 'continue') {
              if (covered[r]!.has(columns[r]![c]!)) return []
              const gap = textBlock('docParagraph', [], undefined, 'a merged-away DOCX cell')
              return [schema.node('docTableCell', null, [gap])]
            }
            return [
              schema.node(
                'docTableCell',
                null,
                cellContent(cell, aligned ? xmlRows[r]![c] : undefined),
              ),
            ]
          }),
        ),
      ),
    )
  }
  // convert.ts cellContentNodes: the cell's paragraphs, then nested tables and anchored boxes as
  // atoms at their anchors; splitCellBoxGroup makes one atom of a group's page-placed boxes and
  // one of the rest.
  function cellContent(cell: TableCell, xmlCell: Cell | undefined) {
    const rich = !!cell.richParas?.length
    const paragraphs = rich
      ? cell.richParas!
      : (cell.paras.length ? cell.paras : ['']).map((text) => ({
          runs: text ? [{ text }] : [],
        }))
    const why = !rich
      ? 'a DOCX cell read without its runs'
      : (xmlCell?.problem ??
        (xmlCell?.paragraphs.length !== paragraphs.length
          ? 'a DOCX cell whose paragraphs the editor reads differently'
          : undefined))
    const content = paragraphs.map((paragraph, i) =>
      textBlock(
        'list' in paragraph && paragraph.list ? 'docListItem' : 'docParagraph',
        paragraph.runs,
        why ? undefined : xmlCell!.paragraphs[i],
        why,
      ),
    )
    const inserts = (cell.nestedTables ?? []).map((_, i) =>
      Math.min(cell.nestedTableAnchors?.[i] ?? content.length, content.length),
    )
    const groups = new Map<number, boolean[]>()
    ;(cell.anchoredBoxes ?? []).forEach((box, i) => {
      const at = Math.min(cell.anchoredBoxAnchors?.[i] ?? 0, content.length)
      const onPage = !!box.outsideCell && !!box.floating && !!box.pageRelV
      groups.set(at, [...(groups.get(at) ?? []), onPage])
    })
    for (const [at, group] of groups) {
      if (group.includes(true)) inserts.push(at)
      if (group.includes(false)) inserts.push(at)
    }
    inserts.sort((a, b) => a - b)
    for (let i = inserts.length - 1; i >= 0; i--)
      content.splice(inserts[i]!, 0, schema.node('docAtom'))
    return content
  }
  const content: Node[] = []
  for (const block of parsed.blocks) {
    if (block.hidden) continue
    if (Object.hasOwn(textTypes, block.type))
      content.push(
        textBlock(textTypes[block.type]!, block.runs ?? [], paragraphAt(block.docxIndex)),
      )
    else if (block.type === 'table') content.push(table(block))
    else content.push(schema.node('docAtom'))
  }
  if (!content.length) content.push(textBlock('docParagraph', [], undefined, 'an empty DOCX body'))
  return { xml, sources, base: schema.node('doc', null, content), rawStyles }
}

/** DOCX/schema 4. Shared ZIP admission must precede this reader. */
export async function openDocx(bytes: Uint8Array): Promise<ByteHistoryAdapter<TextStep[]>> {
  const zip = await JSZip.loadAsync(bytes)
  const { xml, sources, base, rawStyles } = await readBody(zip, bytes)
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
      const before = editable(base)
      const pieces: string[] = []
      let end = 0
      let runBytes = 0
      // Text blocks never change structure, so both lists hold the same blocks in order.
      editable(doc).forEach((node, index) => {
        if (node.eq(before[index]!)) return
        const part = sources[node.attrs.source as number]!
        const prefix = xml.slice(part.start, part.prefixEnd)
        pieces.push(
          xml.slice(end, part.start),
          part.prefixEnd === part.end ? prefix.slice(0, -2) + '>' : prefix,
          part.anchors,
        )
        node.forEach((leaf) => {
          const run = `<w:r>${runProperties(leaf.marks)}${leafXml(leaf)}</w:r>`
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

/** The document the renderer replays a DOCX history on, in the Docs editor's positions */
export async function docxModel(bytes: Uint8Array): Promise<Node> {
  return (await readBody(await JSZip.loadAsync(bytes), bytes)).base
}
