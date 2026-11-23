import { createHash } from 'node:crypto'
import { HistoryError } from '@seuoffice/collab/client'
import {
  getSlideNotes,
  matchesElementRef,
  notesPathForSlide,
  openPptx as openDeck,
  patchSlideXml,
  resolveTarget,
  type OpenedPptx,
} from '@seuoffice/pptx-engine'
import JSZip from 'jszip'
import { SaxesParser, type SaxesTagNS } from 'saxes'
import {
  applyOp,
  deckFile,
  slideIndexOf,
  type SlidesEdit,
  type SlidesOp,
} from '../../../apps/slides/src/web/collab-ops'
import { decodeOp, encodeOp, GONE, WIRE_LIMITS } from '../../../apps/slides/src/web/collab-wire'
import type { ByteHistoryAdapter } from './adapter'

const P = 'http://schemas.openxmlformats.org/presentationml/2006/main'
const A = 'http://schemas.openxmlformats.org/drawingml/2006/main'
const R = 'http://schemas.openxmlformats.org/officeDocument/2006/relationships'
const RELS = 'http://schemas.openxmlformats.org/package/2006/relationships'
const TYPES = 'http://schemas.openxmlformats.org/package/2006/content-types'
const MIME = 'application/vnd.openxmlformats-officedocument.presentationml.'
const NS: Record<string, string> = {
  p: P,
  a: A,
  a16: 'http://schemas.microsoft.com/office/drawing/2014/main',
  r: R,
}

function unsupported(): never {
  throw new HistoryError('unsupported', 'PPTX feature is outside the byte replay profile')
}
function invalid(): never {
  throw new HistoryError('invalid_history', 'PPTX history does not identify a unique target')
}

interface PartNode {
  tag: SaxesTagNS
  children: PartNode[]
  value: string
}

const textParents: Record<string, string> = {
  'p:cNvPr': 'p:nvSpPr',
  'p:txBody': 'p:sp',
  'a:p': 'p:txBody',
  'a:r': 'a:p',
  'a:rPr': 'a:r',
  'a:t': 'a:r',
}

function parseXml(source: string, fragment = false): PartNode {
  const sax = new SaxesParser({ xmlns: true, ...(fragment ? { additionalNamespaces: NS } : {}) })
  const stack: PartNode[] = []
  let root: PartNode | undefined
  let count = 0
  sax.on('error', (error) => {
    if (fragment) unsupported()
    throw error
  })
  sax.on('xmldecl', ({ version, encoding }) => {
    if (version !== '1.0' || (encoding && encoding.toUpperCase() !== 'UTF-8')) unsupported()
  })
  for (const event of ['doctype', 'cdata', 'comment', 'processinginstruction'] as const)
    sax.on(event, unsupported)
  sax.on('opentag', (tag) => {
    if (++count > 16_384 || stack.length >= 64) unsupported()
    if (Object.hasOwn(NS, tag.prefix) && tag.uri !== NS[tag.prefix]) unsupported()
    if (tag.attributes['r:id'] && tag.attributes['r:id'].uri !== R) unsupported()
    if (fragment) {
      if (Object.hasOwn(textParents, tag.name) && stack.at(-1)?.tag.name !== textParents[tag.name])
        unsupported()
      if (tag.name === 'a:fld' || tag.name === 'a:br' || (tag.name === 'p:sp' && stack.length))
        unsupported()
    }
    // Saving a deck with embedded fonts may inflate a second, compressed font container.
    if (tag.local === 'embeddedFontLst' || tag.local === 'embeddedFont') unsupported()
    // Removing a slide would also have to repair these lists of slide ids.
    if (tag.local === 'custShowLst' || tag.local === 'sectionLst') unsupported()
    const node: PartNode = {
      tag,
      children: [],
      value: '',
    }
    if (stack.length) stack[stack.length - 1]!.children.push(node)
    else root = node
    stack.push(node)
  })
  sax.on('text', (value) => {
    const node = stack[stack.length - 1]
    if (node?.tag.name === 'a:t') node.value += value
  })
  sax.on('closetag', () => {
    stack.pop()
  })
  sax.write(source).close()
  return root ?? invalid()
}

