import { afterEach, describe, expect, it, vi } from 'vitest'

const fixture = vi.hoisted(() => {
  const memory = new Uint8Array(8192)
  let next = 128
  const closed: number[] = []
  const module = {
    HEAPU8: memory,
    HEAPU32: new Uint32Array(memory.buffer),
    _malloc: (size: number) => {
      const p = next
      next += Math.max(size, 1) + 8
      return p
    },
    _free: vi.fn(),
    _PDFiumExt_Init: vi.fn(),
    _FPDF_LoadMemDocument: () => 21,
    _FPDF_GetLastError: () => 0,
    _FPDF_CloseDocument: (id: number) => {
      closed.push(id)
    },
    _FPDF_LoadPage: () => 31,
    _FPDF_ClosePage: (id: number) => {
      closed.push(id)
    },
    _FPDF_GetPageCount: () => 3,
    _FPDF_GetPageWidthF: () => -0,
    _FPDFPage_GetMediaBox: () => 1,
    _FPDFText_LoadPage: () => 41,
    _FPDFText_ClosePage: (id: number) => {
      closed.push(id)
    },
    _FPDFPage_GetObject: () => 51,
    _FPDFText_GetTextObject: () => 51,
    _FPDFPageObj_GetType: () => 1,
    _FPDFFormObj_GetObject: (parent: number) => (parent === 51 ? 52 : 53),
    _FPDFFormObj_RemoveObject: () => 1,
    _FPDFPage_InsertObjectAtIndex: (_page: number, _object: number, index: number) =>
      index === 0 ? 1 : 0,
    _FPDFPageObj_Destroy: (id: number) => {
      closed.push(id)
    },
    _FPDFTextObj_GetText: (_object: number, _text: number, buffer: number, size: number) => {
      if (buffer && size >= 4) memory.set([65, 0, 0, 0], buffer)
      return 4
    },
    _FPDFPageObj_NewImageObj: () => 71,
    _FPDFBitmap_CreateEx: () => 62,
    _FPDFImageObj_SetBitmap: (pages: number, count: number) =>
      count === 0 || new DataView(memory.buffer).getUint32(pages, true) === 31 ? 1 : 0,
    _EPDFPage_GetAnnotByObjectNumber: () => 81,
    _FPDFAnnot_GetAP: () => 4,
    _EPDFPage_RemoveAnnotByObjectNumber: () => 1,
    _FPDFPage_RemoveAnnot: () => 1,
    _FPDFPage_CloseAnnot: (id: number) => {
      closed.push(id)
    },
    _FPDFPage_CreateAnnot: () => 82,
    _FPDFAnnot_SetRect: () => 1,
    _EPDFAnnot_SetColor: () => 1,
    _EPDFAnnot_ApplyRedaction: () => 1,
    _FPDFBitmap_Create: () => 61,
    _FPDFBitmap_Destroy: (id: number) => {
      closed.push(id)
    },
    _FPDFBitmap_GetBuffer: () => 2048,
    _FPDFBitmap_GetStride: () => 4,
    _FPDFBitmap_GetHeight: () => 1,
  }
  return { module, closed }
})
vi.mock('@embedpdf/pdfium', () => ({ init: async () => fixture.module }))
afterEach(() => {
  vi.unstubAllGlobals()
  vi.resetModules()
})

