import type { HwpDocument } from '@rhwp/core'
import { assertZipInflatesWithinLimits } from '@seuoffice/zip-gate'
import JSZip from 'jszip'
import { SaxesParser, type SaxesTagNS } from 'saxes'

const header = 'Contents/header.xml'
const manifest = 'Contents/content.hpf'
const hml = 'http://www.hancom.co.kr/hwpml/2011/'
const head = hml + 'head'
const paragraph = hml + 'paragraph'
const contentNamespaces = [
  head,
  hml + 'section',
  hml + 'core',
  paragraph,
  'http://www.hancom.co.kr/hwpml/2016/paragraph',
]
const paragraphAttrs = ['id', 'paraPrIDRef', 'styleIDRef', 'columnBreak', 'pageBreak', 'merged']
const labelAttrs = [
  'topmargin',
  'leftmargin',
  'boxwidth',
  'boxlength',
  'boxmarginhor',
  'boxmarginver',
  'labelcols',
  'labelrows',
  'landscape',
  'pagewidth',
  'pageheight',
]
const opf = 'http://www.idpf.org/2007/opf/'
const limits = { maxParts: 256, maxPartBytes: 1024 * 1024, maxTotalBytes: 4 * 1024 * 1024 }
const maxFileBytes = 2 * 1024 * 1024
const packageXml = [
  header,
  manifest,
  'version.xml',
  'settings.xml',
  'META-INF/container.rdf',
  'META-INF/container.xml',
  'META-INF/manifest.xml',
]
const required = [...packageXml, 'Contents/section0.xml', 'mimetype']
const assets = [
  'mimetype',
  'META-INF/rhwp-hwp5-origin',
  'Preview/PrvText.txt',
  'Preview/PrvImage.png',
]

type Engine = Pick<typeof HwpDocument, 'createEmpty'> & (new (bytes: Uint8Array) => HwpDocument)
type Xml = { uri: string; local: string; attrs: string[][]; children: (Xml | string)[] }
type Part = Xml | Uint8Array

function refuse(part: string, reason: string): never {
  throw new Error(`HWPX source preservation: ${part}: ${reason}`)
}

// The writer also round-trips opaque paragraph markup and arbitrary table label attributes.
// Byte preservation alone says nothing about their relationships or editing semantics.
export function unverifiedHwpxMarkup(tag: SaxesTagNS): string | undefined {
  if (!contentNamespaces.includes(tag.uri)) return tag.name
  for (const a of Object.values(tag.attributes)) {
    if (a.uri === 'http://www.w3.org/2000/xmlns/') continue
    if (
      (a.uri && a.prefix !== 'xml' && !contentNamespaces.includes(a.uri)) ||
      (tag.uri === paragraph &&
        tag.local === 'p' &&
        (a.uri || !paragraphAttrs.includes(a.local))) ||
      (tag.uri === paragraph && tag.local === 'label' && (a.uri || !labelAttrs.includes(a.local)))
    )
      return `${tag.name}/@${a.name}`
  }
}