const children = (node: PartNode, name: string) =>
  node.children.filter((child) => child.tag.name === name)
function one(node: PartNode, name: string): PartNode {
  const matches = children(node, name)
  return matches.length === 1 ? matches[0]! : unsupported()
}
const attr = (node: PartNode, key: string) => node.tag.attributes[key]?.value

function relationships(root: PartNode) {
  if (root.tag.name !== 'Relationships' || root.tag.uri !== RELS) invalid()
  const byId = new Map<string, PartNode>()
  for (const rel of root.children) {
    const id = attr(rel, 'Id')
    if (
      rel.tag.name !== 'Relationship' ||
      rel.tag.uri !== RELS ||
      !id ||
      byId.has(id) ||
      !attr(rel, 'Target') ||
      !attr(rel, 'Type')
    )
      invalid()
    if (attr(rel, 'Type')?.includes('/digital-signature/')) unsupported()
    byId.set(id, rel)
  }
  return byId
}
const internal = (rel: PartNode) =>
  attr(rel, 'TargetMode') === undefined || attr(rel, 'TargetMode') === 'Internal'

type TextEdit = Extract<SlidesEdit, { kind: 'text' }>
const textOnly = (value: string) =>
  value.length > 0 && !/[^\u0020-\uD7FF\uE000-\uFFFD\u{10000}-\u{10FFFF}]/u.test(value)

// Keep the in-place writer on the original run slots; rebuilding a rich txBody can drop markup.
function textBox(opened: OpenedPptx, edit: TextEdit, undo = false) {
  if (edit.at.group || edit.autofit) unsupported()
  const slide = opened.deck.slides[slideIndexOf(opened, edit.at.slide)] ?? invalid()
  const matches = slide.elements.filter((el) => matchesElementRef(el, edit.at.el))
  if (matches.length !== 1) invalid()
  const el = matches[0]!
  if ((el.type !== 'text' && el.type !== 'shape') || !el.text) unsupported()
  const shape = parseXml(el.anchor.originalXml, true)
  if (shape.tag.name !== 'p:sp') unsupported()
  const body = one(shape, 'p:txBody')
  const paras = children(body, 'a:p')
  if (
    !paras.length ||
    paras.length !== edit.paragraphs.length ||
    paras.length !== el.text.paragraphs.length ||
    body.children.some((n) => !['a:bodyPr', 'a:lstStyle', 'a:p'].includes(n.tag.name)) ||
    one(body, 'a:bodyPr').children.some((n) => n.tag.name !== 'a:noAutofit') ||
    children(body, 'a:lstStyle').some((n) => n.children.length)
  )
    unsupported()
  for (const [i, para] of paras.entries()) {
    const p = edit.paragraphs[i]!,
      runs = children(para, 'a:r')
    if (
      (p.srcPara ?? i) !== i ||
      Object.keys(p).some((k) => k !== 'runs' && k !== 'srcPara') ||
      para.children.some((n) => !['a:r', 'a:endParaRPr'].includes(n.tag.name)) ||
      !runs.length ||
      p.runs.length !== runs.length ||
      el.text.paragraphs[i]!.runs.length !== runs.length
    )
      unsupported()
    for (const [j, run] of runs.entries()) {
      const r = p.runs[j]!,
        t = one(run, 'a:t'),
        props = children(run, 'a:rPr')
      if (
        (r.srcRun ?? j) !== j ||
        r.field !== undefined ||
        r.redact !== undefined ||
        (r.link?.kind === 'slide' && r.link.slideIndex >= opened.deck.slides.length) ||
        !textOnly(r.text) ||
        !textOnly(t.value) ||
        t.children.length ||
        run.children.some((n) => n.tag.name !== 'a:t' && n.tag.name !== 'a:rPr') ||
        props.length > 1 ||
        (undo &&
          (Object.keys(r).some((k) => k !== 'text' && k !== 'srcRun') ||
            (!props[0]?.tag.attributes.lang && /\p{Script=Hangul}/u.test(r.text))))
      )
        unsupported()
    }
  }
  return el
}

