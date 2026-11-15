import { init } from '@embedpdf/pdfium'

type Kind = 'doc' | 'page' | 'text' | 'object' | 'font' | 'bitmap' | 'annot' | 'form' | 'formInfo' | 'link' | 'action' | 'dest' | 'clip' | 'segment' | 'tree' | 'treeChild' | 'writer' | 'xobject' | 'buffer' | 'memory'
type Arg = 'n' | 'buf' | Kind
type Spec = { args: readonly Arg[]; result?: Kind; close?: Kind; optional?: boolean }
interface Pdfium {
  HEAPU8: Uint8Array
  HEAPU32: Uint32Array
  _malloc(size: number): number
  _free(ptr: number): void
  _PDFiumExt_Init(): void
  _FPDF_LoadMemDocument(ptr: number, size: number, password: number): number
  _FPDF_GetLastError(): number
  _FPDF_CloseDocument(doc: number): void
  [key: string]: unknown
}
interface Entry { kind: Kind; ptr: number; owner?: number; size?: number; owned?: boolean; holds?: number[] }
interface Ref { ref: number; offset?: number }
const specs: Record<string, Spec> = {
  FPDF_GetLastError: { args: [] }, FPDF_CloseDocument: { args: ['doc'], close: 'doc' },
  FPDF_LoadMemDocument: { args: ['buf','n','buf'], result: 'doc' },
  FPDF_CreateNewDocument: { args: [], result: 'doc' }, FPDF_GetSecurityHandlerRevision: { args: ['doc'] },
  FPDFPage_New: { args: ['doc','n','n','n'], result: 'page' },
  FPDFText_LoadFont: { args: ['doc','buf','n','n','n'], result: 'font' },
  FPDFText_LoadStandardFont: { args: ['doc','buf'], result: 'font' },
  FPDFText_SetText: { args: ['object','buf'] }, FPDFPageObj_CreateTextObj: { args: ['doc','font','n'], result: 'object' },
  FPDFPageObj_CreateNewRect: { args: ['n','n','n','n'], result: 'object' },
  FPDFPageObj_SetFillColor: { args: ['object','n','n','n','n'] }, FPDFPageObj_SetStrokeColor: { args: ['object','n','n','n','n'] },
  FPDFPageObj_SetStrokeWidth: { args: ['object','n'] }, FPDFPath_SetDrawMode: { args: ['object','n','n'] },
  FPDFPageObj_SetLineJoin: { args: ['object','n'] }, FPDFPage_InsertObjectAtIndex: { args: ['page','object','n'] },
  FPDFTextObj_SetTextRenderMode: { args: ['object','n'] }, FPDFPage_InsertObject: { args: ['page','object'] },
  FPDFPage_GenerateContent: { args: ['page'] }, PDFiumExt_OpenFileWriter: { args: [], result: 'writer' },
  PDFiumExt_SaveAsCopy: { args: ['doc','writer'] }, PDFiumExt_GetFileWriterSize: { args: ['writer'] },
  PDFiumExt_GetFileWriterData: { args: ['writer','buf','n'] }, PDFiumExt_CloseFileWriter: { args: ['writer'], close: 'writer' },
  FPDF_NewXObjectFromPage: { args: ['doc','doc','n'], result: 'xobject' },
  FPDF_NewFormObjectFromXObject: { args: ['xobject'], result: 'object' },
  FPDF_CloseXObject: { args: ['xobject'], close: 'xobject' }, FPDFPageObj_Transform: { args: ['object','n','n','n','n','n','n'] },
  FPDFPageObj_TransformClipPath: { args: ['object','n','n','n','n','n','n'] },
  FPDF_GetPageCount: { args: ['doc'] },
  FPDF_GetMetaText: { args: ['doc','buf','buf','n'] },
  FPDF_LoadPage: { args: ['doc','n'], result: 'page' }, FPDF_ClosePage: { args: ['page'], close: 'page' },
  FPDF_GetPageWidthF: { args: ['page'] }, FPDF_GetPageHeightF: { args: ['page'] }, FPDFPage_GetRotation: { args: ['page'] },
  FPDFPage_GetMediaBox: { args: ['page','buf','buf','buf','buf'] }, FPDFPage_GetCropBox: { args: ['page','buf','buf','buf','buf'] },
  FPDFText_LoadPage: { args: ['page'], result: 'text' }, FPDFText_ClosePage: { args: ['text'], close: 'text' },
  FPDFText_CountChars: { args: ['text'] }, FPDFText_GetUnicode: { args: ['text','n'] },
  FPDFText_GetCharBox: { args: ['text','n','buf','buf','buf','buf'] }, FPDFText_GetLooseCharBox: { args: ['text','n','buf'] },
  FPDFText_GetCharOrigin: { args: ['text','n','buf','buf'] }, FPDFText_GetCharAngle: { args: ['text','n'] },
  FPDFText_GetFontSize: { args: ['text','n'] }, FPDFText_GetFontInfo: { args: ['text','n','buf','n','buf'] },
  FPDFText_GetFontWeight: { args: ['text','n'] }, FPDFText_GetFillColor: { args: ['text','n','buf','buf','buf','buf'] },
  FPDFText_GetStrokeColor: { args: ['text','n','buf','buf','buf','buf'] }, FPDFText_GetMatrix: { args: ['text','n','buf'] },
  FPDFText_IsGenerated: { args: ['text','n'] }, FPDFText_IsHyphen: { args: ['text','n'] },
  FPDFText_GetTextObject: { args: ['text','n'], result: 'object' }, FPDFTextObj_GetTextRenderMode: { args: ['object'] },
  FPDFTextObj_GetText: { args: ['object','text','buf','n'] }, FPDFTextObj_GetFontSize: { args: ['object','buf'] },
  FPDFText_CountRects: { args: ['text','n','n'] }, FPDFText_GetRect: { args: ['text','n','buf','buf','buf','buf'] },
  FPDFText_GetBoundedText: { args: ['text','n','n','n','n','buf','n'] }, FPDFText_GetText: { args: ['text','n','n','buf'] },
  FPDFPage_CountObjects: { args: ['page'] }, FPDFPage_GetObject: { args: ['page','n'], result: 'object' },
  FPDFPage_RemoveObject: { args: ['page','object'] }, FPDFPageObj_Destroy: { args: ['object'], close: 'object' },
  FPDFPageObj_GetType: { args: ['object'] }, FPDFPageObj_GetBounds: { args: ['object','buf','buf','buf','buf'] },
  FPDFPageObj_GetMatrix: { args: ['object','buf'] }, FPDFPageObj_SetMatrix: { args: ['object','buf'] },
  FPDFPageObj_GetFillColor: { args: ['object','buf','buf','buf','buf'] }, FPDFPageObj_GetStrokeColor: { args: ['object','buf','buf','buf','buf'] },
  FPDFPageObj_GetStrokeWidth: { args: ['object','buf'] },
  FPDFFormObj_CountObjects: { args: ['object'], optional: true }, FPDFFormObj_GetObject: { args: ['object','n'], result: 'object', optional: true },
  FPDFFormObj_RemoveObject: { args: ['object','object'] },
  EPDFPage_GetAnnotByObjectNumber: { args: ['page','n'], result: 'annot' }, EPDFPage_RemoveAnnotByObjectNumber: { args: ['page','n'] },
  FPDFPage_RemoveAnnot: { args: ['page','n'] },
  FPDFPage_CreateAnnot: { args: ['page','n'], result: 'annot' },
  FPDFAnnot_SetRect: { args: ['annot','buf'] }, EPDFAnnot_SetColor: { args: ['annot','n','n','n','n'] },
  EPDFAnnot_ApplyRedaction: { args: ['page','annot'] },
  FPDFPage_GetAnnotCount: { args: ['page'], optional: true }, FPDFPage_GetAnnot: { args: ['page','n'], result: 'annot', optional: true },
  FPDFPage_CloseAnnot: { args: ['annot'], close: 'annot', optional: true }, FPDFAnnot_GetSubtype: { args: ['annot'], optional: true },
  FPDFAnnot_GetRect: { args: ['annot','buf'], optional: true }, FPDFAnnot_GetFormFieldType: { args: ['form','annot'], optional: true },
  FPDFAnnot_IsChecked: { args: ['form','annot'], optional: true }, FPDFAnnot_GetStringValue: { args: ['annot','buf','buf','n'], optional: true },
  FPDFAnnot_GetFormFieldValue: { args: ['form','annot','buf','n'], optional: true }, FPDFAnnot_GetFormFieldFlags: { args: ['form','annot'], optional: true },
  FPDFAnnot_GetAP: { args: ['annot','n','buf','n'] }, FPDFAnnot_GetFlags: { args: ['annot'] },
  FPDFAnnot_GetColor: { args: ['annot','n','buf','buf','buf','buf'] },
  FPDFAnnot_GetFontColor: { args: ['form','annot','buf','buf','buf'] },
  FPDFAnnot_GetFormFieldName: { args: ['form','annot','buf','n'] },
  FPDFAnnot_GetFontSize: { args: ['form','annot','buf'], optional: true }, FORM_OnAfterLoadPage: { args: ['page','form'], optional: true },
  FORM_OnBeforeClosePage: { args: ['page','form'], optional: true },
  FPDFLink_Enumerate: { args: ['page','buf','buf'], optional: true }, FPDFLink_GetAnnotRect: { args: ['link','buf'], optional: true },
  FPDFLink_GetAction: { args: ['link'], result: 'action', optional: true }, FPDFLink_GetDest: { args: ['doc','link'], result: 'dest', optional: true },
  FPDFAction_GetType: { args: ['action'], optional: true }, FPDFAction_GetDest: { args: ['doc','action'], result: 'dest', optional: true },
  FPDFAction_GetURIPath: { args: ['doc','action','buf','n'], optional: true }, FPDFDest_GetDestPageIndex: { args: ['doc','dest'], optional: true },
  PDFiumExt_OpenFormFillInfo: { args: [], result: 'formInfo', optional: true }, PDFiumExt_InitFormFillEnvironment: { args: ['doc','formInfo'], result: 'form', optional: true },
  PDFiumExt_ExitFormFillEnvironment: { args: ['form'], close: 'form', optional: true }, PDFiumExt_CloseFormFillInfo: { args: ['formInfo'], close: 'formInfo', optional: true },
  FPDFImageObj_GetImageDataDecoded: { args: ['object','buf','n'] }, FPDFImageObj_GetImageDataRaw: { args: ['object','buf','n'] },
  FPDFImageObj_GetImageFilterCount: { args: ['object'] }, FPDFImageObj_GetImageFilter: { args: ['object','n','buf','n'] },
  FPDFImageObj_GetImageMetadata: { args: ['object','page','buf'] }, FPDFImageObj_GetImagePixelSize: { args: ['object','buf','buf'] },
  FPDFImageObj_GetBitmap: { args: ['object'], result: 'bitmap' }, FPDFImageObj_GetRenderedBitmap: { args: ['doc','page','object'], result: 'bitmap' },
  FPDFBitmap_Create: { args: ['n','n','n'], result: 'bitmap' }, FPDFBitmap_Destroy: { args: ['bitmap'], close: 'bitmap' },
  FPDFBitmap_CreateEx: { args: ['n','n','n','buf','n'], result: 'bitmap' },
  FPDFPageObj_NewImageObj: { args: ['doc'], result: 'object' },
  FPDFImageObj_SetBitmap: { args: ['buf','n','object','bitmap'] },
  FPDFBitmap_FillRect: { args: ['bitmap','n','n','n','n','n'] }, FPDFBitmap_GetBuffer: { args: ['bitmap'], result: 'memory' },
  FPDFBitmap_GetFormat: { args: ['bitmap'] }, FPDFBitmap_GetWidth: { args: ['bitmap'] }, FPDFBitmap_GetHeight: { args: ['bitmap'] }, FPDFBitmap_GetStride: { args: ['bitmap'] },
  FPDF_RenderPageBitmap: { args: ['bitmap','page','n','n','n','n','n','n'] },
  FPDFPath_CountSegments: { args: ['object'] }, FPDFPath_GetPathSegment: { args: ['object','n'], result: 'segment' },
  FPDFPathSegment_GetType: { args: ['segment'] }, FPDFPathSegment_GetPoint: { args: ['segment','buf','buf'] }, FPDFPathSegment_GetClose: { args: ['segment'] },
  FPDFPath_GetDrawMode: { args: ['object','buf','buf'] }, FPDFPageObj_GetClipPath: { args: ['object'], result: 'clip', optional: true },
  FPDFClipPath_CountPaths: { args: ['clip'], optional: true }, FPDFClipPath_CountPathSegments: { args: ['clip','n'], optional: true },
  FPDFClipPath_GetPathSegment: { args: ['clip','n','n'], result: 'segment', optional: true },
  FPDFTextObj_GetFont: { args: ['object'], result: 'font' }, FPDFFont_GetFamilyName: { args: ['font','buf','n'] },
  FPDFFont_GetBaseFontName: { args: ['font','buf','n'] }, FPDFFont_GetWeight: { args: ['font'] },
  FPDFFont_GetItalicAngle: { args: ['font','buf'] }, FPDFFont_GetFlags: { args: ['font'] },
  FPDFFont_GetAscent: { args: ['font','n','buf'] }, FPDFFont_GetDescent: { args: ['font','n','buf'] },
  FPDFFont_GetGlyphWidth: { args: ['font','n','n','buf'] },
  FPDFFont_GetIsEmbedded: { args: ['font'] }, FPDFFont_GetFontData: { args: ['font','buf','n','buf'] },
  FPDF_StructTree_GetForPage: { args: ['page'], result: 'tree' }, FPDF_StructTree_Close: { args: ['tree'], close: 'tree' },
  FPDF_StructTree_CountChildren: { args: ['tree'] }, FPDF_StructTree_GetChildAtIndex: { args: ['tree','n'], result: 'treeChild' },
}
const destructors: Partial<Record<Kind, string>> = { doc: 'FPDF_CloseDocument', page: 'FPDF_ClosePage', text: 'FPDFText_ClosePage', object: 'FPDFPageObj_Destroy', bitmap: 'FPDFBitmap_Destroy', annot: 'FPDFPage_CloseAnnot', tree: 'FPDF_StructTree_Close', form: 'PDFiumExt_ExitFormFillEnvironment', formInfo: 'PDFiumExt_CloseFormFillInfo', writer: 'PDFiumExt_CloseFileWriter', xobject: 'FPDF_CloseXObject' }
const worker = self as unknown as { onmessage: ((event: MessageEvent<unknown>) => void) | null; postMessage(value: unknown): void }
let modulePromise: Promise<Pdfium> | undefined
const entries = new Map<number, Entry>()
const identities = new Map<string, number>()
const seed = new Uint32Array(1)
crypto.getRandomValues(seed)
let next = seed[0] || 1
let issued = 0
let documentBytes = 0
let documentPassword = 0
let documentId = 0

