import { HistoryError, type Json } from '@seuoffice/collab/client'
import JSZip from 'jszip'
import { SaxesParser, type SaxesTagNS } from 'saxes'
import type { SlidesEdit } from '../../../apps/slides/src/web/collab-ops'
import { decodeOp, encodeOp, GONE } from '../../../apps/slides/src/web/collab-wire'
import type { ByteHistoryAdapter } from './adapter'

const P = 'http://schemas.openxmlformats.org/presentationml/2006/main'
const A = 'http://schemas.openxmlformats.org/drawingml/2006/main'
const R = 'http://schemas.openxmlformats.org/officeDocument/2006/relationships'
const RELS = 'http://schemas.openxmlformats.org/package/2006/relationships'
const TYPES = 'http://schemas.openxmlformats.org/package/2006/content-types'
const MIME = 'application/vnd.openxmlformats-officedocument.presentationml.'
const EXT = '{FF2B5EF4-FFF2-40B4-BE49-F238E27FC236}'
const NS: Record<string, string> = {
  p: P,
  a: A,
  a16: 'http://schemas.microsoft.com/office/drawing/2014/main',
  r: R,
}
// The editor locates these ranges in the original XML. A second, nested spelling would
// not necessarily name the element a namespace-aware reader picked.
const slideParents: Record<string, readonly string[]> = {
  'p:spTree': ['p:cSld'],
  'p:cSld': ['p:sld'],
  'p:sp': ['p:spTree'],
  'p:nvSpPr': ['p:sp'],
  'p:cNvPr': ['p:nvGrpSpPr', 'p:nvSpPr'],
  'p:txBody': ['p:sp'],
  'a:p': ['p:txBody'],
  'a:r': ['a:p'],
  'a:t': ['a:r'],
  'a:rPr': ['a:r'],
  'a:fld': [],
  'a:br': [],
  'p:ph': [],
  'a16:creationId': ['a:ext'],
}

function unsupported(): never {
  throw new HistoryError('unsupported', 'PPTX is outside the plain text box history profile')
}
function invalid(): never {
  throw new HistoryError('invalid_history', 'PPTX history does not identify a unique target')
}

interface PartNode {
  tag: SaxesTagNS
  children: PartNode[]
  contentStart: number
  contentEnd: number
  value: string
}

async function part(zip: JSZip, path: string, name: string, uri: string) {
  const entry = zip.file(path) ?? invalid()
  const source = new TextDecoder('utf-8', { fatal: true, ignoreBOM: true }).decode(
    await entry.async('uint8array'),
  )
  const sax = new SaxesParser({ xmlns: true })
  const stack: PartNode[] = []
  let root: PartNode | undefined
  let count = 0
  sax.on('xmldecl', ({ version, encoding }) => {
    if (version !== '1.0' || (encoding && encoding.toUpperCase() !== 'UTF-8')) unsupported()
  })
  for (const event of ['doctype', 'cdata', 'comment', 'processinginstruction'] as const)
    sax.on(event, unsupported)
  sax.on('opentag', (tag) => {
    if (++count > 16_384 || stack.length >= 64) unsupported()
    if (tag.uri !== (tag.prefix ? NS[tag.prefix] : uri) || (!tag.prefix && name.includes(':')))
      unsupported()
    const allowed =
      name === 'p:sld' && Object.hasOwn(slideParents, tag.name) ? slideParents[tag.name] : undefined
    if (allowed && !allowed.includes(stack[stack.length - 1]?.tag.name ?? '')) unsupported()
    if (
      Object.values(tag.attributes).some(
        (a) => a.uri === 'http://schemas.openxmlformats.org/markup-compatibility/2006',
      )
    )
      unsupported()
    if (tag.attributes['r:id'] && tag.attributes['r:id'].uri !== R) unsupported()
    const node: PartNode = {
      tag,
      children: [],
      value: '',
      contentStart: sax.position,
      contentEnd: sax.position,
    }
    if (stack.length) stack[stack.length - 1]!.children.push(node)
    else root = node
    stack.push(node)
  })
  sax.on('text', (value) => {
    const node = stack[stack.length - 1]
    if (node?.tag.name === 'a:t') node.value += value
    else if (value.trim()) unsupported()
  })
  sax.on('closetag', (tag) => {
    const node = stack.pop()!
    if (!tag.isSelfClosing) {
      node.contentEnd = source.lastIndexOf('<', sax.position - 1)
      if (tag.name === 'a:t' && !source.startsWith('</a:t>', node.contentEnd)) unsupported()
    }
  })
  sax.write(source).close()
  if (!root || root.tag.name !== name || root.tag.uri !== uri) invalid()
  return { source, root }
}