const xmlRels = new Set([
  ...[
    'officeDocument',
    'slide',
    'slideLayout',
    'slideMaster',
    'theme',
    'notesSlide',
    'notesMaster',
    'presProps',
    'viewProps',
    'tableStyles',
    'extended-properties',
    'custom-properties',
  ].map((name) => `${R}/${name}`),
  `${RELS}/metadata/core-properties`,
])
const rasterPath = /^ppt\/media\/[^/]+\.(?:png|jpe?g)$/i
const thumbnailPath = /^docProps\/thumbnail\.(?:png|jpe?g)$/
const printerPath = 'ppt/printerSettings/printerSettings1.bin'

function checkSlideList(parts: ReadonlyMap<string, Uint8Array>) {
  const xml = (path: string) => parseXml(utf8.decode(parts.get(path) ?? invalid()))
  const byOwner = new Map<string, Map<string, PartNode>>()
  const metadata = new Map<string, string>()
  for (const path of parts.keys()) {
    if (/\.rels$/i.test(path) && !path.endsWith('.rels')) unsupported()
    if (!path.endsWith('.rels')) continue
    const owner = path === '_rels/.rels' ? '' : path.replace(/(^|\/)_rels\/([^/]+)\.rels$/, '$1$2')
    if (owner && (owner === path || !parts.has(owner))) invalid()
    if (owner === printerPath || thumbnailPath.test(owner)) unsupported()
    const links = relationships(xml(path))
    for (const link of links.values()) {
      const type = attr(link, 'Type')
      if (attr(link, 'TargetMode') === 'External') {
        if (type !== `${R}/hyperlink`) unsupported()
        continue
      }
      if (!internal(link)) unsupported()
      const target = resolveTarget(owner, attr(link, 'Target')!)
      if (!target || !parts.has(target)) invalid()
      if (type === `${R}/printerSettings`) {
        if (
          owner !== 'ppt/presentation.xml' ||
          attr(link, 'Target') !== 'printerSettings/printerSettings1.bin' ||
          target !== printerPath ||
          metadata.has(target)
        )
          unsupported()
        metadata.set(target, `${MIME}printerSettings`)
      } else if (type === `${RELS}/metadata/thumbnail`) {
        if (
          owner ||
          attr(link, 'Target') !== target ||
          !thumbnailPath.test(target) ||
          metadata.has(target)
        )
          unsupported()
        metadata.set(target, target.endsWith('.png') ? 'image/png' : 'image/jpeg')
      } else if (type === `${R}/image`) {
        if (!rasterPath.test(target)) unsupported()
      } else if (!type || !xmlRels.has(type) || !/\.xml$/i.test(target)) unsupported()
    }
    byOwner.set(owner, links)
  }
  const typesRoot = xml('[Content_Types].xml')
  if (typesRoot.tag.name !== 'Types' || typesRoot.tag.uri !== TYPES) invalid()
  const types = new Map<string, string>()
  for (const node of children(typesRoot, 'Override')) {
    const name = attr(node, 'PartName'),
      type = attr(node, 'ContentType')
    if (node.tag.uri !== TYPES || !name || !type || types.has(name)) invalid()
    for (const path of metadata.keys())
      if (name !== `/${path}` && name.toLowerCase() === `/${path.toLowerCase()}`) unsupported()
    types.set(name, type)
  }
  const defaults = new Map<string, string>()
  for (const node of children(typesRoot, 'Default')) {
    const ext = attr(node, 'Extension')?.toLowerCase(),
      type = attr(node, 'ContentType')
    if (node.tag.uri !== TYPES || !ext || !type || defaults.has(ext)) invalid()
    defaults.set(ext, type)
  }
  for (const path of parts.keys()) {
    if (path !== printerPath && !thumbnailPath.test(path)) continue
    const declared = types.get(`/${path}`) ?? defaults.get(path.slice(path.lastIndexOf('.') + 1))
    if (!declared || declared !== metadata.get(path)) unsupported()
  }
  const office = [...(byOwner.get('') ?? invalid()).values()].filter(
    (r) => attr(r, 'Type') === `${R}/officeDocument`,
  )
  if (
    office.length !== 1 ||
    !internal(office[0]!) ||
    attr(office[0]!, 'Target') !== 'ppt/presentation.xml' ||
    types.get('/ppt/presentation.xml') !== `${MIME}presentation.main+xml`
  )
    unsupported()
  const pres = xml('ppt/presentation.xml')
  if (pres.tag.name !== 'p:presentation') unsupported()
  const rels = byOwner.get('ppt/presentation.xml') ?? invalid()
  const ids = new Set<string>(),
    paths = new Set<string>(),
    notes = new Set<string>()
  for (const node of one(pres, 'p:sldIdLst').children) {
    const rel = rels.get(attr(node, 'r:id') ?? ''),
      id = attr(node, 'id')
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
    if (!/^slides\/slide[1-9]\d{0,8}\.xml$/.test(target)) unsupported()
    const path = `ppt/${target}`
    if (paths.has(path) || !parts.has(path) || types.get(`/${path}`) !== `${MIME}slide+xml`)
      invalid()
    paths.add(path)
    const slideRels = byOwner.get(path)
    if (!slideRels) continue
    const links = [...slideRels.values()].filter((r) => attr(r, 'Type') === `${R}/notesSlide`)
    if (links.length > 1) invalid()
    if (!links.length) continue
    const link = links[0]!,
      note = resolveTarget(path, attr(link, 'Target') ?? '')
    if (
      !internal(link) ||
      !/^ppt\/notesSlides\/notesSlide[1-9]\d{0,8}\.xml$/.test(note) ||
      types.get(`/${note}`) !== `${MIME}notesSlide+xml` ||
      notes.has(note) ||
      xml(note).tag.name !== 'p:notes'
    )
      invalid()
    notes.add(note)
  }
  if (!paths.size) invalid()
  return [...paths]
}