function ref(id: unknown): Ref {
  if (!id || typeof id !== 'object' || !('ref' in id) || !Number.isSafeInteger(id.ref) || Number(id.ref) <= 0) throw new TypeError('Invalid PDFium reference')
  if ('offset' in id && (!Number.isSafeInteger(id.offset) || Number(id.offset) < 0)) throw new TypeError('Invalid PDFium reference offset')
  return id as Ref
}
function entry(id: number, kind?: Kind): Entry {
  const item = entries.get(id)
  if (!item || (kind !== undefined && item.kind !== kind)) throw new Error('Stale or incompatible PDFium reference')
  return item
}
function add(kind: Kind, ptr: number, owner?: number, size?: number): number {
  if (issued === 0xffff_ffff) throw new Error('PDFium references exhausted')
  const id = next
  next = (next + 1) >>> 0
  if (next === 0) next = 1
  issued++
  entries.set(id, { kind, ptr, ...(owner === undefined ? {} : { owner }), ...(size === undefined ? {} : { size }), owned: true })
  identities.set(`${kind}:${ptr}`, id)
  return id
}
function borrowed(kind: Kind, ptr: number, owner?: number, size?: number): number {
  const existing = identities.get(`${kind}:${ptr}`)
  if (existing !== undefined) return existing
  const id = add(kind, ptr, owner, size)
  entry(id).owned = false
  return id
}
function byteRange(id: number, offset: number, size: number): { ptr: number; size: number } {
  const item = entry(id)
  if (item.kind !== 'buffer' && item.kind !== 'memory') throw new Error('Expected a PDFium buffer')
  if (!Number.isSafeInteger(offset) || !Number.isSafeInteger(size) || offset < 0 || size < 0 || item.size === undefined || offset > item.size || size > item.size - offset) throw new RangeError('PDFium buffer range is outside its allocation')
  return { ptr: item.ptr + offset, size }
}
function bytes64(bytes: Uint8Array): { type: 'bytes'; base64: string } {
  let out = ''
  for (let i = 0; i < bytes.length; i += 0x8000) out += String.fromCharCode(...bytes.subarray(i, i + 0x8000))
  return { type: 'bytes', base64: btoa(out) }
}
function decode64(value: unknown): Uint8Array {
  if (!value || typeof value !== 'object' || !('type' in value) || value.type !== 'bytes' || !('base64' in value) || typeof value.base64 !== 'string') throw new TypeError('Invalid typed PDFium bytes')
  const s = atob(value.base64)
  return Uint8Array.from(s, c => c.charCodeAt(0))
}
async function pdfium(): Promise<Pdfium> {
  modulePromise ??= (async () => {
    const response = await fetch(new URL('../pdfium.wasm', self.location.href))
    if (!response.ok) throw new Error(`Cannot load PDFium: ${response.status}`)
    const initialize = init as unknown as (overrides: object) => Promise<{ pdfium: Pdfium } | Pdfium>
    const wrapped = await initialize({ wasmBinary: await response.arrayBuffer(), thisProgram: 'seuoffice-pdf' })
    const m = 'pdfium' in wrapped ? wrapped.pdfium : wrapped
    m._PDFiumExt_Init()
    return m
  })()
  return modulePromise
}
function invoke(m: Pdfium, name: string, args: number[]): number {
  const fn = m[`_${name}`]
  if (typeof fn !== 'function') throw new Error(`PDFium primitive ${name} is unavailable`)
  const value: unknown = Reflect.apply(fn, m, args)
  if (value === undefined) return 0
  if (typeof value !== 'number') throw new Error(`Invalid PDFium primitive result for ${name}`)
  return value
}
function release(m: Pdfium, id: number): void {
  if ([...entries].some(([other, item]) => item.holds?.includes(id) && !belongsTo(other, id))) {
    throw new Error('PDFium resource is still in use')
  }
  const item = entry(id)
  for (const [child, value] of [...entries]) if (value.owner === id) release(m, child)
  if (item.kind === 'buffer') m._free(item.ptr)
  else if (item.kind !== 'memory' && item.owned && destructors[item.kind]) invoke(m, destructors[item.kind]!, [item.ptr])
  if (identities.get(`${item.kind}:${item.ptr}`) === id) identities.delete(`${item.kind}:${item.ptr}`)
  entries.delete(id)
}

