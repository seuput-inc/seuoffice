import type { HwpDocument } from '@rhwp/core'
import { assertZipInflatesWithinLimits } from '@seuoffice/zip-gate'
import JSZip from 'jszip'
import { SaxesParser } from 'saxes'

const header = 'Contents/header.xml'
const manifest = 'Contents/content.hpf'
const head = 'http://www.hancom.co.kr/hwpml/2011/head'
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

function tree(bytes: Uint8Array, part: string): Xml {
  const stack: Xml[] = []
  let root: Xml | undefined
  let elements = 0
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
    stack.pop()
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
// The independent blank document provides evidence for its defaults only, never for custom data.
function copiedRegions(part: string, root: Xml): string[] {
  const regions: string[] = []
  const visit = (node: Xml) => {
    if (
      (part === header &&
        node.uri === head &&
        ['numbering', 'bullet', 'memoProperties'].includes(node.local)) ||
      (part === manifest && node.uri === opf && node.local === 'metadata')
    ) {
      regions.push(JSON.stringify(node))
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
    regions.push(JSON.stringify(root.children.slice(ref + 1)))
  }
  visit(root)
  return regions
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
    if (['settings.xml', 'version.xml', 'META-INF/rhwp-hwp5-origin'].includes(name)) {
      const change = difference(original, independent)
      if (change) refuse(name, `unverified source-only settings (${change})`)
    } else if (!(original instanceof Uint8Array)) {
      const defaults = new Set(
        independent && !(independent instanceof Uint8Array) ? copiedRegions(name, independent) : [],
      )
      if (copiedRegions(name, original).some((r) => !defaults.has(r)))
        refuse(name, 'unverified source-only XML')
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