const utf8 = new TextDecoder('utf-8', { fatal: true, ignoreBOM: true })

export async function openPptx(bytes: Uint8Array): Promise<ByteHistoryAdapter<SlidesOp>> {
  const zip = await JSZip.loadAsync(bytes)
  const parts = new Map<string, Uint8Array>()
  for (const entry of Object.values(zip.files)) {
    if (entry.dir) continue
    const data = await entry.async('uint8array')
    if (/\.(xml|rels)$/i.test(entry.name)) {
      const root = parseXml(utf8.decode(data))
      if (/^ppt\/slides\/slide\d+\.xml$/.test(entry.name)) {
        if (root.tag.name !== 'p:sld') unsupported()
        one(one(root, 'p:cSld'), 'p:spTree')
      }
    } else if (entry.name === printerPath) {
      // Accept the stock python-pptx template payload, not arbitrary binary printer instructions.
      if (
        createHash('sha256').update(data).digest('hex') !==
        'd7768f87e07d29634782e448ece1cddd05a52b9499254222b1190bc8f6dc579e'
      )
        unsupported()
    } else {
      if (!rasterPath.test(entry.name) && !thumbnailPath.test(entry.name)) unsupported()
      if (entry.name.toLowerCase().endsWith('.png')) {
        if (data.length < 8 || ![137, 80, 78, 71, 13, 10, 26, 10].every((v, i) => data[i] === v))
          invalid()
      } else if (data[0] !== 255 || data[1] !== 216 || data[2] !== 255) invalid()
    }
    parts.set(entry.name, data)
  }
  const paths = checkSlideList(parts)
  const opened = await openDeck(bytes)
  parts.clear()
  const checkModel = (listed: string[]) => {
    if (
      listed.length !== opened.deck.slides.length ||
      listed.some((path, i) => opened.deck.slides[i]!.path !== path)
    )
      invalid()
  }
  checkModel(paths)
  let dirty = false
  return {
    decode: decodeOp,
    encode: encodeOp,
    integrate(remote, local) {
      if (local.length) invalid()
      for (const op of remote) {
        const edit = op.edit
        const targets =
          edit.kind === 'deleteSlides'
            ? edit.slides
            : [edit.kind === 'text' ? edit.at.slide : edit.slide]
        if (targets.includes(GONE)) unsupported()
        if (
          new Set(targets).size !== targets.length ||
          targets.some((s) => slideIndexOf(opened, s) < 0)
        )
          invalid()
        if (edit.kind === 'text') textBox(opened, edit)
        if (
          edit.kind === 'notes' &&
          edit.text.split(/\r\n|\r|\n/, WIRE_LIMITS.paragraphs + 1).length > WIRE_LIMITS.paragraphs
        )
          unsupported()
        const applied = applyOp(opened, op)
        if (!applied) invalid()
        if (applied.linksDropped) unsupported()
        if (
          (edit.kind === 'blankSlide' || edit.kind === 'duplicateSlide') &&
          applied.made !== edit.made
        )
          invalid()
        if (edit.kind !== 'text') {
          const entries = opened.archive.entries
          let total = 0
          if (entries.size > 256) unsupported()
          for (const part of entries.values())
            if (part.length > 1024 * 1024 || (total += part.length) > 4 * 1024 * 1024) unsupported()
          checkModel(checkSlideList(entries))
          if (edit.kind === 'notes') {
            const slide = opened.deck.slides[slideIndexOf(opened, edit.slide)]!
            if (
              !notesPathForSlide(opened.archive, slide.path) ||
              getSlideNotes(opened.archive, slide.path) !==
                edit.text.replace(/\r\n?/g, '\n').replace(/\n+$/, '')
            )
              unsupported()
          }
        }
        dirty = true
      }
      return []
    },
    captureUndo({ edit }) {
      if (edit.kind === 'blankSlide' || edit.kind === 'duplicateSlide') {
        return (later) => {
          // Any later structural edit may have copied, removed or reused the created part.
          for (const { edit: next } of later) {
            if (next.kind === 'notes' && next.slide !== edit.made) continue
            if (
              next.kind === 'text' &&
              next.at.slide !== edit.made &&
              !next.paragraphs.some((p) =>
                p.runs.some((r) => r.link?.kind === 'slide' || r.link?.kind === 'action'),
              )
            )
              continue
            return null
          }
          return [{ id: crypto.randomUUID(), edit: { kind: 'deleteSlides', slides: [edit.made] } }]
        }
      }
      if (edit.kind !== 'text') unsupported()
      const box = textBox(opened, edit, true)
      const paragraphs = box.text!.paragraphs.map((p) => ({
        runs: p.runs.map((r) => ({ text: r.text })),
      }))
      return (later) => {
        if (
          later.some(({ edit: next }) =>
            next.kind === 'notes'
              ? false
              : next.kind !== 'text' ||
                (next.at.slide === edit.at.slide && matchesElementRef(box, next.at.el)),
          )
        )
          return null
        return [{ id: crypto.randomUUID(), edit: { kind: 'text', at: edit.at, paragraphs } }]
      }
    },
    async produceFile() {
      for (const slide of opened.deck.slides) {
        if (!slide.structureDirty && !slide.elements.some((el) => el.dirty)) continue
        const xml = patchSlideXml(structuredClone(slide))
        if (Buffer.byteLength(xml) > 1024 * 1024) unsupported()
        parseXml(xml)
      }
      return { fileName: 'history.pptx', bytes: dirty ? await deckFile(opened) : bytes }
    },
  }
}
