import { PDFBool, PDFDocument, PDFHexString, PDFName, degrees, rgb } from 'pdf-lib'
import { readAnnotSubjects } from '../../../apps/pdf/src/main/annot-subjects'
import {
  pageFingerprints,
  readPdfiumDocument,
  readPdfiumText,
  type PdfiumTextOptions,
} from './pdf/pdfium-text'
import {
  imagesToPdfBytes,
  readImageInfo,
  type ImagesToPdfOptions,
} from '../../../apps/pdf/src/main/images-to-pdf'
import {
  ANNOT_KINDS,
  stripAnnotsBytes,
  type AnnotKind,
  type StripAnnotsOptions,
} from '../../../apps/pdf/src/main/annot-strip'
import { overlayPdfBytes, type OverlayOptions } from '../../../apps/pdf/src/main/overlay'
import { duplexMergeBytes } from '../../../apps/pdf/src/main/print-prep'
import { addPdfInternalLinks, sanitizePdfLinks } from '../../../apps/docs/src/main/pdf-links'
import { decryptPdf, encryptPdf, repairPdf } from './pdf/qpdf'
import { flattenAnnotations } from './pdf/flatten'
import {
  runStructureTree,
  type NativeOcrRequest,
  type StructureRunRequest,
} from './pdf/structure-runner'
import type { OcrRecognition } from '../../../packages/pdf2docx/src/ocr'
import { encode as encodeWire } from '../bridge/wire'
import type { NativeCallback } from './transport'
import { enumeratePdfStreams, readPdfStream, replacePdfStreams } from './pdf/compress-primitives'
import { recodeImage } from './pdf/compress-primitives/recode'

const read = (s: string): Uint8Array => Uint8Array.from(atob(s), (c) => c.charCodeAt(0))
const encode = (bytes: Uint8Array): string => {
  let out = ''
  for (let at = 0; at < bytes.length; at += 8192)
    out += String.fromCharCode(...bytes.subarray(at, at + 8192))
  return btoa(out)
}
const load = (s: string) => PDFDocument.load(read(s), { updateMetadata: false })
const save = async (doc: PDFDocument) => encode(await doc.save({ useObjectStreams: false }))
const grid = (n: number) => {
  n = Math.min(Math.max(Math.floor(n), 2), 16)
  const cols = n === 2 ? 2 : Math.ceil(Math.sqrt(n))
  return { cols, rows: Math.ceil(n / cols) }
}
const rotation = (angle: number) => ((angle % 360) + 360) % 360
const rectFor = (
  rot: number,
  box: { x: number; y: number; width: number; height: number },
  l: number,
  t: number,
  r: number,
  b: number,
) => {
  const { x, y, width: w, height: h } = box
  if (rot === 90) return { x: x + t * w, y: y + l * h, w: (b - t) * w, h: (r - l) * h }
  if (rot === 180) return { x: x + (1 - r) * w, y: y + t * h, w: (r - l) * w, h: (b - t) * h }
  if (rot === 270) return { x: x + (1 - b) * w, y: y + (1 - r) * h, w: (b - t) * w, h: (r - l) * h }
  return { x: x + l * w, y: y + (1 - b) * h, w: (r - l) * w, h: (b - t) * h }
}