function tree(bytes: Uint8Array, part: string): Xml {
  const stack: Xml[] = []
  let root: Xml | undefined
  let elements = 0
  const content = part === header || part.startsWith('Contents/section')
  const master = part.startsWith('Contents/masterpage')
  const parser = new SaxesParser({ xmlns: true, fileName: part })
  parser.on('xmldecl', ({ encoding, version }) => {
    if ((encoding && !/^utf-8$/i.test(encoding)) || version !== '1.0')
      refuse(part, 'unsupported XML declaration')
  })
  parser.on('doctype', () => refuse(part, 'document type declaration'))
  parser.on('comment', () => refuse(part, 'unmodelled XML comment'))
  parser.on('processinginstruction', () => refuse(part, 'unmodelled processing instruction'))
  parser.on('opentag', (tag) => {
    if (stack.length >= 64 || ++elements > 40_000) refuse(part, 'XML structure limit')
    if (content || (master && stack.length > 0)) {
      const markup = unverifiedHwpxMarkup(tag)
      if (markup) refuse(part, `unverified HWPX extension markup: ${markup}`)
    }
    const attrs = Object.values(tag.attributes)
      .filter((a) => a.uri !== 'http://www.w3.org/2000/xmlns/')
      .map((a) => [a.uri, a.local, a.value])
      .sort(([au, al], [bu, bl]) => (au < bu ? -1 : au > bu ? 1 : al < bl ? -1 : al > bl ? 1 : 0))
    const node: Xml = { uri: tag.uri, local: tag.local, attrs, children: [] }
    if (stack.length) stack[stack.length - 1].children.push(node)
    else root = node
    stack.push(node)
  })
  parser.on('closetag', () => {
    const node = stack.pop()!
    // An empty text element holds no characters: rhwp writes an empty run as `<hp:t></hp:t>`
    // where Hancom Office writes none, and drops one Hancom Office leaves after an object.
    if (node.uri === paragraph && node.local === 't' && !node.attrs.length && !node.children.length)
      stack.at(-1)?.children.pop()
  })
  const text = (value: string) => {
    if (!value || !stack.length) return
    const { children } = stack[stack.length - 1]
    const last = children.at(-1)
    if (typeof last === 'string') children[children.length - 1] = last + value
    else children.push(value)
  }
  parser.on('text', text)
  parser.on('cdata', text)
  parser.write(new TextDecoder('utf-8', { fatal: true }).decode(bytes)).close()
  return root ?? refuse(part, 'missing XML root')
}

async function parts(bytes: Uint8Array): Promise<Map<string, Part>> {
  if (bytes.length > maxFileBytes) refuse('ZIP', 'file size limit')
  await assertZipInflatesWithinLimits(bytes, limits)
  const zip = await JSZip.loadAsync(bytes, { checkCRC32: true, createFolders: false })
  const files = Object.values(zip.files)
  const end = bytes.length - 22
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength)
  if (
    end < 0 ||
    view.getUint32(end, true) !== 0x06054b50 ||
    view.getUint16(end + 20, true) !== 0 ||
    view.getUint16(end + 10, true) !== files.length
  )
    refuse('ZIP', 'ambiguous part directory')
  const result = new Map<string, Part>()
  for (const entry of files) {
    if (entry.dir) continue
    const name = entry.name
    if (entry.unsafeOriginalName !== name) refuse(name, 'noncanonical part name')
    if (
      packageXml.includes(name) ||
      /^Contents\/(?:section|masterpage)(?:0|[1-9]\d*)\.xml$/.test(name)
    ) {
      result.set(name, tree(await entry.async('uint8array'), name))
    } else if (
      assets.includes(name) ||
      /^BinData\/[\w.-]+\.(?:png|jpe?g|gif|bmp|tiff?|wmf|emf|webp)$/i.test(name)
    ) {
      result.set(name, await entry.async('uint8array'))
    } else refuse(name, 'unverified part')
  }
  for (const name of required) if (!result.has(name)) refuse(name, 'missing part')
  return result
}

function serialize(doc: HwpDocument): Uint8Array {
  const out = doc.exportHwpxWithReport()
  try {
    const report = JSON.parse(out.contentLoss())
    if (
      report.schemaVersion !== 1 ||
      report.outputFormat !== 'hwpx' ||
      report.count !== 0 ||
      !Array.isArray(report.losses) ||
      report.losses.length
    )
      refuse('model', 'serializer reported loss')
    return out.takeBytes()
  } finally {
    out.free()
  }
}

function xmlDifference(a: Xml, b: Xml): string | undefined {
  if (a.uri !== b.uri || a.local !== b.local) return `/${a.local} (element name)`
  for (let i = 0; i < Math.max(a.attrs.length, b.attrs.length); i++) {
    const left = a.attrs[i]
    const right = b.attrs[i]
    if (!left || !right || left.some((value, j) => value !== right[j]))
      return `/${a.local}/@${(left ?? right)[1]}`
  }
  for (let i = 0; i < Math.max(a.children.length, b.children.length); i++) {
    const left = a.children[i]
    const right = b.children[i]
    if (left === right) continue
    if (!left || !right || typeof left === 'string' || typeof right === 'string')
      return `/${a.local}/node()[${i + 1}]`
    const change = xmlDifference(left, right)
    if (change) return `/${a.local}${change}`
  }
}