describe('PDFium session worker', () => {
  it('keeps handles scoped, checks output buffers, and closes live resources', async () => {
    let reply: ((value: unknown) => void) | undefined
    const worker: {
      location: { href: string }
      onmessage?: (event: { data: unknown }) => void
      postMessage: (value: unknown) => void
    } = {
      location: { href: 'https://service/pdf/pdfium-session/worker.js' },
      postMessage: (value) => {
        reply?.(value)
      },
    }
    vi.stubGlobal('self', worker)
    vi.stubGlobal('fetch', async () => ({ ok: true, arrayBuffer: async () => new ArrayBuffer(0) }))
    // The worker installs its message handler at import time, after the globals are stubbed.
    await import('./worker')
    async function send(command: unknown): Promise<{ ok?: unknown; error?: { message: string } }> {
      const { promise, resolve } = Promise.withResolvers<unknown>()
      reply = resolve
      worker.onmessage!({ data: command })
      return (await promise) as { ok?: unknown; error?: { message: string } }
    }
    function handle(value: unknown): number {
      if (!value || typeof value !== 'object' || !('ref' in value) || typeof value.ref !== 'number')
        throw new Error('Expected a PDFium handle')
      return value.ref
    }
    const opened = await send({
      op: 'open',
      bytes: { type: 'bytes', base64: btoa('%PDF') },
      password: null,
    })
    const doc = handle(opened.ok)
    const page = handle(
      (await send({ op: 'call', name: 'FPDF_LoadPage', args: [{ ref: doc }, 0] })).ok,
    )
    expect((await send({ op: 'call', name: 'FPDF_GetPageCount', args: [{ ref: doc }] })).ok).toBe(3)
    const object = handle(
      (await send({ op: 'call', name: 'FPDFPage_GetObject', args: [{ ref: page }, 0] })).ok,
    )
    const textPage = handle(
      (await send({ op: 'call', name: 'FPDFText_LoadPage', args: [{ ref: page }] })).ok,
    )
    const glyphObject = handle(
      (await send({ op: 'call', name: 'FPDFText_GetTextObject', args: [{ ref: textPage }, 0] })).ok,
    )
    expect(glyphObject).toBe(object)
    await send({ op: 'call', name: 'FPDFText_ClosePage', args: [{ ref: textPage }] })
    expect(
      (await send({ op: 'call', name: 'FPDFPageObj_GetType', args: [{ ref: object }] })).ok,
    ).toBe(1)
    const buffer = handle((await send({ op: 'alloc', size: 4 })).ok)
    await send({ op: 'write', ref: buffer, offset: 1, bytes: { type: 'bytes', base64: 'AQI=' } })
    expect((await send({ op: 'read', ref: buffer, offset: 1, len: 2 })).ok).toEqual({
      type: 'bytes',
      base64: 'AQI=',
    })
    expect(
      (await send({ op: 'call', name: 'FPDF_GetPageWidthF', args: [{ ref: page }] })).ok,
    ).toEqual({ type: 'number', value: '-0' })
    const wrongKind = await send({ op: 'call', name: 'FPDF_GetPageCount', args: [{ ref: page }] })
    expect(wrongKind.error?.message).toContain('incompatible')
    const overrun = await send({
      op: 'call',
      name: 'FPDFPage_GetMediaBox',
      args: [
        { ref: page },
        { ref: buffer, offset: 1 },
        { ref: buffer },
        { ref: buffer },
        { ref: buffer },
      ],
    })
    expect(overrun.error?.message).toContain('outside')
    const bitmap = handle(
      (await send({ op: 'call', name: 'FPDFBitmap_Create', args: [1, 1, 1] })).ok,
    )
    const pixels = handle(
      (await send({ op: 'call', name: 'FPDFBitmap_GetBuffer', args: [{ ref: bitmap }] })).ok,
    )
    expect((await send({ op: 'read', ref: pixels, offset: 0, len: 4 })).ok).toEqual({
      type: 'bytes',
      base64: 'AAAAAA==',
    })
    await send({ op: 'call', name: 'FPDFBitmap_Destroy', args: [{ ref: bitmap }] })
    expect((await send({ op: 'read', ref: pixels, offset: 0, len: 1 })).error?.message).toContain(
      'Stale',
    )
    const denied = await send({ op: 'call', name: 'malloc', args: [1] })
    expect(denied.error?.message).toContain('not permitted')
    await send({ op: 'close' })
    expect(fixture.closed).toEqual([41, 61, 31, 21])
    const stale = await send({ op: 'call', name: 'FPDF_GetPageCount', args: [{ ref: doc }] })
    expect(stale.error?.message).toContain('Stale')
  })

  it('transfers nested objects, bounds byte outputs and protects bitmap backing storage', async () => {
    let reply: ((value: unknown) => void) | undefined
    const worker: {
      location: { href: string }
      onmessage?: (event: { data: unknown }) => void
      postMessage: (value: unknown) => void
    } = {
      location: { href: 'https://service/pdf/pdfium-session/worker.js' },
      postMessage: (value) => {
        reply?.(value)
      },
    }
    vi.stubGlobal('self', worker)
    vi.stubGlobal('fetch', async () => ({ ok: true, arrayBuffer: async () => new ArrayBuffer(0) }))
    // The worker installs its handler at import, after these globals are stubbed.
    await import('./worker')
    async function send(command: unknown): Promise<{ ok?: unknown; error?: { message: string } }> {
      const { promise, resolve } = Promise.withResolvers<unknown>()
      reply = resolve
      worker.onmessage!({ data: command })
      return (await promise) as { ok?: unknown; error?: { message: string } }
    }
    function handle(value: unknown): number {
      if (!value || typeof value !== 'object' || !('ref' in value) || typeof value.ref !== 'number')
        throw new Error('Expected a PDFium handle')
      return value.ref
    }
    const call = (name: string, ...args: unknown[]) => send({ op: 'call', name, args })
    const r = (id: number) => ({ ref: id })
    const doc = handle(
      (await send({ op: 'open', bytes: { type: 'bytes', base64: btoa('%PDF') }, password: null }))
        .ok,
    )
    const page = handle((await call('FPDF_LoadPage', r(doc), 0)).ok)
    const form = handle((await call('FPDFPage_GetObject', r(page), 0)).ok)
    const nested = handle((await call('FPDFFormObj_GetObject', r(form), 0)).ok)
    const child = handle((await call('FPDFFormObj_GetObject', r(nested), 0)).ok)
    expect((await call('FPDFPage_RemoveObject', r(page), r(child))).error?.message).toContain(
      'container',
    )
    expect((await call('FPDFFormObj_RemoveObject', r(form), r(child))).error?.message).toContain(
      'container',
    )
    expect((await call('FPDFPageObj_Destroy', r(child))).error?.message).toContain('owned')
    expect((await call('FPDFFormObj_RemoveObject', r(nested), r(child))).ok).toBe(1)
    expect((await call('FPDFPage_InsertObjectAtIndex', r(page), r(child), 1)).ok).toBe(0)
    expect((await call('FPDFPage_InsertObjectAtIndex', r(page), r(child), 0)).ok).toBe(1)
    expect(
      (await call('FPDFPage_InsertObjectAtIndex', r(page), r(child), 0)).error?.message,
    ).toContain('owned')
    const text = handle((await call('FPDFText_LoadPage', r(page))).ok)
    expect((await call('FPDFTextObj_GetText', r(child), r(text), 0, 0)).ok).toBe(4)
    const annot = handle((await call('EPDFPage_GetAnnotByObjectNumber', r(page), 9)).ok)
    expect((await call('FPDFAnnot_GetAP', r(annot), 0, 0, 0)).ok).toBe(4)
    const apShort = handle((await send({ op: 'alloc', size: 3 })).ok)
    expect((await call('FPDFAnnot_GetAP', r(annot), 0, r(apShort), 4)).error?.message).toContain(
      'outside',
    )
    expect((await call('EPDFPage_RemoveAnnotByObjectNumber', r(page), 9)).error?.message).toContain(
      'Close',
    )
    await call('FPDFPage_CloseAnnot', r(annot))
    expect((await call('EPDFPage_RemoveAnnotByObjectNumber', r(page), 9)).ok).toBe(1)
    expect((await call('FPDFPage_RemoveAnnot', r(page), 0)).ok).toBe(1)
    const short = handle((await send({ op: 'alloc', size: 3 })).ok)
    expect(
      (await call('FPDFTextObj_GetText', r(child), r(text), r(short), 4)).error?.message,
    ).toContain('outside')
    const bytes = handle((await send({ op: 'alloc', size: 4 })).ok)
    expect((await call('FPDFTextObj_GetText', r(child), r(text), r(bytes), 4)).ok).toBe(4)
    expect((await send({ op: 'read', ref: bytes, offset: 0, len: 4 })).ok).toEqual({
      type: 'bytes',
      base64: 'QQAAAA==',
    })
    const backing = handle((await send({ op: 'alloc', size: 4 })).ok)
    const bitmap = handle((await call('FPDFBitmap_CreateEx', 1, 1, 4, r(backing), 4)).ok)
    expect((await send({ op: 'free', ref: backing })).error?.message).toContain('in use')
    const image = handle((await call('FPDFPageObj_NewImageObj', r(doc))).ok)
    expect((await call('FPDFImageObj_SetBitmap', 0, 0, r(image), r(bitmap))).ok).toBe(1)
    const array = handle((await send({ op: 'alloc', size: 4 })).ok)
    const encoded = new Uint8Array(4)
    new DataView(encoded.buffer).setUint32(0, page, true)
    await send({
      op: 'write',
      ref: array,
      offset: 0,
      bytes: { type: 'bytes', base64: btoa(String.fromCharCode(...encoded)) },
    })
    expect((await call('FPDFImageObj_SetBitmap', r(array), 1, r(image), r(bitmap))).ok).toBe(1)
    expect((await send({ op: 'read', ref: array, offset: 0, len: 4 })).ok).toEqual({
      type: 'bytes',
      base64: btoa(String.fromCharCode(...encoded)),
    })
    await call('FPDFBitmap_Destroy', r(bitmap))
    expect((await send({ op: 'free', ref: backing })).error).toBeUndefined()
    const redact = handle((await call('FPDFPage_CreateAnnot', r(page), 28)).ok)
    expect((await call('FPDFAnnot_SetRect', r(redact), r(short))).error?.message).toContain(
      'outside',
    )
    const rect = handle((await send({ op: 'alloc', size: 16 })).ok)
    expect((await call('FPDFAnnot_SetRect', r(redact), r(rect))).ok).toBe(1)
    expect((await call('EPDFAnnot_SetColor', r(redact), 2, 0, 0, 0)).ok).toBe(1)
    expect((await call('EPDFAnnot_ApplyRedaction', r(page), r(redact))).error?.message).toContain(
      'text',
    )
    await call('FPDFText_ClosePage', r(text))
    expect((await call('EPDFAnnot_ApplyRedaction', r(page), r(redact))).ok).toBe(1)
    expect((await call('FPDFPageObj_GetType', r(child))).error?.message).toContain('Stale')
    await call('FPDFPage_CloseAnnot', r(redact))
    await send({ op: 'close' })
    expect((await call('FPDFPageObj_GetType', r(child))).error?.message).toContain('Stale')
  })
})