function belongsTo(id: number, ancestor: number): boolean {
  let current: number | undefined = id
  while (current !== undefined) {
    if (current === ancestor) return true
    current = entries.get(current)?.owner
  }
  return false
}
function documentOf(id: number): number | undefined {
  let current: number | undefined = id
  while (current !== undefined) {
    const item = entry(current)
    if (item.kind === 'doc') return current
    current = item.owner
  }
  return undefined
}
function scalar(value: unknown): number {
  if (typeof value === 'number' && Number.isFinite(value)) return value
  if (value && typeof value === 'object' && 'type' in value && value.type === 'number' && 'value' in value) {
    if (value.value === 'NaN') return NaN
    if (value.value === 'Infinity') return Infinity
    if (value.value === '-Infinity') return -Infinity
    if (value.value === '-0') return -0
  }
  throw new TypeError('Invalid PDFium scalar')
}
function wireNumber(value: number): number | { type: 'number'; value: string } {
  if (Number.isNaN(value)) return { type: 'number', value: 'NaN' }
  if (value === Infinity) return { type: 'number', value: 'Infinity' }
  if (value === -Infinity) return { type: 'number', value: '-Infinity' }
  if (Object.is(value, -0)) return { type: 'number', value: '-0' }
  return value
}

const fixedBuffers: Record<string, Record<number, number>> = {
  FPDFPage_GetMediaBox: { 1:4,2:4,3:4,4:4 }, FPDFPage_GetCropBox: { 1:4,2:4,3:4,4:4 },
  FPDFText_GetCharBox: { 2:8,3:8,4:8,5:8 }, FPDFText_GetLooseCharBox: { 2:16 },
  FPDFText_GetCharOrigin: { 2:8,3:8 }, FPDFText_GetFontInfo: { 4:4 },
  FPDFText_GetFillColor: { 2:4,3:4,4:4,5:4 }, FPDFText_GetStrokeColor: { 2:4,3:4,4:4,5:4 },
  FPDFText_GetMatrix: { 2:24 }, FPDFText_GetRect: { 2:8,3:8,4:8,5:8 },
  FPDFPageObj_GetBounds: { 1:4,2:4,3:4,4:4 }, FPDFPageObj_GetMatrix: { 1:24 }, FPDFPageObj_SetMatrix: { 1:24 },
  FPDFPageObj_GetFillColor: { 1:4,2:4,3:4,4:4 }, FPDFPageObj_GetStrokeColor: { 1:4,2:4,3:4,4:4 },
  FPDFPageObj_GetStrokeWidth: { 1:4 }, FPDFAnnot_GetRect: { 1:16 }, FPDFAnnot_GetFontSize: { 2:4 },
  FPDFAnnot_SetRect: { 1:16 },
  FPDFAnnot_GetColor: { 2:4,3:4,4:4,5:4 }, FPDFAnnot_GetFontColor: { 2:4,3:4,4:4 },
  FPDFLink_Enumerate: { 1:4,2:4 }, FPDFLink_GetAnnotRect: { 1:16 }, FPDFImageObj_GetImageMetadata: { 2:28 },
  FPDFImageObj_GetImagePixelSize: { 1:4,2:4 }, FPDFPathSegment_GetPoint: { 1:4,2:4 },
  FPDFPath_GetDrawMode: { 1:4,2:4 }, FPDFFont_GetItalicAngle: { 1:4 }, FPDFFont_GetAscent: { 2:4 },
  FPDFFont_GetDescent: { 2:4 }, FPDFFont_GetFontData: { 3:4 },
  FPDFTextObj_GetFontSize: { 1:4 }, FPDFFont_GetGlyphWidth: { 3:4 },
}
const sizedBuffers: Record<string, Record<number, [number, number]>> = {
  FPDF_LoadMemDocument: { 0:[1,1] }, FPDF_GetMetaText: { 2:[3,1] }, FPDFText_GetFontInfo: { 2:[3,1] },
  FPDFText_GetBoundedText: { 5:[6,2] }, FPDFText_GetText: { 3:[2,2] },
  FPDFAnnot_GetStringValue: { 2:[3,1] }, FPDFAnnot_GetFormFieldValue: { 2:[3,1] },
  FPDFAnnot_GetAP: { 2:[3,1] }, FPDFAnnot_GetFormFieldName: { 2:[3,1] },
  FPDFAction_GetURIPath: { 2:[3,1] }, FPDFImageObj_GetImageDataDecoded: { 1:[2,1] },
  FPDFImageObj_GetImageDataRaw: { 1:[2,1] }, FPDFImageObj_GetImageFilter: { 2:[3,1] },
  FPDFFont_GetFamilyName: { 1:[2,1] }, FPDFFont_GetBaseFontName: { 1:[2,1] }, FPDFFont_GetFontData: { 1:[2,1] },
  FPDFText_LoadFont: { 1:[2,1] }, PDFiumExt_GetFileWriterData: { 1:[2,1] },
  FPDFTextObj_GetText: { 2:[3,1] }, FPDFImageObj_SetBitmap: { 0:[1,4] },
}
const terminatedBuffers: Record<string, Record<number, 1 | 2>> = {
  FPDF_LoadMemDocument: { 2:1 }, FPDF_GetMetaText: { 1:1 }, FPDFAnnot_GetStringValue: { 1:1 }, FPDFText_SetText: { 1:2 },
  FPDFText_LoadStandardFont: { 1:1 },
}
function bitmapBytes(input: unknown[]): number {
  const width = scalar(input[0]), height = scalar(input[1]), format = scalar(input[2]), stride = scalar(input[4])
  const channels = format === 1 ? 1 : format === 2 ? 3 : format === 3 || format === 4 ? 4 : 0
  if (!Number.isSafeInteger(width) || width <= 0 || width > 0x7fff_ffff ||
      !Number.isSafeInteger(height) || height <= 0 || height > 0x7fff_ffff ||
      !Number.isSafeInteger(stride) || stride < 0 || stride > 0x7fff_ffff || !channels ||
      (input[3] !== 0 && stride < width * channels) ||
      !Number.isSafeInteger(stride * height) || stride * height > 0xffff_ffff) throw new RangeError('Invalid PDFium bitmap dimensions')
  return stride * height
}
function checkedBuffer(m: Pdfium, name: string, i: number, target: Ref, input: unknown[]): number {
  const offset = target.offset ?? 0
  const fixed = fixedBuffers[name]?.[i]
  const sized = sizedBuffers[name]?.[i]
  const terminated = terminatedBuffers[name]?.[i]
  if (name === 'FPDFBitmap_CreateEx' && i === 3) return byteRange(target.ref, offset, bitmapBytes(input)).ptr
  let length = fixed
  if (sized) {
    const count = scalar(input[sized[0]])
    if (!Number.isSafeInteger(count) || count < 0 || count > 0xffff_ffff || count * sized[1] > 0xffff_ffff) throw new RangeError('Invalid PDFium buffer size')
    length = count * sized[1] + (name === 'FPDFText_GetText' && count > 0 ? 2 : 0)
  }
  if (terminated) {
    const item = entry(target.ref)
    const remaining = (item.size ?? -1) - offset
    const range = byteRange(target.ref, offset, remaining)
    let found = false
    for (let p = 0; p + terminated <= remaining; p += terminated) {
      if (m.HEAPU8[range.ptr + p] === 0 && (terminated === 1 || m.HEAPU8[range.ptr + p + 1] === 0)) { found = true; break }
    }
    if (!found) throw new RangeError('PDFium string is not terminated within its buffer')
    return range.ptr
  }
  if (length === undefined) throw new Error(`Missing buffer contract for ${name}`)
  return byteRange(target.ref, offset, length).ptr
}
function call(m: Pdfium, requested: string, input: unknown[]): unknown {
  const name = requested.startsWith('_') ? requested.slice(1) : requested
  const spec = Object.hasOwn(specs, name) ? specs[name] : undefined
  if (!spec) throw new Error(`PDFium primitive ${name} is not permitted`)
  if (typeof m[`_${name}`] !== 'function' && spec.optional) throw new Error(`PDFium optional primitive ${name} is unavailable`)
  if (input.length !== spec.args.length) throw new TypeError(`Invalid ${name} argument count`)
  if (name === 'FPDFBitmap_CreateEx') bitmapBytes(input)
  if (name === 'FPDFImageObj_SetBitmap' && (!Number.isSafeInteger(scalar(input[1])) || scalar(input[1]) < 0 || scalar(input[1]) > 0x3fff_ffff)) throw new RangeError('Invalid PDFium page count')
  let owner: number | undefined
  const args = spec.args.map((kind, i) => {
    const value = input[i]
    if (kind === 'n') return scalar(value)
    if (value === 0) {
      if (kind === 'buf') {
        const sized = sizedBuffers[name]?.[i]
        if ((sized && scalar(input[sized[0]]) === 0) || (name === 'FPDF_LoadMemDocument' && i === 2) || (name === 'FPDFBitmap_CreateEx' && i === 3)) return 0
      }
      if (kind === 'form' && name.startsWith('FPDFAnnot_')) return 0
      throw new TypeError(`Null reference is not valid for ${name}`)
    }
    const target = ref(value)
    if (kind === 'buf') return checkedBuffer(m, name, i, target, input)
    if (target.offset !== undefined && target.offset !== 0) throw new TypeError('Handle offsets are not allowed')
    const item = entry(target.ref, kind)
    owner ??= target.ref
    return item.ptr
  })
  if (spec.close) {
    const handle = ref(input[0]).ref
    if (spec.close === 'object' && !entry(handle, 'object').owned) throw new Error('Cannot destroy a page-owned PDFium object')
    release(m, handle)
    return 0
  }
  if (name === 'EPDFAnnot_ApplyRedaction') {
    const page = ref(input[0]).ref, annot = ref(input[1]).ref
    if (entry(annot, 'annot').owner !== page) throw new Error('PDFium annotation does not belong to this page')
    for (const [id, item] of entries) {
      if (id !== annot && belongsTo(id, page) && (item.kind === 'annot' || item.kind === 'text' || item.kind === 'tree' || item.kind === 'link')) {
        throw new Error('Close page annotations, text pages and trees before redaction')
      }
    }
  }
  if ((name === 'FPDFPage_RemoveAnnot' || name === 'EPDFPage_RemoveAnnotByObjectNumber') &&
      [...entries.values()].some(item => item.kind === 'annot' && item.owner === ref(input[0]).ref)) {
    throw new Error('Close page annotation handles before removing annotations')
  }
  if (name === 'FPDFPage_RemoveObject' || name === 'FPDFFormObj_RemoveObject') {
    const object = entry(ref(input[1]).ref, 'object')
    if (object.owned || object.owner !== ref(input[0]).ref) throw new Error('PDFium object does not belong to this container')
  }
  if (name === 'FPDFPage_InsertObject' || name === 'FPDFPage_InsertObjectAtIndex') {
    const object = ref(input[1]).ref
    if (!entry(object, 'object').owned) throw new Error('PDFium object is already owned by a container')
    const document = documentOf(object)
    if (document !== undefined && document !== documentOf(ref(input[0]).ref)) throw new Error('PDFium object belongs to another document')
  }
  let pages = 0
  if (name === 'FPDFImageObj_SetBitmap' && args[0] && scalar(input[1]) > 0) {
    const count = scalar(input[1])
    const view = new DataView(m.HEAPU8.buffer, m.HEAPU8.byteOffset)
    const pointers: number[] = []
    const document = documentOf(ref(input[2]).ref)
    for (let i = 0; i < count; i++) {
      const id = view.getUint32(args[0] + i * 4, true)
      if (document !== undefined && document !== documentOf(id)) throw new Error('PDFium page belongs to another document')
      pointers.push(entry(id, 'page').ptr)
    }
    pages = m._malloc(count * 4)
    if (!pages) throw new Error('PDFium page array allocation failed')
    const output = new DataView(m.HEAPU8.buffer, m.HEAPU8.byteOffset)
    pointers.forEach((ptr, i) => output.setUint32(pages + i * 4, ptr, true))
    args[0] = pages
  }
  let result: number
  try { result = invoke(m, name, args) } finally {
    if (pages) m._free(pages)
    if (name === 'EPDFAnnot_ApplyRedaction') {
      const page = ref(input[0]).ref
      for (const [id, item] of [...entries]) {
        if (item.kind === 'object' && !item.owned && item.owner === page && entries.has(id)) release(m, id)
      }
    }
  }
  if ((name === 'FPDFPage_RemoveObject' || name === 'FPDFFormObj_RemoveObject') && result) {
    const object = entry(ref(input[1]).ref, 'object')
    const document = documentOf(ref(input[0]).ref)
    if (document === undefined) delete object.owner
    else object.owner = document
    object.owned = true
  }
  if (name === 'FPDFPage_InsertObject' || (name === 'FPDFPage_InsertObjectAtIndex' && result)) {
    const object = entry(ref(input[1]).ref, 'object')
    object.owner = ref(input[0]).ref
    object.owned = false
  }
  if (name === 'FPDFLink_Enumerate' && result) {
    const out = ref(input[2]); const where = byteRange(out.ref, out.offset ?? 0, 4).ptr
    const view = new DataView(m.HEAPU8.buffer, m.HEAPU8.byteOffset)
    const ptr = view.getUint32(where, true)
    view.setUint32(where, ptr ? borrowed('link', ptr, ref(input[0]).ref) : 0, true)
  }
  if (!spec.result) return wireNumber(result)
  if (result === 0) return 0
  let size: number | undefined
  if (spec.result === 'memory') {
    const bitmap = args[0]!
    size = invoke(m, 'FPDFBitmap_GetStride', [bitmap]) * invoke(m, 'FPDFBitmap_GetHeight', [bitmap])
    if (!Number.isSafeInteger(size) || size < 0 || result + size > m.HEAPU8.length) throw new RangeError('Invalid PDFium bitmap buffer')
  }
  if (name === 'FPDF_NewFormObjectFromXObject' && owner !== undefined) owner = documentOf(owner)
  if (name === 'FPDFText_GetTextObject' && owner !== undefined) owner = entry(owner, 'text').owner
  if (name === 'FPDFTextObj_GetFont') {
    let ancestor = owner
    while (ancestor !== undefined && entry(ancestor).kind !== 'doc') ancestor = entry(ancestor).owner
    owner = ancestor
  }
  const createdObject = name === 'FPDFPageObj_CreateTextObj' || name === 'FPDFPageObj_CreateNewRect' || name === 'FPDF_NewFormObjectFromXObject' || name === 'FPDFPageObj_NewImageObj'
  const borrowedResult = (spec.result === 'object' && !createdObject) || (spec.result === 'font' && name !== 'FPDFText_LoadFont' && name !== 'FPDFText_LoadStandardFont') || spec.result === 'link' || spec.result === 'action' || spec.result === 'dest' || spec.result === 'clip' || spec.result === 'segment' || spec.result === 'treeChild' || spec.result === 'memory'
  const id = borrowedResult ? borrowed(spec.result, result, owner, size) : add(spec.result, result, owner, size)
  if (name === 'FPDFFormObj_GetObject') entry(id).owner = ref(input[0]).ref
  if (name === 'FPDFBitmap_CreateEx' && input[3] !== 0) entry(id).holds = [ref(input[3]).ref]
  if (name === 'FPDF_LoadMemDocument') entry(id).holds = [ref(input[0]).ref]
  if (name === 'PDFiumExt_InitFormFillEnvironment') entry(id).holds = [ref(input[1]).ref]
  if (name === 'FPDF_NewXObjectFromPage') entry(id).holds = [ref(input[1]).ref]
  return { ref: id }
}
async function execute(raw: unknown): Promise<unknown> {
  if (!raw || typeof raw !== 'object' || !('op' in raw)) throw new TypeError('Invalid PDFium command')
  const command = raw as Record<string, unknown>
  const m = await pdfium()
  switch (command.op) {
    case 'open': {
      if (documentId) throw new Error('PDFium document is already open')
      const bytes = decode64(command.bytes)
      documentBytes = m._malloc(bytes.length || 1)
      if (!documentBytes) throw new Error('PDFium document allocation failed')
      m.HEAPU8.set(bytes, documentBytes)
      if (command.password !== null && command.password !== undefined) {
        if (typeof command.password !== 'string') throw new TypeError('Invalid PDF password')
        const password = new TextEncoder().encode(`${command.password}\0`)
        documentPassword = m._malloc(password.length)
        if (!documentPassword) throw new Error('PDFium password allocation failed')
        m.HEAPU8.set(password, documentPassword)
      }
      const doc = m._FPDF_LoadMemDocument(documentBytes, bytes.length, documentPassword)
      if (!doc) { const code = m._FPDF_GetLastError(); throw Object.assign(new Error('PDFium could not open the document'), { code }) }
      documentId = add('doc', doc)
      return { ref: documentId }
    }
    case 'supports': {
      if (typeof command.name !== 'string') throw new TypeError('Invalid PDFium primitive')
      const name = command.name.startsWith('_') ? command.name.slice(1) : command.name
      if (!Object.hasOwn(specs, name)) throw new Error(`PDFium primitive ${name} is not permitted`)
      return typeof m[`_${name}`] === 'function'
    }
    case 'call': {
      if (typeof command.name !== 'string' || !Array.isArray(command.args)) throw new TypeError('Invalid PDFium call')
      return call(m, command.name, command.args)
    }
    case 'batch': {
      if (!Array.isArray(command.calls) || command.calls.length > 10000) throw new TypeError('Invalid PDFium batch')
      return command.calls.map(item => {
        if (!item || typeof item !== 'object' || typeof item.name !== 'string' || !Array.isArray(item.args)) throw new TypeError('Invalid PDFium batch call')
        return call(m, item.name, item.args)
      })
    }
    case 'alloc': {
      const size = scalar(command.size)
      if (!Number.isSafeInteger(size) || size < 0 || size > 0xffff_ffff) throw new RangeError('Invalid PDFium allocation size')
      const ptr = m._malloc(size || 1)
      if (!ptr) throw new Error('PDFium allocation failed')
      return { ref: add('buffer', ptr, undefined, size) }
    }
    case 'free': { const id = scalar(command.ref); entry(id, 'buffer'); release(m, id); return null }
    case 'read': {
      const range = byteRange(scalar(command.ref), scalar(command.offset), scalar(command.len))
      return bytes64(m.HEAPU8.subarray(range.ptr, range.ptr + range.size))
    }
    case 'write': {
      const bytes = decode64(command.bytes)
      const range = byteRange(scalar(command.ref), scalar(command.offset), bytes.length)
      m.HEAPU8.set(bytes, range.ptr)
      return null
    }
    case 'close': {
      for (const id of [...entries.keys()].reverse()) if (entries.has(id)) release(m, id)
      if (documentPassword) m._free(documentPassword)
      if (documentBytes) m._free(documentBytes)
      documentPassword = 0; documentBytes = 0; documentId = 0
      return null
    }
    default: throw new Error('Invalid PDFium command')
  }
}
worker.onmessage = event => {
  void execute(event.data).then(value => worker.postMessage({ ok: value })).catch(error => {
    const e = error instanceof Error ? error : new Error(String(error))
    const code = 'code' in e && typeof e.code === 'number' ? e.code : undefined
    worker.postMessage({ error: { message: e.message, ...(code === undefined ? {} : { code }) } })
  })
}
