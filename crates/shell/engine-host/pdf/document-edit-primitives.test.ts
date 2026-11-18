import { describe, expect, it } from 'vitest'
import { PDFArray, PDFDict, PDFDocument, PDFHexString, PDFName, PDFNumber, PDFRawStream } from 'pdf-lib'
import { PdfDocumentPrimitives } from './document-edit-primitives'

const b64 = (bytes: Uint8Array) => btoa(Array.from(bytes, byte => String.fromCharCode(byte)).join(''))
const bytes = (value: string) => Uint8Array.from(atob(value), c => c.charCodeAt(0))

describe('PDF document primitive session', () => {
  it('retains dictionary references and annotations in one saved graph', async () => {
    const source = await PDFDocument.create(); source.addPage([200, 300])
    const codec = await PdfDocumentPrimitives.open(b64(await source.save()))
    const pageRef = await codec.execute({ op: 'pageRef', index: 0 }) as number
    const page = await codec.execute({ op: 'page', index: 0 }) as number
    const annotation = await codec.execute({ op: 'make', register: true, value: { dict: { Type: { name: 'Annot' }, Subtype: { name: 'Text' }, Rect: { array: [0, 0, 20, 20] }, P: { ref: pageRef }, Contents: { text: 'note' } } } }) as number
    await codec.execute({ op: 'set', target: page, key: 'Annots', value: { array: [{ ref: annotation }] } })
    const output = await codec.execute({ op: 'save' }) as string
    const restored = await PDFDocument.load(bytes(output), { updateMetadata: false })
    const annots = restored.getPage(0).node.lookup(PDFName.of('Annots'), PDFArray)
    const dict = annots.lookup(0, PDFDict)
    expect(dict.lookup(PDFName.of('Contents'), PDFHexString).decodeText()).toBe('note')
    expect(dict.get(PDFName.of('P'))).toBe(restored.getPage(0).ref)
  })
  it('creates a radio widget through field primitives and reports its identity', async () => {
    const source = await PDFDocument.create(); source.addPage([200, 300])
    const codec = await PdfDocumentPrimitives.open(b64(await source.save()))
    const field = await codec.execute({ op: 'fieldCreate', name: 'answer', kind: 'radio' }) as number
    await codec.execute({ op: 'fieldAddToPage', field, page: 0, box: { x: 1, y: 2, width: 30, height: 20, rotation: 0 }, option: 'yes' })
    await codec.execute({ op: 'fieldSelect', field, value: 'yes' })
    await codec.execute({ op: 'fieldDirty', field })
    const found = await codec.execute({ op: 'fieldFind', name: 'answer' }) as { kind: string; widgets: unknown[] }
    expect(found.kind).toBe('radio'); expect(found.widgets).toHaveLength(1)
    const restored = await PDFDocument.load(bytes(await codec.execute({ op: 'save' }) as string))
    expect(restored.getForm().getRadioGroup('answer').getSelected()).toBe('yes')
  })
  it('reorders original pages after removal without losing page identity', async () => {
    const source = await PDFDocument.create(); source.addPage([100, 100]); source.addPage([200, 100]); source.addPage([300, 100])
    const codec = await PdfDocumentPrimitives.open(b64(await source.save()))
    await codec.execute({ op: 'pageCount' })
    await codec.execute({ op: 'removePage', index: 1 })
    await codec.execute({ op: 'reorderPages', indices: [2, 0] })
    const restored = await PDFDocument.load(bytes(await codec.execute({ op: 'save' }) as string))
    expect(restored.getPages().map(page => page.getWidth())).toEqual([300, 100])
  })
  it('distinguishes absent entries from incorrectly typed PDF objects', async () => {
    const source = await PDFDocument.create(); source.addPage()
    const codec = await PdfDocumentPrimitives.open(b64(await source.save()))
    const object = await codec.execute({ op: 'make', value: { dict: { Value: 3 } } }) as number
    expect(await codec.execute({ op: 'readMaybe', target: object, key: 'Missing', type: 'array' })).toBeNull()
    await expect(codec.execute({ op: 'readMaybe', target: object, key: 'Value', type: 'array' })).rejects.toThrow()
  })
  it('exposes paired image streams and replaces bytes without losing image metadata', async () => {
    const source = await PDFDocument.create(); source.addPage()
    const mask = source.context.register(PDFRawStream.of(source.context.obj({ Type: 'XObject', Subtype: 'Image', Width: 40, Height: 20, ColorSpace: 'DeviceGray', Filter: 'FlateDecode' }), Uint8Array.of(1, 2, 3)))
    const image = source.context.register(PDFRawStream.of(source.context.obj({ Type: 'XObject', Subtype: 'Image', Width: 40, Height: 20, ColorSpace: 'DeviceRGB', Filter: 'FlateDecode', SMask: mask, Intent: 'RelativeColorimetric' }), Uint8Array.of(4, 5, 6)))
    source.catalog.set(PDFName.of('TestImage'), image)
    const codec = await PdfDocumentPrimitives.open(b64(await source.save({ useObjectStreams: false })))
    const streams = await codec.execute({ op: 'shrinkStreams', metadataOnly: true }) as Array<{ ref: string; subtype: string; colorSpace: string | null; maskRef: string | null; maskPresent: boolean; maskIsStream: boolean; bytesBase64?: string }>
    const base = streams.find(stream => stream.maskPresent)!
    expect(base.subtype).toBe('/Image'); expect(base.colorSpace).toBe('/DeviceRGB')
    expect(base.maskIsStream).toBe(true); expect(streams.some(stream => stream.ref === base.maskRef)).toBe(true)
    expect(base.bytesBase64).toBeUndefined()
    expect(await codec.execute({ op: 'readRawStream', ref: base.ref })).toBe(b64(Uint8Array.of(4, 5, 6)))
    await codec.execute({ op: 'replaceRawStreams', replacements: [{ ref: base.ref, bytesBase64: b64(Uint8Array.of(9, 8)), width: 20, height: 10 }] })
    const restored = await PDFDocument.load(bytes(await codec.execute({ op: 'save', useObjectStreams: true }) as string))
    const result = restored.catalog.lookup(PDFName.of('TestImage'))
    if (!(result instanceof PDFRawStream)) throw new Error('expected an image stream')
    expect(result.dict.lookup(PDFName.of('Width'), PDFNumber).asNumber()).toBe(20)
    expect(result.dict.get(PDFName.of('SMask'))).toBeDefined()
    expect(result.dict.lookup(PDFName.of('Intent'), PDFName).asString()).toBe('/RelativeColorimetric')
    expect([...result.getContents()]).toEqual([9, 8])
  })
})