function difference(a: Part, b: Part | undefined): string | undefined {
  if (b === undefined) return 'missing part'
  if (a instanceof Uint8Array && b instanceof Uint8Array) {
    return a.length === b.length && a.every((v, i) => v === b[i]) ? undefined : 'binary payload'
  }
  if (a instanceof Uint8Array || b instanceof Uint8Array) return 'part type'
  return xmlDifference(a, b)
}

// These XML regions are spliced from the input by rhwp, not written from editable model fields.
// The independent blank document admits its own defaults; `copiedXml` admits the rest.
function copiedRegions(part: string, root: Xml): (Xml | string)[] {
  const regions: (Xml | string)[] = []
  const visit = (node: Xml) => {
    if (
      (part === header &&
        node.uri === head &&
        ['numbering', 'bullet', 'memoProperties'].includes(node.local)) ||
      (part === manifest && node.uri === opf && node.local === 'metadata')
    ) {
      regions.push(node)
      return
    }
    if (node.local === 'parameters') refuse(part, 'unverified field parameter XML')
    if (
      part === manifest &&
      node.local === 'item' &&
      node.attrs.some(([, local, value]) => local === 'href' && value.startsWith('Scripts/'))
    )
      refuse(part, 'unverified script relationship')
    for (const child of node.children) if (typeof child !== 'string') visit(child)
  }
  if (part === header) {
    const ref = root.children.findIndex(
      (n) => typeof n !== 'string' && n.uri === head && n.local === 'refList',
    )
    if (ref === -1) refuse(part, 'missing resource list')
    regions.push(...root.children.slice(ref + 1))
  }
  visit(root)
  return regions
}

type Rule = { in: string | null; attrs: string[]; text?: true }
const config = 'urn:oasis:names:tc:opendocument:xmlns:config:1.0'
const appSetting = hml + 'app HWPApplicationSetting'
const compatible = `${head} compatibleDocument`
const layout = `${head} layoutCompatibility`
const paraHead: Rule = {
  in: null,
  attrs: [
    'start',
    'level',
    'align',
    'useInstWidth',
    'autoIndent',
    'widthAdjust',
    'textOffsetType',
    'textOffset',
    'numFormat',
    'charPrIDRef',
    'checkable',
  ],
  text: true,
}
// rhwp copies these parts and regions from the source into every save, edited or not, and the
// reconstruction check below proves it for the file at hand. They are document-wide: the writing
// application, print options, the caret to restore, compatibility and link options, metadata, and
// the numbering and bullet definitions paragraphs refer to by id. Co-editing commands set only a
// paragraph's heading type and level, never a definition. Admitted element by element; `in` is the
// parent, null for the root of a part or region.
const copiedXml: Record<string, Record<string, Rule>> = {
  'version.xml': {
    [hml + 'version HCFVersion']: {
      in: null,
      attrs: [
        'tagetApplication',
        'major',
        'minor',
        'micro',
        'buildNumber',
        'os',
        'xmlVersion',
        'application',
        'appVersion',
      ],
    },
  },
  'settings.xml': {
    [appSetting]: { in: null, attrs: [] },
    [hml + 'app CaretPosition']: { in: appSetting, attrs: ['listIDRef', 'paraIDRef', 'pos'] },
    [config + ' config-item-set']: { in: appSetting, attrs: ['name'] },
    [config + ' config-item']: {
      in: config + ' config-item-set',
      attrs: ['name', 'type'],
      text: true,
    },
  },
  [header]: {
    [`${head} numbering`]: { in: null, attrs: ['id', 'start'] },
    [`${head} bullet`]: { in: null, attrs: ['id', 'char', 'useImage'] },
    [`${head} paraHead`]: paraHead,
    [compatible]: { in: null, attrs: ['targetProgram'] },
    [layout]: { in: compatible, attrs: [] },
    ...Object.fromEntries(
      ['char', 'paragraph', 'section', 'object', 'field'].map((n) => [
        `${head} ${n}`,
        { in: layout, attrs: [] },
      ]),
    ),
    [`${head} docOption`]: { in: null, attrs: [] },
    [`${head} linkinfo`]: {
      in: `${head} docOption`,
      attrs: ['path', 'pageInherit', 'footnoteInherit'],
    },
    [`${head} metaTag`]: { in: null, attrs: [], text: true },
    [`${head} trackchageConfig`]: { in: null, attrs: ['flags'] },
  },
  [manifest]: {
    [`${opf} metadata`]: { in: null, attrs: [] },
    [`${opf} title`]: { in: `${opf} metadata`, attrs: [], text: true },
    [`${opf} language`]: { in: `${opf} metadata`, attrs: [], text: true },
    [`${opf} meta`]: { in: `${opf} metadata`, attrs: ['name', 'content'], text: true },
  },
}

