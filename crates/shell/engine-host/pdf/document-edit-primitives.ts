import { PDFArray, PDFBool, PDFCheckBox, PDFDict, PDFDocument, PDFDropdown, PDFHexString, PDFName, PDFNull, PDFNumber, PDFOptionList, PDFRadioGroup, PDFRawStream, PDFRef, PDFSignature, PDFString, PDFTextField, StandardFonts, TextAlignment, degrees, rgb } from 'pdf-lib'
import type { PDFField, PDFObject } from 'pdf-lib'
import type { NativeCallback } from '../transport'

const decode = (value: string): Uint8Array => Uint8Array.from(atob(value), c => c.charCodeAt(0))
const encode = (bytes: Uint8Array): string => {
  let result = ''
  for (let i = 0; i < bytes.length; i += 8192) result += String.fromCharCode(...bytes.subarray(i, i + 8192))
  return btoa(result)
}

type Value = null | boolean | number | { name: string } | { text: string } | { literal: string } |
  { ref: number } | { array: Value[] } | { dict: Record<string, Value> }
type ObjectType = 'array' | 'dict' | 'name' | 'number'
type Command =
  | { op: 'pageCount' }
  | { op: 'page'; index: number }
  | { op: 'catalog' }
  | { op: 'pageRef'; index: number }
  | { op: 'rotation'; index: number }
  | { op: 'read'; target: number; key: string; resolve?: boolean }
  | { op: 'resolve'; target: number }
  | { op: 'arrayRead'; target: number; index: number; resolve?: boolean }
  | { op: 'readMaybe'; target: number; key: string; type: ObjectType }
  | { op: 'resolveMaybe'; target: number; type: ObjectType }
  | { op: 'arrayReadMaybe'; target: number; index: number; type: ObjectType }
  | { op: 'arraySize'; target: number }
  | { op: 'inspect'; target: number }
  | { op: 'make'; value: Value; register?: boolean }
  | { op: 'stream'; bytesBase64: string; dict: Record<string, Value>; register?: boolean }
  | { op: 'set'; target: number; key: string; value: Value }
  | { op: 'delete'; target: number; key: string }
  | { op: 'push'; target: number; value: Value }
  | { op: 'remove'; target: number; index: number }
  | { op: 'rotatePage'; index: number; angle: number }
  | { op: 'removePage'; index: number }
  | { op: 'reorderPages'; indices: number[] }
  | { op: 'metadataString'; key: 'title' | 'author' | 'subject'; value: string }
  | { op: 'metadataKeywords'; value: string[] }
  | { op: 'metadataDate'; value: number }
  | { op: 'formText'; name: string; value: string }
  | { op: 'formRadio'; name: string; value: string | null }
  | { op: 'formChoice'; name: string; value: string | null }
  | { op: 'formCheckbox'; name: string; checked: boolean }
  | { op: 'formDirtyFields' }
  | { op: 'updateFieldAppearance'; index: number }
  | { op: 'setNeedAppearances' }
  | { op: 'drawPng'; index: number; image: string; x: number; y: number; width: number; height: number; opacity: number }
  | { op: 'embedPng'; image: string }
  | { op: 'embedHelvetica' }
  | { op: 'pdfDate'; ms: number }
  | { op: 'urlHref'; url: string }
  | { op: 'same'; left: number; right: number }
  | { op: 'pdfRef'; object: number; generation: number }
  | { op: 'lookupRef'; object: number; generation: number }
  | { op: 'formAddField'; ref: number }
  | { op: 'fieldFind'; name: string }
  | { op: 'fieldNames' }
  | { op: 'fieldCreate'; name: string; kind: 'text' | 'checkbox' | 'radio' | 'choice' }
  | { op: 'fieldRequired'; field: number; value: boolean }
  | { op: 'fieldRemove' | 'fieldDirty' | 'fieldMultiline' | 'fieldOptions'; field: number }
  | { op: 'fieldRemoveWidget'; field: number; index: number }
  | { op: 'fieldAddToPage'; field: number; page: number; box: { x: number; y: number; width: number; height: number; rotation: number }; option?: string }
  | { op: 'fieldAddOptions'; field: number; options: string[] }
  | { op: 'fieldSelect' | 'fieldText'; field: number; value: string }
  | { op: 'fieldCheck'; field: number; checked: boolean }
  | { op: 'fieldMaxLength' | 'fieldFontSize'; field: number; value: number }
  | { op: 'fieldAlignment'; field: number; value: 'left' | 'center' | 'right' }
  | { op: 'shrinkStreams'; metadataOnly?: boolean }
  | { op: 'readRawStream'; ref: string }
  | { op: 'replaceRawStreams'; replacements: Array<{ ref: string; bytesBase64: string; width?: number; height?: number }> }
  | { op: 'save'; useObjectStreams?: boolean }