const children = (node: PartNode, name: string) =>
  node.children.filter((child) => child.tag.name === name)
function one(node: PartNode, name: string): PartNode {
  const matches = children(node, name)
  return matches.length === 1 ? matches[0]! : unsupported()
}
const attr = (node: PartNode, key: string) => node.tag.attributes[key]?.value

async function relationships(zip: JSZip, path: string) {
  const { root } = await part(zip, path, 'Relationships', RELS)
  const byId = new Map<string, PartNode>()
  for (const rel of root.children) {
    const id = attr(rel, 'Id')
    if (rel.tag.name !== 'Relationship' || !id || byId.has(id)) invalid()
    if (attr(rel, 'Type')?.includes('/digital-signature/')) unsupported()
    byId.set(id, rel)
  }
  return byId
}
const internal = (rel: PartNode) =>
  !attr(rel, 'TargetMode') || attr(rel, 'TargetMode') === 'Internal'

interface Box {
  text: PartNode
  language: boolean
  current: string
}

function boxOf(shape: PartNode): Box {
  const body = one(shape, 'p:txBody')
  const para = one(body, 'a:p')
  const run = one(para, 'a:r')
  const text = one(run, 'a:t')
  const props = children(run, 'a:rPr')
  const bodyProps = one(body, 'a:bodyPr')
  if (
    body.children.some((n) => !['a:bodyPr', 'a:lstStyle', 'a:p'].includes(n.tag.name)) ||
    bodyProps.children.some((n) => n.tag.name !== 'a:noAutofit') ||
    children(body, 'a:lstStyle').some((n) => n.children.length) ||
    para.children.some((n) => !['a:r', 'a:endParaRPr'].includes(n.tag.name)) ||
    run.children.some((n) => !['a:rPr', 'a:t'].includes(n.tag.name)) ||
    props.length > 1 ||
    props.some(
      (n) => n.children.length || Object.keys(n.tag.attributes).some((a) => a !== 'lang'),
    ) ||
    text.tag.isSelfClosing ||
    text.children.length ||
    Object.keys(text.tag.attributes).length ||
    !plainText(text.value)
  )
    unsupported()
  return { text, current: text.value, language: !!(props[0] && attr(props[0], 'lang')) }
}

const plainText = (value: string) =>
  value.length > 0 && !/[^\u0020-\uD7FF\uE000-\uFFFD\u{10000}-\u{10FFFF}]/u.test(value)
const escapeText = (value: string) =>
  value.replaceAll('&', '&amp;').replaceAll('<', '&lt;').replaceAll('>', '&gt;')

type TextOp = { id: string; edit: Extract<SlidesEdit, { kind: 'text' }> }

function decode(json: Json): TextOp {
  const { id, edit } = decodeOp(json)
  if (
    edit.kind !== 'text' ||
    edit.at.group ||
    edit.at.slide === GONE ||
    edit.autofit ||
    edit.paragraphs.length !== 1
  )
    unsupported()
  const p = edit.paragraphs[0]!
  const r = p.runs[0]
  if (
    p.runs.length !== 1 ||
    !r ||
    !plainText(r.text) ||
    (p.srcPara ?? 0) !== 0 ||
    (r.srcRun ?? 0) !== 0 ||
    Object.keys(p).some((k) => k !== 'srcPara' && k !== 'runs') ||
    Object.keys(r).some((k) => k !== 'srcRun' && k !== 'text')
  )
    unsupported()
  return { id, edit }
}