const PDF_OPERATIONS: Record<string, true> = {
  'pdf.readAnnotSubjects': true,
  'pdf.readPdfText': true,
  'pdf.readPdfDocument': true,
  'pdf.isImage': true,
  'pdf.fingerprints': true,
  'pdf.flattenAnnotations': true,
  'pdf.pageCount': true,
  'pdf.stitchPages': true,
  'pdf.imagesToPdf': true,
  'pdf.duplex': true,
  'pdf.overlay': true,
  'pdf.stripAnnots': true,
  'pdf.qpdfEncrypt': true,
  'pdf.qpdfDecrypt': true,
  'pdf.repair': true,
  'pdf.docsAddInternalLinks': true,
  'pdf.docsMerge': true,
  'pdf.inspectPdf': true,
  'pdf.encryptedPageCount': true,
  'pdf.readStaticFormFills': true,
  'pdf.extractPages': true,
  'pdf.insertPdf': true,
  'pdf.insertBlankPage': true,
  'pdf.splitPdf': true,
  'pdf.mergePdf': true,
  'pdf.replacePages': true,
  'pdf.setPageSize': true,
  'pdf.cropPages': true,
  'pdf.splitPages': true,
  'pdf.mergePages': true,
  'pdf.applyPageEdits': true,
  'pdf.compressEnumerate': true,
  'pdf.compressRead': true,
  'pdf.compressRecode': true,
  'pdf.compressReplace': true,
  'pdf.applyMetadata': true,
  'pdf.appendBlankPages': true,
}

export function hasPdfOperation(name: string): boolean {
  return Object.hasOwn(PDF_OPERATIONS, name)
}

function record(value: unknown, label: string): Record<string, unknown> {
  if (value === null || typeof value !== 'object' || Array.isArray(value))
    throw new TypeError(`${label} must be an object`)
  return value as Record<string, unknown>
}
function string(value: unknown, label: string): string {
  if (typeof value !== 'string') throw new TypeError(`${label} must be a string`)
  return value
}
function number(value: unknown, label: string): number {
  if (typeof value !== 'number') throw new TypeError(`${label} must be a number`)
  return value
}
function boolean(value: unknown, label: string): boolean {
  if (typeof value !== 'boolean') throw new TypeError(`${label} must be a boolean`)
  return value
}
function array(value: unknown, label: string): unknown[] {
  if (!Array.isArray(value)) throw new TypeError(`${label} must be an array`)
  return value
}