/** The path of the first element, attribute or text of `node` that `copiedXml` does not admit. */
function unverifiedCopiedXml(
  part: string,
  node: Xml | string,
  parent: string | null,
): string | undefined {
  if (typeof node === 'string') return '/text()'
  const name = `${node.uri} ${node.local}`
  const rule = copiedXml[part]?.[name]
  // A paragraph head belongs to the numbering or bullet around it.
  const within = name === `${head} paraHead` ? [`${head} numbering`, `${head} bullet`] : [rule?.in]
  if (!rule || !within.includes(parent)) return `/${node.local}`
  for (const [uri, local] of node.attrs)
    if (uri || !rule.attrs.includes(local)) return `/${node.local}/@${local}`
  for (const [i, child] of node.children.entries()) {
    const change =
      typeof child !== 'string'
        ? unverifiedCopiedXml(part, child, name)
        : rule.text
          ? undefined
          : `/node()[${i + 1}]`
    if (change) return `/${node.local}${change}`
  }
}

/** Additional to package/relationship and protection checks. The constructor must use initialized WASM. */
export async function certifyHwpxSource(bytes: Uint8Array, Document: Engine): Promise<void> {
  if (bytes.length > maxFileBytes) refuse('ZIP', 'file size limit')
  const snapshot = new Uint8Array(bytes)
  const source = await parts(snapshot)
  const blank = Document.createEmpty()
  let baseline: Map<string, Part>
  try {
    blank.createBlankDocument()
    baseline = await parts(serialize(blank))
  } finally {
    blank.free()
  }
  for (const [name, original] of source) {
    const independent = baseline.get(name)
    if (name === 'version.xml' || name === 'settings.xml') {
      const change =
        original instanceof Uint8Array ? 'part type' : unverifiedCopiedXml(name, original, null)
      if (change) refuse(name, `unverified source-only settings (${change})`)
    } else if (name === 'META-INF/rhwp-hwp5-origin') {
      // It tells rhwp how to read the paragraphs, so only the value rhwp writes itself is known.
      const change = difference(original, independent)
      if (change) refuse(name, `unverified source-only settings (${change})`)
    } else if (!(original instanceof Uint8Array)) {
      const defaults = new Set(
        independent && !(independent instanceof Uint8Array)
          ? copiedRegions(name, independent).map((r) => JSON.stringify(r))
          : [],
      )
      for (const region of copiedRegions(name, original)) {
        if (defaults.has(JSON.stringify(region))) continue
        const change = unverifiedCopiedXml(name, region, null)
        if (change) refuse(name, `unverified source-only XML (${change})`)
      }
    }
  }
  const doc = new Document(snapshot)
  let rebuilt: Map<string, Part>
  try {
    if (doc.getSourceFormat() !== 'hwpx') refuse('model', 'wrong format')
    rebuilt = await parts(serialize(doc))
  } finally {
    doc.free()
  }
  for (const [name, original] of source) {
    const change = difference(original, rebuilt.get(name))
    if (change) refuse(name, `model reconstruction differs (${change})`)
  }
  for (const name of rebuilt.keys())
    if (!source.has(name)) refuse(name, 'model reconstruction added a part')
}