/** A request-owned pdf-lib document. Only object/array codec operations live here;
 * policy, object traversal and edit ordering stay in the native caller. */
export class PdfDocumentPrimitives {
  private readonly handles = new Map<number, PDFObject>()
  private readonly fields = new Map<number, PDFField>()
  private readonly streamRefs = new Map<string, PDFRef>()
  private next = 1
  private constructor(private readonly doc: PDFDocument) {}
  static async open(bytesBase64: string): Promise<PdfDocumentPrimitives> {
    return new PdfDocumentPrimitives(await PDFDocument.load(decode(bytesBase64), { updateMetadata: false }))
  }
  private hold(value: PDFObject): number { const id = this.next++; this.handles.set(id, value); return id }
  private get(id: number): PDFObject { const value = this.handles.get(id); if (!value) throw new Error('invalid PDF object handle'); return value }
  private lookupMaybe(value: PDFObject | undefined, type: ObjectType): PDFObject | undefined {
    if (type === 'array') return this.doc.context.lookupMaybe(value, PDFArray)
    if (type === 'dict') return this.doc.context.lookupMaybe(value, PDFDict)
    if (type === 'name') return this.doc.context.lookupMaybe(value, PDFName)
    return this.doc.context.lookupMaybe(value, PDFNumber)
  }
  private dict(id: number): PDFDict { const value = this.get(id); if (!(value instanceof PDFDict)) throw new Error('PDF object is not a dictionary'); return value }
  private array(id: number): PDFArray { const value = this.get(id); if (!(value instanceof PDFArray)) throw new Error('PDF object is not an array'); return value }
  private object(value: Value): PDFObject {
    if (value === null) return PDFNull
    if (typeof value === 'boolean') return value ? PDFBool.True : PDFBool.False
    if (typeof value === 'number') return PDFNumber.of(value)
    if ('name' in value) return PDFName.of(value.name)
    if ('text' in value) return PDFHexString.fromText(value.text)
    if ('literal' in value) return PDFString.of(value.literal)
    if ('ref' in value) return this.get(value.ref)
    if ('array' in value) { const result = this.doc.context.obj([]); for (const item of value.array) result.push(this.object(item)); return result }
    const result = this.doc.context.obj({}); for (const [key, item] of Object.entries(value.dict)) result.set(PDFName.of(key), this.object(item)); return result
  }
  private result(value: PDFObject | undefined): number | null { return value === undefined ? null : this.hold(value) }
  async execute(command: Command): Promise<unknown> {
    const context = this.doc.context
    switch (command.op) {
      case 'pageCount': return this.doc.getPageCount()
      case 'page': return this.hold(this.doc.getPages()[command.index]!.node)
      case 'pageRef': return this.hold(this.doc.getPages()[command.index]!.ref)
      case 'catalog': return this.hold(this.doc.catalog)
      case 'rotation': return this.doc.getPages()[command.index]!.getRotation().angle
      case 'rotatePage': this.doc.getPages()[command.index]!.setRotation(degrees(command.angle)); return null
      case 'removePage': this.doc.removePage(command.index); return null
      case 'reorderPages': { const pages = this.doc.getPages(); const target = command.indices.map(index => pages[index]!); while (this.doc.getPageCount()) this.doc.removePage(0); for (const page of target) this.doc.addPage(page); return null }
      case 'metadataString': { if (command.key === 'title') this.doc.setTitle(command.value); else if (command.key === 'author') this.doc.setAuthor(command.value); else this.doc.setSubject(command.value); return null }
      case 'metadataKeywords': this.doc.setKeywords(command.value); return null
      case 'metadataDate': this.doc.setModificationDate(new Date(command.value)); return null
      case 'formText': this.doc.getForm().getTextField(command.name).setText(command.value); return null
      case 'formRadio': { const field = this.doc.getForm().getRadioGroup(command.name); if (command.value) field.select(command.value); else field.clear(); return null }
      case 'formChoice': { const field = this.doc.getForm().getField(command.name); if (field instanceof PDFDropdown || field instanceof PDFOptionList) { if (command.value) field.select(command.value); else field.clear() } return null }
      case 'formCheckbox': { const field = this.doc.getForm().getCheckBox(command.name); if (command.checked) field.check(); else field.uncheck(); return null }
      case 'formDirtyFields': { if (!this.doc.catalog.has(PDFName.of('AcroForm'))) return []; const form = this.doc.getForm(); form.getDefaultFont(); return form.getFields().flatMap((field, index) => field.needsAppearancesUpdate() ? [index] : []) }
      case 'updateFieldAppearance': { const form = this.doc.getForm(); form.getFields()[command.index]!.defaultUpdateAppearances(form.getDefaultFont()); return null }
      case 'setNeedAppearances': this.doc.getForm().acroForm.dict.set(PDFName.of('NeedAppearances'), PDFBool.True); return null
      case 'drawPng': { const image = await this.doc.embedPng(command.image); this.doc.getPages()[command.index]!.drawImage(image, { x: command.x, y: command.y, width: command.width, height: command.height, opacity: command.opacity }); return null }
      case 'embedPng': return this.hold((await this.doc.embedPng(command.image)).ref)
      case 'embedHelvetica': return this.hold((await this.doc.embedFont(StandardFonts.Helvetica)).ref)
      case 'pdfDate': { const d = new Date(command.ms); const p = (n: number) => String(n).padStart(2, '0'); const offset = -d.getTimezoneOffset(); const sign = offset >= 0 ? '+' : '-'; const abs = Math.abs(offset); return `D:${d.getFullYear()}${p(d.getMonth() + 1)}${p(d.getDate())}${p(d.getHours())}${p(d.getMinutes())}${p(d.getSeconds())}${sign}${p(Math.floor(abs / 60))}'${p(abs % 60)}'` }
      case 'urlHref': { try { return new URL(command.url).href } catch { return null } }
      case 'pdfRef': return this.hold(PDFRef.of(command.object, command.generation))
      case 'lookupRef': return this.result(context.lookupMaybe(PDFRef.of(command.object, command.generation), PDFDict))
      case 'formAddField': { const ref = this.get(command.ref); if (!(ref instanceof PDFRef)) throw new Error('PDF field reference is not indirect'); this.doc.getForm().acroForm.addField(ref); return null }
      case 'same': return this.get(command.left) === this.get(command.right)
      case 'fieldNames': return this.doc.getForm().getFields().map(field => field.getName())
      case 'fieldFind': {
        const field = this.doc.getForm().getFieldMaybe(command.name)
        if (!field) return null
        const id = this.next++; this.fields.set(id, field)
        const kids = field.acroField.Kids()
        const widgets = field.acroField.getWidgets().map((widget, index) => ({ dict: this.hold(widget.dict), ref: this.hold(kids && kids.size() ? kids.get(index) : field.ref) }))
        const kind = field instanceof PDFRadioGroup ? 'radio' : field instanceof PDFCheckBox ? 'checkbox' : field instanceof PDFTextField ? 'text' : field instanceof PDFDropdown ? 'choice' : field instanceof PDFSignature ? 'signature' : 'other'
        return { handle: id, kind, ref: this.hold(field.ref), widgets }
      }
      case 'fieldCreate': {
        const form = this.doc.getForm()
        const field = command.kind === 'text' ? form.createTextField(command.name) : command.kind === 'checkbox' ? form.createCheckBox(command.name) : command.kind === 'radio' ? form.createRadioGroup(command.name) : form.createDropdown(command.name)
        const id = this.next++; this.fields.set(id, field); return id
      }
      case 'fieldRequired': { const field = this.fields.get(command.field); if (!field) throw new Error('invalid PDF field handle'); if (command.value) field.enableRequired(); else field.disableRequired(); return null }
      case 'fieldRemove': { const field = this.fields.get(command.field); if (!field) throw new Error('invalid PDF field handle'); this.doc.getForm().removeField(field); return null }
      case 'fieldRemoveWidget': { const field = this.fields.get(command.field); if (!field) throw new Error('invalid PDF field handle'); field.acroField.removeWidget(command.index); return null }
      case 'fieldDirty': { const field = this.fields.get(command.field); if (!field) throw new Error('invalid PDF field handle'); const mark = Reflect.get(field, 'markAsDirty'); if (typeof mark !== 'function') throw new Error('PDF field cannot be marked dirty'); mark.call(field); return null }
      case 'fieldAddToPage': {
        const field = this.fields.get(command.field); if (!field) throw new Error('invalid PDF field handle')
        const page = this.doc.getPages()[command.page]!
        const options = { x: command.box.x, y: command.box.y, width: command.box.width, height: command.box.height, rotate: degrees(command.box.rotation), borderWidth: 1, borderColor: rgb(0.45, 0.45, 0.45), backgroundColor: undefined }
        if (field instanceof PDFRadioGroup) field.addOptionToPage(command.option!, page, options)
        else if (field instanceof PDFTextField || field instanceof PDFCheckBox || field instanceof PDFDropdown) field.addToPage(page, options)
        else throw new Error('PDF field does not support widgets')
        return null
      }
      case 'fieldOptions': { const field = this.fields.get(command.field); if (!(field instanceof PDFRadioGroup)) throw new Error('PDF field is not radio'); return field.getOptions() }
      case 'fieldAddOptions': { const field = this.fields.get(command.field); if (!(field instanceof PDFDropdown)) throw new Error('PDF field is not a dropdown'); field.addOptions(command.options); return null }
      case 'fieldSelect': { const field = this.fields.get(command.field); if (!(field instanceof PDFDropdown || field instanceof PDFRadioGroup)) throw new Error('PDF field cannot select'); field.select(command.value); return null }
      case 'fieldCheck': { const field = this.fields.get(command.field); if (!(field instanceof PDFCheckBox)) throw new Error('PDF field is not a checkbox'); if (command.checked) field.check(); else field.uncheck(); return null }
      case 'fieldMultiline': { const field = this.fields.get(command.field); if (!(field instanceof PDFTextField)) throw new Error('PDF field is not text'); field.enableMultiline(); return null }
      case 'fieldMaxLength': { const field = this.fields.get(command.field); if (!(field instanceof PDFTextField)) throw new Error('PDF field is not text'); field.setMaxLength(command.value); return null }
      case 'fieldAlignment': { const field = this.fields.get(command.field); if (!(field instanceof PDFTextField)) throw new Error('PDF field is not text'); field.setAlignment({ left: TextAlignment.Left, center: TextAlignment.Center, right: TextAlignment.Right }[command.value]); return null }
      case 'fieldText': { const field = this.fields.get(command.field); if (!(field instanceof PDFTextField)) throw new Error('PDF field is not text'); field.setText(command.value); return null }
      case 'fieldFontSize': { const field = this.fields.get(command.field); if (!(field instanceof PDFTextField)) throw new Error('PDF field is not text'); field.setFontSize(command.value); return null }
      case 'resolve': return this.result(context.lookup(this.get(command.target)))
      case 'readMaybe': return this.result(this.lookupMaybe(this.dict(command.target).get(PDFName.of(command.key)), command.type))
      case 'resolveMaybe': return this.result(this.lookupMaybe(this.get(command.target), command.type))
      case 'arrayReadMaybe': return this.result(this.lookupMaybe(this.array(command.target).get(command.index), command.type))
      case 'read': { const dict = this.dict(command.target); const value = dict.get(PDFName.of(command.key)); return this.result(command.resolve && value ? context.lookup(value) : value) }
      case 'arrayRead': { const array = this.array(command.target); const value = array.get(command.index); return this.result(command.resolve && value ? context.lookup(value) : value) }
      case 'arraySize': return this.array(command.target).size()
      case 'inspect': {
        const value = this.get(command.target)
        if (value instanceof PDFRef) return { type: 'ref', objectNumber: value.objectNumber, generationNumber: value.generationNumber }
        if (value instanceof PDFHexString || value instanceof PDFString) return { type: 'text', value: value.decodeText() }
        if (value instanceof PDFName) return { type: 'name', value: value.decodeText() }
        if (value instanceof PDFNumber) return { type: 'number', value: value.asNumber() }
        if (value instanceof PDFDict) return { type: 'dict' }
        if (value instanceof PDFArray) return { type: 'array', size: value.size() }
        return { type: 'other', value: value.toString() }
      }
      case 'make': { const value = this.object(command.value); return this.hold(command.register ? context.register(value) : value) }
      case 'stream': { const dict = this.object({ dict: command.dict }); if (!(dict instanceof PDFDict)) throw new Error('invalid stream dictionary'); const value = PDFRawStream.of(dict, decode(command.bytesBase64)); return this.hold(command.register ? context.register(value) : value) }
      case 'set': this.dict(command.target).set(PDFName.of(command.key), this.object(command.value)); return null
      case 'delete': this.dict(command.target).delete(PDFName.of(command.key)); return null
      case 'push': this.array(command.target).push(this.object(command.value)); return null
      case 'remove': this.array(command.target).remove(command.index); return null
      case 'shrinkStreams': {
        const name = (key: string) => PDFName.of(key)
        const refKey = (ref: PDFRef) => `${ref.objectNumber}:${ref.generationNumber}`
        const asInt = (value: unknown) => { const n = Number(String(value ?? '').replace(/^\//, '')); return Number.isFinite(n) ? n : 0 }
        return context.enumerateIndirectObjects().flatMap(([ref, object]) => {
          if (!(object instanceof PDFRawStream)) return []
          this.streamRefs.set(refKey(ref), ref)
          const dict = object.dict
          const mask = dict.get(name('SMask'))
          const color = context.lookup(dict.get(name('ColorSpace')))
          return [{
            ref: refKey(ref), subtype: String(dict.get(name('Subtype')) ?? ''), filter: String(dict.get(name('Filter')) ?? ''),
            width: asInt(dict.get(name('Width'))), height: asInt(dict.get(name('Height'))),
            bitsPerComponent: asInt(dict.get(name('BitsPerComponent')) ?? 8),
            colorSpace: color instanceof PDFName ? color.asString() : null,
            decodeParms: dict.get(name('DecodeParms')) !== undefined, matte: dict.get(name('Matte')) !== undefined,
            maskPresent: mask !== undefined, maskRef: mask instanceof PDFRef ? refKey(mask) : null,
            maskIsStream: mask instanceof PDFRef && context.lookup(mask) instanceof PDFRawStream,
            ...(command.metadataOnly ? {} : { bytesBase64: encode(object.contents) }),
          }]
        })
      }
      case 'readRawStream': {
        const ref = this.streamRefs.get(command.ref)
        const object = ref ? context.lookup(ref) : undefined
        if (!(object instanceof PDFRawStream)) throw new Error('PDF stream is missing')
        return encode(object.contents)
      }
      case 'replaceRawStreams': {
        const objects = new Map(context.enumerateIndirectObjects().map(([ref, object]) => [`${ref.objectNumber}:${ref.generationNumber}`, { ref, object }]))
        for (const replacement of command.replacements) {
          const found = objects.get(replacement.ref)
          if (!found || !(found.object instanceof PDFRawStream)) throw new Error('PDF stream replacement target is missing')
          const bytes = decode(replacement.bytesBase64)
          let dict = found.object.dict
          if (replacement.width !== undefined && replacement.height !== undefined) {
            dict = dict.clone()
            dict.set(PDFName.of('Width'), PDFNumber.of(replacement.width))
            dict.set(PDFName.of('Height'), PDFNumber.of(replacement.height))
            dict.set(PDFName.of('Length'), PDFNumber.of(bytes.length))
          }
          context.assign(found.ref, PDFRawStream.of(dict, bytes))
        }
        return null
      }
      case 'save': return encode(await (command.useObjectStreams === true ? this.doc.save({ useObjectStreams: true }) : this.doc.save({ useObjectStreams: false, updateFieldAppearances: false })))
    }
  }
}

interface WorkerReply { ok?: unknown; error?: { message: string } }

/** The worker is terminated on cancellation, including while pdf-lib is busy. */
export async function runDocumentEditSession(_input: unknown, signal: AbortSignal, callback: NativeCallback): Promise<null> {
  signal.throwIfAborted()
  const worker = new Worker(new URL('./pdf/document-edit-primitives/worker.js', document.baseURI), { type: 'module' })
  let pending: { resolve: (reply: WorkerReply) => void; reject: (error: Error) => void } | undefined
  let stopped = false
  const stop = (error: Error) => {
    if (stopped) return
    stopped = true
    worker.terminate()
    pending?.reject(error)
    pending = undefined
  }
  const abort = () => stop(new DOMException('The PDF document session was cancelled', 'AbortError'))
  signal.addEventListener('abort', abort, { once: true })
  worker.onerror = event => stop(new Error(event.message || 'PDF document worker failed'))
  worker.onmessageerror = () => stop(new Error('PDF document worker returned an invalid message'))
  worker.onmessage = event => {
    if (!pending) { stop(new Error('Unexpected PDF document worker reply')); return }
    const current = pending
    pending = undefined
    current.resolve(event.data as WorkerReply)
  }
  try {
    let response: WorkerReply | undefined
    for (;;) {
      signal.throwIfAborted()
      const command = await callback('pdfDocument.next', response === undefined ? {} : { response })
      signal.throwIfAborted()
      if (!command || typeof command !== 'object' || !('op' in command) || typeof command.op !== 'string') throw new TypeError('Invalid PDF document command')
      const { promise, resolve, reject } = Promise.withResolvers<WorkerReply>()
      pending = { resolve, reject }
      try { worker.postMessage(command) } catch (error) { pending = undefined; reject(error instanceof Error ? error : new Error(String(error))) }
      response = await promise
      if (command.op === 'close') return null
    }
  } finally {
    signal.removeEventListener('abort', abort)
    stop(new Error('PDF document session closed'))
  }
}