class PdfInput {
  constructor(private readonly value: Record<string, unknown>) {}
  private text(key: string): string {
    return string(this.value[key], key)
  }
  private numeric(key: string): number {
    return number(this.value[key], key)
  }
  private strings(key: string): string[] {
    return array(this.value[key], key).map((v) => string(v, key))
  }
  private optionalText(key: string): string | undefined {
    const value = this.value[key]
    return value === undefined ? undefined : string(value, key)
  }
  private nullableText(key: string): string | null {
    const value = this.value[key]
    return value === null ? null : string(value, key)
  }
  get bytesBase64() {
    return this.text('bytesBase64')
  }
  get otherBase64() {
    return this.text('otherBase64')
  }
  get othersBase64() {
    return this.strings('othersBase64')
  }
  get imagesBase64() {
    return this.strings('imagesBase64')
  }
  get partsBase64() {
    return this.strings('partsBase64')
  }
  get oddBase64() {
    return this.text('oddBase64')
  }
  get firstBase64() {
    return this.nullableText('firstBase64')
  }
  get evenBase64() {
    return this.nullableText('evenBase64')
  }
  get userPassword() {
    return this.text('userPassword')
  }
  get ownerPassword() {
    return this.optionalText('ownerPassword')
  }
  get password() {
    return this.optionalText('password')
  }
  get pages() {
    return array(this.value.pages, 'pages').map((v) => number(v, 'pages'))
  }
  get rotations(): { pageIndex: number; delta: number }[] {
    return array(this.value.rotations, 'rotations').map((entry) => {
      const value = record(entry, 'rotation')
      return {
        pageIndex: number(value.pageIndex, 'pageIndex'),
        delta: number(value.delta, 'delta'),
      }
    })
  }
  get deletedPages() {
    return array(this.value.deletedPages, 'deletedPages').map((value) =>
      number(value, 'deletedPages'),
    )
  }
  get textOptions(): PdfiumTextOptions {
    const opts: PdfiumTextOptions = {}
    for (const key of ['fromPage', 'toPage', 'charBudget'] as const) {
      if (this.value[key] !== undefined) opts[key] = number(this.value[key], key)
    }
    if (this.value.pages !== undefined) opts.pages = this.pages
    if (this.value.password !== undefined) opts.password = string(this.value.password, 'password')
    return opts
  }
  get after() {
    return this.numeric('after')
  }
  get chunkSize() {
    return this.numeric('chunkSize')
  }
  get perSheet() {
    return this.numeric('perSheet')
  }
  get perPage() {
    return this.numeric('perPage')
  }
  get width() {
    return this.numeric('width')
  }
  get height() {
    return this.numeric('height')
  }
  get multiple() {
    return this.numeric('multiple')
  }
  get separator() {
    return boolean(this.value.separator, 'separator')
  }
  get direction(): 'horizontal' | 'vertical' {
    const value = this.value.direction
    if (value !== 'horizontal' && value !== 'vertical')
      throw new TypeError('direction must be horizontal or vertical')
    return value
  }
  get rect() {
    const value = record(this.value.rect, 'rect')
    return {
      l: number(value.l, 'rect.l'),
      t: number(value.t, 'rect.t'),
      r: number(value.r, 'rect.r'),
      b: number(value.b, 'rect.b'),
    }
  }
  get links(): unknown {
    return this.value.links
  }
  get imageOptions(): ImagesToPdfOptions {
    const value = record(this.value.imageOptions, 'imageOptions')
    const paper = value.paper === null ? null : record(value.paper, 'paper')
    return {
      paper:
        paper === null ? null : { w: number(paper.w, 'paper.w'), h: number(paper.h, 'paper.h') },
      marginPt: number(value.marginPt, 'marginPt'),
    }
  }
  get overlayOptions(): OverlayOptions {
    const value = record(this.value.overlayOptions, 'overlayOptions')
    return {
      behind: boolean(value.behind, 'behind'),
      backgroundPage: number(value.backgroundPage, 'backgroundPage'),
      pages:
        value.pages === null ? null : array(value.pages, 'pages').map((v) => number(v, 'pages')),
    }
  }
  get stripOptions(): StripAnnotsOptions {
    const value = record(this.value.stripOptions, 'stripOptions')
    const kinds: AnnotKind[] = array(value.kinds, 'kinds').map((kind) => {
      if (typeof kind !== 'string' || !ANNOT_KINDS.some((known) => known === kind))
        throw new TypeError('invalid annotation kind')
      return ANNOT_KINDS.find((known) => known === kind)!
    })
    return {
      kinds,
      pages:
        value.pages === null ? null : array(value.pages, 'pages').map((v) => number(v, 'pages')),
    }
  }
  get steps() {
    return array(this.value.steps, 'steps').map(
      (step): { source: 'odd' | 'first' | 'even'; index: number; page: number } => {
        const value = record(step, 'step')
        if (value.source !== 'odd' && value.source !== 'first' && value.source !== 'even')
          throw new TypeError('invalid step source')
        return {
          source: value.source,
          index: number(value.index, 'index'),
          page: number(value.page, 'page'),
        }
      },
    )
  }
}

export interface PdfStructureCallbacks {
  ocr: ((request: NativeOcrRequest) => Promise<OcrRecognition | null>) | null
  onProgress: (done: number, total: number) => void
  signal: AbortSignal
}