export async function openPptx(bytes: Uint8Array): Promise<ByteHistoryAdapter<TextOp>> {
  const zip = await JSZip.loadAsync(bytes)
  const types = new Map<string, string>()
  for (const node of children(
    (await part(zip, '[Content_Types].xml', 'Types', TYPES)).root,
    'Override',
  )) {
    const name = attr(node, 'PartName'),
      type = attr(node, 'ContentType')
    if (!name || !type || types.has(name)) invalid()
    types.set(name, type)
  }
  const office = [...(await relationships(zip, '_rels/.rels')).values()].filter(
    (r) => attr(r, 'Type') === `${R}/officeDocument`,
  )
  if (
    office.length !== 1 ||
    !internal(office[0]!) ||
    attr(office[0]!, 'Target') !== 'ppt/presentation.xml' ||
    types.get('/ppt/presentation.xml') !== `${MIME}presentation.main+xml`
  )
    unsupported()
  const pres = (await part(zip, 'ppt/presentation.xml', 'p:presentation', P)).root
  const rels = await relationships(zip, 'ppt/_rels/presentation.xml.rels')
  const slides = new Map<string, Map<string, Box>>()
  const parts: { path: string; source: string; boxes: Box[] }[] = []
  const ids = new Set<string>()
  for (const node of one(pres, 'p:sldIdLst').children) {
    const rel = rels.get(attr(node, 'r:id') ?? '')
    const id = attr(node, 'id')
    if (
      node.tag.name !== 'p:sldId' ||
      !id ||
      ids.has(id) ||
      !rel ||
      !internal(rel) ||
      attr(rel, 'Type') !== `${R}/slide`
    )
      invalid()
    ids.add(id)
    const target = attr(rel, 'Target') ?? ''
    const number = /^slides\/slide([1-9]\d*)\.xml$/.exec(target)?.[1]
    if (!number) unsupported()
    const slide = `s_${number}`,
      path = `ppt/${target}`
    if (slides.has(slide) || types.get(`/${path}`) !== `${MIME}slide+xml`) invalid()
    const { source, root } = await part(zip, path, 'p:sld', P)
    const tree = one(one(root, 'p:cSld'), 'p:spTree')
    const refs = new Map<string, Box>(),
      boxes: Box[] = []
    for (const shape of tree.children) {
      if (shape.tag.name === 'p:nvGrpSpPr' || shape.tag.name === 'p:grpSpPr') continue
      if (shape.tag.name !== 'p:sp') unsupported()
      const nv = one(one(shape, 'p:nvSpPr'), 'p:cNvPr')
      if (nv.children.length !== 1) unsupported()
      const numeric = attr(nv, 'id') ?? ''
      const ext = one(one(nv, 'a:extLst'), 'a:ext')
      if (ext.children.length !== 1) unsupported()
      const guid = attr(one(ext, 'a16:creationId'), 'id') ?? ''
      if (
        attr(ext, 'uri') !== EXT ||
        !/^\d{1,10}$/.test(numeric) ||
        !/^\{[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}\}$/i.test(guid)
      )
        unsupported()
      const box = boxOf(shape)
      boxes.push(box)
      for (const key of new Set([`e_${numeric}`, `e_${guid.slice(1, 9).toLowerCase()}`])) {
        if (refs.has(key)) invalid()
        refs.set(key, box)
      }
    }
    parts.push({ path, source, boxes })
    slides.set(slide, refs)
  }
  if (!slides.size) invalid()
  const target = (op: TextOp) => slides.get(op.edit.at.slide)?.get(op.edit.at.el) ?? invalid()
  return {
    decode,
    encode: encodeOp,
    integrate(remote, local) {
      if (local.length) invalid()
      for (const op of remote) {
        const box = target(op),
          text = op.edit.paragraphs[0]!.runs[0]!.text
        // The editor stamps ko-KR when a changed, untagged run gains Hangul.
        if (!box.language && /\p{Script=Hangul}/u.test(text)) unsupported()
        box.current = text
      }
      return []
    },
    captureUndo(op) {
      const box = target(op),
        text = box.current
      return (later) =>
        later.some((next) => target(next) === box)
          ? null
          : [
              {
                id: crypto.randomUUID(),
                edit: { kind: 'text', at: op.edit.at, paragraphs: [{ runs: [{ text }] }] },
              },
            ]
    },
    async produceFile() {
      let dirty = false
      for (const { path, source, boxes } of parts) {
        let pos = 0
        const chunks: string[] = []
        for (const box of boxes) {
          if (box.current === box.text.value) continue
          chunks.push(source.slice(pos, box.text.contentStart), escapeText(box.current))
          pos = box.text.contentEnd
        }
        if (!chunks.length) continue
        chunks.push(source.slice(pos))
        const output = chunks.join('')
        if (Buffer.byteLength(output) > 1024 * 1024) unsupported()
        zip.file(path, output, { date: zip.file(path)!.date, createFolders: false })
        dirty = true
      }
      return {
        fileName: 'history.pptx',
        bytes: dirty
          ? await zip.generateAsync({ type: 'uint8array', compression: 'DEFLATE' })
          : bytes,
      }
    },
  }
}