export function pdfStructureOperation(
  raw: unknown,
  callbacks: PdfStructureCallbacks,
): Promise<unknown> {
  const value = record(raw, 'PDF structure input')
  const operation = value.operation
  if (
    operation !== 'tree' &&
    operation !== 'fingerprints' &&
    operation !== 'blankPages' &&
    operation !== 'images'
  ) {
    throw new TypeError('invalid PDF structure operation')
  }
  const request: StructureRunRequest = {
    operation,
    bytes: read(string(value.bytesBase64, 'bytesBase64')),
    ocr: callbacks.ocr,
    onProgress: callbacks.onProgress,
    signal: callbacks.signal,
  }
  if (value.password !== undefined) request.password = string(value.password, 'password')
  if (value.pages !== undefined)
    request.pages = array(value.pages, 'pages').map((page) => number(page, 'pages'))
  if (value.minImagePx !== undefined) request.minImagePx = number(value.minImagePx, 'minImagePx')
  return runStructureTree(request)
}
export async function pdfStructureService(
  raw: unknown,
  signal: AbortSignal,
  callback: NativeCallback,
): Promise<unknown> {
  const value = record(raw, 'PDF structure input')
  if (typeof value.ocrAvailable !== 'boolean') throw new TypeError('ocrAvailable must be a boolean')
  const pending: Promise<void>[] = []
  let progressFailure: { error: unknown } | undefined
  try {
    const result = await pdfStructureOperation(raw, {
      signal,
      ocr: value.ocrAvailable
        ? async (request) => {
            const result = await callback('pdf.ocr', {
              pngBase64: encode(request.png),
              page: request.page,
              requestId: request.requestId,
            })
            if (result === null) return null
            if (
              !result ||
              typeof result !== 'object' ||
              !('lines' in result) ||
              !Array.isArray(result.lines)
            )
              throw new Error('invalid native OCR response')
            return result as OcrRecognition
          }
        : null,
      onProgress: (done, total) => {
        pending.push(
          callback('progress', { done, total }).then(
            () => {},
            (error) => {
              progressFailure ??= { error }
            },
          ),
        )
      },
    })
    await Promise.all(pending)
    if (progressFailure !== undefined) throw progressFailure.error
    signal.throwIfAborted()
    const wire = encodeWire(result)
    return { ok: true, header: wire.header, buffersBase64: wire.buffers.map(encode) }
  } catch (error) {
    await Promise.allSettled(pending)
    signal.throwIfAborted()
    const thrown = error instanceof Error ? error : new Error(String(error))
    const extra = thrown as Error & { code?: unknown; validRange?: unknown; reason?: unknown }
    return {
      ok: false,
      name: thrown.name,
      message: thrown.message,
      code: extra.code,
      validRange: extra.validRange,
      reason: extra.reason,
    }
  }
}

export async function pdfBinaryService(
  name: string,
  raw: unknown,
  signal: AbortSignal,
): Promise<unknown> {
  if (name !== 'pdf.qpdfEncrypt' && name !== 'pdf.qpdfDecrypt' && name !== 'pdf.repair') {
    throw new Error(`Unknown PDF binary operation: ${name}`)
  }
  try {
    const bytesBase64 = await pdfPageOperation(name, raw, signal)
    return { ok: true, bytesBase64 }
  } catch (error) {
    signal.throwIfAborted()
    const thrown = error instanceof Error ? error : new Error(String(error))
    const details = thrown as Error & { code?: unknown; reason?: unknown }
    return {
      ok: false,
      name: thrown.name,
      message: thrown.message,
      code: details.code,
      reason: details.reason,
    }
  }
}

export async function pdfPageOperation(
  name: string,
  raw: unknown,
  signal?: AbortSignal,
): Promise<unknown> {
  if (!hasPdfOperation(name)) throw new Error(`Unknown PDF operation: ${name}`)
  const value = record(raw, 'PDF operation input')
  const input = new PdfInput(value)
  if (name === 'pdf.compressEnumerate') return enumeratePdfStreams(input.bytesBase64)
  if (name === 'pdf.compressRead')
    return readPdfStream(
      input.bytesBase64,
      string(value.ref, 'ref'),
      boolean(value.inflated, 'inflated'),
    )
  if (name === 'pdf.compressRecode') {
    const kind = value.kind
    if (kind !== 'jpeg' && kind !== 'raw') throw new TypeError('invalid image kind')
    const components = value.components
    if (components !== 1 && components !== 3) throw new TypeError('invalid image components')
    const result = await recodeImage({
      kind,
      data: read(string(value.dataBase64, 'dataBase64')),
      width: number(value.width, 'width'),
      height: number(value.height, 'height'),
      components,
      longSide: number(value.longSide, 'longSide'),
      quality: number(value.quality, 'quality'),
    })
    return result === null
      ? null
      : { jpegBase64: encode(result.jpeg), width: result.width, height: result.height }
  }
  if (name === 'pdf.compressReplace') {
    const replacements = array(value.replacements, 'replacements').map((item) => {
      const entry = record(item, 'replacement')
      return {
        ref: string(entry.ref, 'ref'),
        jpegBase64: string(entry.jpegBase64, 'jpegBase64'),
        width: number(entry.width, 'width'),
        height: number(entry.height, 'height'),
      }
    })
    return replacePdfStreams(input.bytesBase64, replacements)
  }
  if (name === 'pdf.readAnnotSubjects') return readAnnotSubjects(read(input.bytesBase64))
  if (name === 'pdf.readPdfText') return readPdfiumText(read(input.bytesBase64), input.password)
  if (name === 'pdf.readPdfDocument')
    return readPdfiumDocument(read(input.bytesBase64), input.textOptions)
  if (name === 'pdf.isImage') return readImageInfo(read(input.bytesBase64)) !== null
  if (name === 'pdf.fingerprints') return pageFingerprints(read(input.bytesBase64), input.password)
  if (name === 'pdf.flattenAnnotations')
    return encode(await flattenAnnotations(read(input.bytesBase64)))
  if (name === 'pdf.pageCount')
    return (await PDFDocument.load(read(input.bytesBase64))).getPageCount()
  if (name === 'pdf.stitchPages') {
    const odd = await PDFDocument.load(read(input.oddBase64))
    const first =
      input.firstBase64 === null ? undefined : await PDFDocument.load(read(input.firstBase64))
    const even =
      input.evenBase64 === null ? undefined : await PDFDocument.load(read(input.evenBase64))
    const passes = { odd, first, even }
    const merged = await PDFDocument.create()
    for (const step of input.steps) {
      const source = passes[step.source]
      const [page] =
        source !== undefined && step.index < source.getPageCount()
          ? await merged.copyPages(source, [step.index])
          : await merged.copyPages(odd, [step.page - 1])
      if (page) merged.addPage(page)
    }
    return encode(await merged.save())
  }
  if (name === 'pdf.imagesToPdf')
    return encode(await imagesToPdfBytes(input.imagesBase64.map(read), input.imageOptions))
  if (name === 'pdf.duplex')
    return encode(await duplexMergeBytes(input.imagesBase64.map(read), input.multiple))
  if (name === 'pdf.overlay')
    return encode(
      await overlayPdfBytes(read(input.bytesBase64), read(input.otherBase64), input.overlayOptions),
    )
  if (name === 'pdf.stripAnnots') {
    const result = await stripAnnotsBytes(read(input.bytesBase64), input.stripOptions)
    return { bytes: encode(result.bytes), removed: result.removed }
  }
  if (name === 'pdf.qpdfEncrypt')
    return encode(
      await encryptPdf(read(input.bytesBase64), input.userPassword, input.ownerPassword, signal),
    )
  if (name === 'pdf.qpdfDecrypt')
    return encode(await decryptPdf(read(input.bytesBase64), input.password ?? '', signal))
  if (name === 'pdf.repair') return encode(await repairPdf(read(input.bytesBase64), signal))
  if (name === 'pdf.docsAddInternalLinks') {
    if (!Array.isArray(input.links) || input.links.length === 0) return input.bytesBase64
    const doc = await PDFDocument.load(read(input.bytesBase64))
    return addPdfInternalLinks(doc, sanitizePdfLinks(input.links, doc.getPageCount())) === 0
      ? input.bytesBase64
      : encode(await doc.save())
  }
  if (name === 'pdf.docsMerge') {
    const merged = await PDFDocument.create()
    for (const part of input.partsBase64) {
      const doc = await PDFDocument.load(read(part))
      for (const page of await merged.copyPages(doc, doc.getPageIndices())) merged.addPage(page)
    }
    if (Array.isArray(input.links) && input.links.length > 0)
      addPdfInternalLinks(merged, sanitizePdfLinks(input.links, merged.getPageCount()))
    return encode(await merged.save())
  }
  if (name === 'pdf.inspectPdf' || name === 'pdf.encryptedPageCount') {
    const bytes = read(input.bytesBase64)
    try {
      const doc = await PDFDocument.load(bytes, { updateMetadata: false, ignoreEncryption: true })
      if (name === 'pdf.encryptedPageCount') return doc.getPageCount()
      if (doc.isEncrypted) return { ok: false, problem: 'encrypted' }
      const pages = doc.getPageCount()
      return pages > 0 ? { ok: true, pages } : { ok: false, problem: 'unreadable' }
    } catch {
      return name === 'pdf.encryptedPageCount' ? 0 : { ok: false, problem: 'unreadable' }
    }
  }
  const src = await load(input.bytesBase64)
  if (name === 'pdf.appendBlankPages') {
    for (const rawPage of array(value.pages, 'pages')) {
      const page = record(rawPage, 'page')
      src.addPage([number(page.width, 'width'), number(page.height, 'height')])
    }
    return encode(await src.save({ useObjectStreams: false }))
  }
  if (name === 'pdf.applyMetadata') {
    const metadata = record(value.metadata, 'metadata')
    if (metadata.title !== undefined) src.setTitle(string(metadata.title, 'title'))
    if (metadata.author !== undefined) src.setAuthor(string(metadata.author, 'author'))
    if (metadata.subject !== undefined) src.setSubject(string(metadata.subject, 'subject'))
    if (metadata.keywords !== undefined)
      src.setKeywords(array(metadata.keywords, 'keywords').map((word) => string(word, 'keyword')))
    src.setModificationDate(new Date(number(value.modifiedAt, 'modifiedAt')))
    return encode(await src.save({ useObjectStreams: false, updateFieldAppearances: false }))
  }
  switch (name) {
    case 'pdf.applyPageEdits': {
      const pages = src.getPages()
      for (const edit of input.rotations) {
        const page = pages[edit.pageIndex]
        if (page) page.setRotation(degrees((page.getRotation().angle + edit.delta) % 360))
      }
      for (const index of [...input.deletedPages].sort((a, b) => b - a)) {
        if (index >= 0 && index < src.getPageCount() && src.getPageCount() > 1)
          src.removePage(index)
      }
      if (src.catalog.has(PDFName.of('AcroForm'))) {
        const form = src.getForm()
        const font = form.getDefaultFont()
        let needAppearances = false
        for (const field of form.getFields()) {
          if (!field.needsAppearancesUpdate()) continue
          try {
            field.defaultUpdateAppearances(font)
          } catch {
            needAppearances = true
          }
        }
        if (needAppearances) form.acroForm.dict.set(PDFName.of('NeedAppearances'), PDFBool.True)
      }
      return encode(await src.save({ useObjectStreams: false, updateFieldAppearances: false }))
    }
    case 'pdf.readStaticFormFills': {
      const value = src.catalog.get(PDFName.of('GenOfficeStaticFormFills'))
      if (!(value instanceof PDFHexString)) return []
      try {
        const parsed: unknown = JSON.parse(value.decodeText())
        if (!Array.isArray(parsed)) return []
        return parsed.filter((value: unknown) => {
          if (!value || typeof value !== 'object') return false
          const record = value as Record<string, unknown>
          return (
            typeof record.id === 'string' &&
            (record.kind === 'text' || record.kind === 'check' || record.kind === 'cross') &&
            Number.isInteger(record.pageIndex) &&
            Array.isArray(record.rect) &&
            record.rect.length === 4 &&
            record.rect.every((entry) => typeof entry === 'number' && Number.isFinite(entry))
          )
        })
      } catch {
        return []
      }
    }
    case 'pdf.extractPages': {
      const out = await PDFDocument.create()
      const pages = input.pages.filter((p) => p >= 0 && p < src.getPageCount())
      for (const page of await out.copyPages(src, pages)) out.addPage(page)
      return save(out)
    }
    case 'pdf.insertPdf': {
      const other = await load(input.otherBase64)
      const pages = await src.copyPages(other, other.getPageIndices())
      let at = Math.min(Math.max(input.after + 1, 0), src.getPageCount())
      for (const page of pages) src.insertPage(at++, page)
      return { merged: await save(src), count: pages.length }
    }
    case 'pdf.insertBlankPage': {
      const at = Math.min(Math.max(input.after + 1, 0), src.getPageCount())
      const ref = src.getPage(Math.min(Math.max(input.after, 0), src.getPageCount() - 1))
      src.insertPage(at, [ref.getWidth(), ref.getHeight()]).setRotation(ref.getRotation())
      return save(src)
    }
    case 'pdf.splitPdf': {
      const out: string[] = []
      const size = Math.max(1, Math.floor(input.chunkSize))
      for (let start = 0; start < src.getPageCount(); start += size) {
        const part = await PDFDocument.create()
        const count = Math.min(size, src.getPageCount() - start)
        for (const page of await part.copyPages(
          src,
          Array.from({ length: count }, (_, i) => start + i),
        ))
          part.addPage(page)
        out.push(await save(part))
      }
      return out
    }
    case 'pdf.mergePdf': {
      let appended = 0
      for (const bytes of input.othersBase64) {
        const other = await load(bytes)
        const pages = await src.copyPages(other, other.getPageIndices())
        for (const page of pages) src.addPage(page)
        appended += pages.length
      }
      return { merged: await save(src), appended }
    }
    case 'pdf.replacePages': {
      const other = await load(input.otherBase64)
      const valid = [...new Set(input.pages.filter((p) => p >= 0 && p < src.getPageCount()))].sort(
        (a, b) => a - b,
      )
      if (!valid.length) throw new Error('replacePages: no valid pages to replace')
      const at = valid[0]!
      for (const page of [...valid].reverse()) src.removePage(page)
      const copied = await src.copyPages(other, other.getPageIndices())
      let i = Math.min(at, src.getPageCount())
      for (const page of copied) src.insertPage(i++, page)
      return { merged: await save(src), removed: valid.length, inserted: copied.length }
    }
    case 'pdf.setPageSize': {
      for (const page of src.getPages()) {
        const rot = rotation(page.getRotation().angle)
        const tw = rot === 90 || rot === 270 ? input.height : input.width
        const th = rot === 90 || rot === 270 ? input.width : input.height
        const { x, y, width: w, height: h } = page.getMediaBox()
        if (w === tw && h === th) continue
        const k = Math.min(tw / w, th / h)
        page.scaleContent(k, k)
        page.scaleAnnotations(k, k)
        const bx = x * k - (tw - w * k) / 2
        const by = y * k - (th - h * k) / 2
        page.setMediaBox(bx, by, tw, th)
        page.setCropBox(bx, by, tw, th)
      }
      return save(src)
    }
    case 'pdf.cropPages': {
      const l = Math.min(Math.max(input.rect.l, 0), 1)
      const t = Math.min(Math.max(input.rect.t, 0), 1)
      const r = Math.min(Math.max(input.rect.r, l), 1)
      const b = Math.min(Math.max(input.rect.b, t), 1)
      if (r - l <= 0 || b - t <= 0) throw new Error('cropPages: empty crop rect')
      for (const index of input.pages) {
        if (index < 0 || index >= src.getPageCount()) continue
        const page = src.getPage(index)
        const rect = rectFor(rotation(page.getRotation().angle), page.getCropBox(), l, t, r, b)
        page.setCropBox(rect.x, rect.y, rect.w, rect.h)
      }
      return save(src)
    }
    case 'pdf.splitPages': {
      const out = await PDFDocument.create()
      const { cols, rows } = grid(input.perPage)
      for (let i = 0; i < src.getPageCount(); i++) {
        const copies = await out.copyPages(
          src,
          Array.from({ length: input.perPage }, () => i),
        )
        for (let c = 0; c < input.perPage; c++) {
          const page = copies[c]!
          const col = c % cols,
            row = Math.floor(c / cols)
          const rect = rectFor(
            rotation(page.getRotation().angle),
            page.getCropBox(),
            col / cols,
            row / rows,
            (col + 1) / cols,
            (row + 1) / rows,
          )
          page.setMediaBox(rect.x, rect.y, rect.w, rect.h)
          page.setCropBox(rect.x, rect.y, rect.w, rect.h)
          out.addPage(page)
        }
      }
      return save(out)
    }
    case 'pdf.mergePages': {
      const n = Math.min(Math.max(Math.floor(input.perSheet), 2), 16)
      const { cols, rows } = grid(n)
      const out = await PDFDocument.create()
      const first = src.getPage(0)
      const firstRot = rotation(Math.round(first.getRotation().angle / 90) * 90)
      const turns = firstRot === 90 || firstRot === 270
      const fw = turns ? first.getHeight() : first.getWidth()
      const fh = turns ? first.getWidth() : first.getHeight()
      const sw = n === 2 ? fh : fw,
        sh = n === 2 ? fw : fh
      for (const page of src.getPages())
        if (!page.node.Contents())
          page.node.set(PDFName.of('Contents'), src.context.register(src.context.stream('')))
      const embedded = await out.embedPages(src.getPages())
      const cw = sw / cols,
        ch = sh / rows
      for (let start = 0; start < src.getPageCount(); start += n) {
        const sheet = out.addPage([sw, sh])
        for (let i = 0; i < n && start + i < src.getPageCount(); i++) {
          const ep = embedded[start + i]!
          const rot = rotation(Math.round(src.getPage(start + i).getRotation().angle / 90) * 90)
          const turn = rot === 90 || rot === 270
          const scale = Math.min(
            cw / (turn ? ep.height : ep.width),
            ch / (turn ? ep.width : ep.height),
          )
          const w = ep.width * scale,
            h = ep.height * scale
          const col = input.direction === 'vertical' ? Math.floor(i / rows) : i % cols
          const row = input.direction === 'vertical' ? i % rows : Math.floor(i / cols)
          const cx = col * cw,
            cy = sh - (row + 1) * ch
          const x =
            rot === 90
              ? cx + (cw - h) / 2
              : rot === 180
                ? cx + (cw + w) / 2
                : rot === 270
                  ? cx + (cw + h) / 2
                  : cx + (cw - w) / 2
          const y =
            rot === 90
              ? cy + (ch + w) / 2
              : rot === 180
                ? cy + (ch + h) / 2
                : rot === 270
                  ? cy + (ch - w) / 2
                  : cy + (ch - h) / 2
          sheet.drawPage(ep, {
            x,
            y,
            xScale: scale,
            yScale: scale,
            rotate: degrees((360 - rot) % 360),
          })
        }
        if (input.separator) {
          const line = { thickness: 0.75, color: rgb(0.62, 0.62, 0.62) }
          for (let c = 1; c < cols; c++)
            sheet.drawLine({ start: { x: c * cw, y: 0 }, end: { x: c * cw, y: sh }, ...line })
          for (let r = 1; r < rows; r++)
            sheet.drawLine({ start: { x: 0, y: r * ch }, end: { x: sw, y: r * ch }, ...line })
        }
      }
      return save(out)
    }
    default:
      throw new Error(`Unknown PDF page operation: ${name}`)
  }
}
