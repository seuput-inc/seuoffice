/**
 * Save As lists every format the document converts to: Word and ODT save the
 * document, PDF/HTML/Markdown/page images write a copy through the regular
 * export with the picked path, and the open document keeps its path and unsaved
 * state. Pure dialog rules (filters, suggestion, picked kind) plus the real
 * file actions on a real editor with a stub main process.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { Editor } from '@tiptap/core'
import { buildBlankDocx } from '@seuoffice/docx-engine'
import { editorExtensions } from '../src/renderer/editor/extensions'
import type { DocState } from '../src/renderer/doc-state'
import type { OpenDocxResult } from '../src/shared/ipc'
import { loadFile, save, exportSaveAsCopy, exportPdf } from '../src/renderer/file-actions'
import {
  hwpExportFilters,
  hwpExportTarget,
  saveAsDefaultPath,
  saveAsFilters,
  saveAsTarget,
} from '../src/main/save-as-formats'

const handoff = new Map<string, Uint8Array>()
vi.mock('../src/renderer/doc-bytes', () => ({
  fetchDocBytes: async (url: string) => {
    const bytes = handoff.get(url)
    if (!bytes) throw new Error(`no bytes for ${url}`)
    handoff.delete(url)
    return bytes.slice()
  },
}))

vi.mock('../src/renderer/components/toast-bus', () => ({
  showToast: () => {},
  setToastEmitter: () => {},
}))

// page images: the rasterizer is pdf.js; only the page count matters here
let pdfPages = 1
vi.mock('pdfjs-dist/legacy/build/pdf.mjs', () => ({
  GlobalWorkerOptions: {},
  getDocument: () => ({
    promise: Promise.resolve({
      get numPages() {
        return pdfPages
      },
      getPage: async () => ({
        getViewport: () => ({ width: 10, height: 10 }),
        render: () => ({ promise: Promise.resolve() }),
        cleanup: () => {},
      }),
    }),
    destroy: async () => {},
  }),
}))
vi.mock('pdfjs-dist/legacy/build/pdf.worker.min.mjs?url', () => ({ default: 'worker.js' }))

type Ctx = Parameters<typeof loadFile>[0]

const label = (key: string) => `<${key}>`

describe('Save As dialog rules', () => {
  it('lists the native formats first, then every export', () => {
    expect(saveAsFilters('docx', true, label)).toEqual([
      { name: '<filterWord>', extensions: ['docx'] },
      { name: '<filterOdt>', extensions: ['odt'] },
      { name: '<filterPdf>', extensions: ['pdf'] },
      { name: '<filterHwp>', extensions: ['hwp'] },
      { name: '<filterHwpx>', extensions: ['hwpx'] },
      { name: '<filterHtml>', extensions: ['html', 'htm'] },
      { name: '<filterMarkdown>', extensions: ['md', 'markdown'] },
      { name: '<filterPageImages>', extensions: ['png'] },
    ])
    // an ODT document defaults to ODT
    expect(saveAsFilters('odt', true, label).map((f) => f.extensions[0])).toEqual([
      'odt',
      'docx',
      'pdf',
      'hwp',
      'hwpx',
      'html',
      'md',
      'png',
    ])
  })

  it('a first save offers only the native formats, as before', () => {
    expect(saveAsFilters('docx', false, label)).toEqual([
      { name: '<filterWord>', extensions: ['docx'] },
      { name: '<filterOdt>', extensions: ['odt'] },
    ])
  })

  it('drops the document extension from the suggestion only when exports are offered', () => {
    expect(saveAsDefaultPath('/docs/report.docx', true)).toBe('/docs/report')
    expect(saveAsDefaultPath('/docs/form.ODT', true)).toBe('/docs/form')
    expect(saveAsDefaultPath('/docs/report.docx', false)).toBe('/docs/report.docx')
    expect(saveAsDefaultPath('v1.2 notes', true)).toBe('v1.2 notes')
  })

  it('reads the picked kind from the extension', () => {
    expect(saveAsTarget('/d/a.docx', 'odt', true)).toEqual({ path: '/d/a.docx', kind: 'docx' })
    expect(saveAsTarget('/d/a.odt', 'docx', true)).toEqual({ path: '/d/a.odt', kind: 'odt' })
    expect(saveAsTarget('/d/a.pdf', 'docx', true)).toEqual({ path: '/d/a.pdf', kind: 'pdf' })
    expect(saveAsTarget('/d/a.htm', 'docx', true)).toEqual({ path: '/d/a.htm', kind: 'html' })
    expect(saveAsTarget('/d/a.md', 'docx', true)).toEqual({ path: '/d/a.md', kind: 'md' })
    expect(saveAsTarget('/d/a.hwp', 'docx', true)).toEqual({ path: '/d/a.hwp', kind: 'hwp' })
    expect(saveAsTarget('/d/a.HWPX', 'odt', true)).toEqual({ path: '/d/a.HWPX', kind: 'hwpx' })
    expect(saveAsTarget('C:\\d\\A.PNG', 'docx', true)).toEqual({
      path: 'C:\\d\\A.PNG',
      kind: 'png',
    })
  })

  it("the Hangul export's own dialog offers the new-document format first", () => {
    expect(hwpExportFilters('hwp', label)).toEqual([
      { name: '<filterHwp>', extensions: ['hwp'] },
      { name: '<filterHwpx>', extensions: ['hwpx'] },
    ])
    expect(hwpExportFilters('hwpx', label).map((f) => f.extensions[0])).toEqual(['hwpx', 'hwp'])
    expect(hwpExportTarget('/d/a.HWP', 'hwpx')).toEqual({ path: '/d/a.HWP', format: 'hwp' })
    // a dialog that adds no extension: the offered format
    expect(hwpExportTarget('/d/report', 'hwpx')).toEqual({ path: '/d/report.hwpx', format: 'hwpx' })
  })

  it('a bare or unknown name falls back to the document format', () => {
    // some Linux dialogs do not add the chosen type's extension
    expect(saveAsTarget('/d/report', 'docx', true)).toEqual({
      path: '/d/report.docx',
      kind: 'docx',
    })
    expect(saveAsTarget('/d/report', 'odt', true)).toEqual({ path: '/d/report.odt', kind: 'odt' })
    expect(saveAsTarget('/d/notes.txt', 'docx', true)).toEqual({
      path: '/d/notes.txt.docx',
      kind: 'docx',
    })
    // without exports offered an export extension is just part of the name
    expect(saveAsTarget('/d/a.pdf', 'docx', false)).toEqual({
      path: '/d/a.pdf.docx',
      kind: 'docx',
    })
  })
})

// ---- the renderer side: real file actions, stub main process ----

let urlSeq = 0
function openResult(path: string, bytes: Uint8Array): OpenDocxResult {
  const url = `test-bytes://${++urlSeq}`
  handoff.set(url, bytes)
  return { path, name: path.split('/').pop() ?? path, dataUrl: url, hash: 'h' }
}

type Pick = { path: string; kind?: string; dir?: string; pdfPath?: string }
let pick: Pick | null = null
let desktop: Record<string, ReturnType<typeof vi.fn>>

function installDesktop(): void {
  const wrote = async (path: string) => ({ ok: true, path })
  desktop = {
    getRecentFiles: vi.fn(async () => []),
    getSystemLocale: vi.fn(async () => 'en-US'),
    docPasswordIntentRevision: vi.fn(async () => 0),
    discardDocPasswordIntents: vi.fn(async () => ({ ok: true })),
    saveDocx: vi.fn(wrote),
    saveDocxNew: vi.fn(wrote),
    saveDocxAs: vi.fn(wrote),
    writeRecoveryCopy: vi.fn(wrote),
    pickSaveAsPath: vi.fn(async () => pick),
    exportPdf: vi.fn(async (_name: string, _w: number, _h: number, outPath?: string) =>
      outPath ? { ok: true, path: outPath } : { ok: false },
    ),
    exportHtml: vi.fn(async (_name: string, _html: string, outPath?: string) =>
      outPath ? { ok: true, path: outPath } : { ok: false },
    ),
    exportMarkdown: vi.fn(
      async (_name: string, _md: string, _images: unknown[], outPath?: string) =>
        outPath ? { ok: true, path: outPath, imagesDropped: 0 } : { ok: false },
    ),
    exportHwp: vi.fn(async (_name: string, _docx: ArrayBuffer, outPath?: string) =>
      outPath ? { ok: true, path: outPath } : { ok: false },
    ),
    pickExportImagesTarget: vi.fn(async () => null),
    takeExportPdf: vi.fn(async () => ({ ok: true, base64: btoa('%PDF') })),
    writeExportImage: vi.fn(async (dir: string, fileName: string) => ({
      ok: true,
      path: `${dir}/${fileName}`,
    })),
  }
  ;(window as unknown as { desktop: unknown }).desktop = desktop
}

const editors: Editor[] = []

function makeCtx(): Ctx {
  const editor = new Editor({
    element: document.createElement('div'),
    extensions: editorExtensions,
  })
  editors.push(editor)
  const state: Record<string, unknown> = {
    editor,
    doc: null as DocState | null,
    dirtyRef: { current: false },
    saveInFlightRef: { current: false },
    saveIncompleteRef: { current: false },
    pendingMixedExportRef: { current: false },
    printAutoOpenedPreviewRef: { current: false },
    section: null,
    sectionDirty: false,
    sections: [],
    sectionsDirty: [],
    trailingStartType: null,
    pageColor: null,
    pageColorDirty: false,
    header: null,
    headerDirty: false,
    footer: null,
    footerDirty: false,
    hfVariants: {},
    hfVariantsDirty: [],
    sectionHfEdits: {},
    hfLinks: {},
    titlePg: false,
    titlePgDirty: false,
    evenOddHf: false,
    evenOddHfDirty: false,
    mirrorMargins: false,
    mirrorMarginsDirty: false,
    pgNumEdit: null,
    pgNumDirtySections: [],
    pendingNumbering: { newDefs: [], restartNums: [], levelEdits: [], picBullets: [] },
    numberingDirty: false,
    styleUpserts: {},
    comments: [],
    commentsDirty: false,
    watermark: null,
    watermarkDirty: false,
    watermarkStyle: null,
    watermarkPicture: null,
    inkAnnotations: [],
    inksDirty: false,
    footnotes: [],
    endnotes: [],
    notesDirty: false,
    sources: [],
    sourcesDirty: false,
    zoteroDocumentData: '',
    zoteroDocumentDataDirty: false,
    themeFonts: null,
    themeFontsDirty: false,
    themeColors: null,
    themeColorsDirty: false,
    protection: null,
    protectionDirty: false,
    writeProtection: null,
    writeProtectionDirty: false,
    removePersonalInfo: false,
    removePersonalInfoDirty: false,
    setDoc: (next: unknown) => {
      state.doc = typeof next === 'function' ? (next as (p: unknown) => unknown)(state.doc) : next
    },
  }
  return new Proxy(state, {
    get(target, key) {
      if (
        typeof key === 'string' &&
        !(key in target) &&
        key !== 'settleFontSettings' &&
        /^(set|on|prompt|bump)/.test(key)
      )
        return () => {}
      return target[key as string]
    },
  }) as unknown as Ctx
}

function docOf(ctx: Ctx): DocState {
  const doc = (ctx as unknown as { doc: DocState | null }).doc
  if (!doc) throw new Error('no document')
  return doc
}

/** an open /docs/report.docx with an unsaved edit */
async function openEdited(): Promise<Ctx> {
  const ctx = makeCtx()
  expect(await loadFile(ctx, openResult('/docs/report.docx', await buildBlankDocx({})))).toBe('ok')
  ctx.editor!.commands.insertContent('unsaved words')
  ctx.dirtyRef.current = true
  return ctx
}

function expectUntouched(ctx: Ctx): void {
  expect(docOf(ctx).filePath).toBe('/docs/report.docx')
  expect(docOf(ctx).fileName).toBe('report.docx')
  expect(ctx.dirtyRef.current).toBe(true)
  expect(ctx.editor!.getText()).toContain('unsaved words')
  expect(desktop.saveDocxAs).not.toHaveBeenCalled()
  expect(desktop.saveDocx).not.toHaveBeenCalled()
  expect(desktop.saveDocxNew).not.toHaveBeenCalled()
}

describe('Save As into another format', () => {
  beforeEach(() => {
    // the jsdom test environment has no CSS.escape (document style CSS uses it)
    ;(globalThis as { CSS?: unknown }).CSS ??= { escape: (s: string) => s }
    installDesktop()
    pick = null
    pdfPages = 1
    // a PDF export prints the pagination preview's page sheets (exportPdf); the App
    // mounts that preview, so the test puts one letter-size sheet in the page
    const sheet = document.createElement('div')
    sheet.className = 'pv-page'
    sheet.style.width = '816px'
    sheet.style.height = '1056px'
    document.body.append(sheet)
  })
  afterEach(() => {
    while (editors.length) editors.pop()!.destroy()
    document.querySelectorAll('.pv-page').forEach((sheet) => sheet.remove())
    vi.restoreAllMocks()
  })

  it('offers exports when saving an existing document under a new name', async () => {
    const ctx = await openEdited()
    expect(await save(ctx, true)).toBe(false)
    expect(desktop.pickSaveAsPath.mock.calls[0].slice(0, 4)).toEqual([
      'report.docx',
      '/docs/report.docx',
      'docx',
      true,
    ])
    expectUntouched(ctx)
  })

  it('a Word pick still saves the document to the new path', async () => {
    const ctx = await openEdited()
    pick = { path: '/docs/copy.docx', kind: 'docx' }
    expect(await save(ctx, true)).toBe(true)
    expect(desktop.saveDocxAs).toHaveBeenCalledTimes(1)
    expect(desktop.saveDocxAs.mock.calls[0][0]).toBe('/docs/copy.docx')
    expect(docOf(ctx).filePath).toBe('/docs/copy.docx')
    expect(ctx.dirtyRef.current).toBe(false)
    expect(desktop.exportPdf).not.toHaveBeenCalled()
  })

  it('a PDF pick writes a copy through the PDF export and leaves the document as it was', async () => {
    const ctx = await openEdited()
    pick = { path: '/out/report.pdf', kind: 'pdf' }
    expect(await save(ctx, true)).toBe(false)
    expect(desktop.exportPdf).toHaveBeenCalledTimes(1)
    expect(desktop.exportPdf.mock.calls[0][3]).toBe('/out/report.pdf')
    expectUntouched(ctx)
  })

  it('HTML and Markdown picks go to their exports with the picked path', async () => {
    const ctx = await openEdited()
    pick = { path: '/out/report.html', kind: 'html' }
    expect(await save(ctx, true)).toBe(false)
    expect(desktop.exportHtml.mock.calls[0][2]).toBe('/out/report.html')
    pick = { path: '/out/report.md', kind: 'md' }
    expect(await save(ctx, true)).toBe(false)
    expect(desktop.exportMarkdown.mock.calls[0][3]).toBe('/out/report.md')
    expectUntouched(ctx)
  })

  it('a Hangul pick sends the docx a save would write to the Hangul export', async () => {
    const ctx = await openEdited()
    pick = { path: '/out/report.hwpx', kind: 'hwpx' }
    expect(await save(ctx, true)).toBe(false)
    expect(desktop.exportHwp).toHaveBeenCalledTimes(1)
    const [name, docx, outPath, passwordSet] = desktop.exportHwp.mock.calls[0]
    expect([name, outPath, passwordSet]).toEqual(['report.docx', '/out/report.hwpx', false])
    // a whole docx: a zip
    expect([...new Uint8Array(docx as ArrayBuffer).slice(0, 2)]).toEqual([0x50, 0x4b])
    pick = { path: '/out/report.hwp', kind: 'hwp' }
    expect(await save(ctx, true)).toBe(false)
    expect(desktop.exportHwp.mock.calls[1][2]).toBe('/out/report.hwp')
    expectUntouched(ctx)
  })

  it('page images: one page keeps the typed name', async () => {
    vi.spyOn(HTMLCanvasElement.prototype, 'getContext').mockReturnValue(null)
    vi.spyOn(HTMLCanvasElement.prototype, 'toBlob').mockImplementation(function (cb) {
      cb(new Blob([new Uint8Array([137, 80, 78, 71])], { type: 'image/png' }))
    })
    const ctx = await openEdited()
    pick = { path: '/out/report.png', kind: 'png', dir: '/out', pdfPath: '/tmp/x.pdf' }
    expect(await save(ctx, true)).toBe(false)
    expect(desktop.pickExportImagesTarget).not.toHaveBeenCalled()
    expect(desktop.exportPdf.mock.calls[0][3]).toBe('/tmp/x.pdf')
    expect(desktop.writeExportImage.mock.calls.map((c) => [c[0], c[1]])).toEqual([
      ['/out', 'report.png'],
    ])
    expectUntouched(ctx)
  })

  it('page images: several pages are numbered beside the typed name', async () => {
    vi.spyOn(HTMLCanvasElement.prototype, 'getContext').mockReturnValue(null)
    vi.spyOn(HTMLCanvasElement.prototype, 'toBlob').mockImplementation(function (cb) {
      cb(new Blob([new Uint8Array([137, 80, 78, 71])], { type: 'image/png' }))
    })
    pdfPages = 3
    const ctx = await openEdited()
    pick = { path: '/out/report.png', kind: 'png', dir: '/out', pdfPath: '/tmp/y.pdf' }
    expect(await save(ctx, true)).toBe(false)
    expect(desktop.writeExportImage.mock.calls.map((c) => c[1])).toEqual([
      'report-01.png',
      'report-02.png',
      'report-03.png',
    ])
    expectUntouched(ctx)
  })

  it.each([true, false])('owned full Save As waits for the actual copy result %s', async (ok) => {
    const ctx = await openEdited()
    ctx.ownedSaveRequestId = 'captured-close-save'
    pick = { path: '/out/report.html', kind: 'html' }
    let finish: ((value: { ok: boolean; path: string }) => void) | undefined
    desktop.exportHtml.mockImplementation(
      () =>
        new Promise((resolve) => {
          finish = resolve
        }),
    )
    let completed = false
    const saving = save(ctx, true).then((result) => {
      completed = true
      return result
    })
    await vi.waitFor(() => expect(finish).toBeDefined())
    expect(completed).toBe(false)
    expect(desktop.exportHtml.mock.calls[0][3]).toBe('captured-close-save')
    finish!({ ok, path: '/out/report.html' })
    expect(await saving).toBe(ok)
    expectUntouched(ctx)
  })

  it.each([
    ['pdf', 'exportPdf', 6],
    ['html', 'exportHtml', 3],
    ['md', 'exportMarkdown', 4],
    ['hwp', 'exportHwp', 4],
    ['hwpx', 'exportHwp', 4],
  ] as const)('keeps the owned save token through a %s copy', async (kind, method, tokenIndex) => {
    const ctx = await openEdited()
    ctx.ownedSaveRequestId = 'captured-close-save'
    expect(await exportSaveAsCopy(ctx, { path: `/out/report.${kind}`, kind })).toBe(true)
    expect(desktop[method].mock.calls[0][tokenIndex]).toBe('captured-close-save')
    expectUntouched(ctx)
  })

  it('keeps the owned token through every PNG stage and page write', async () => {
    vi.spyOn(HTMLCanvasElement.prototype, 'getContext').mockReturnValue(null)
    vi.spyOn(HTMLCanvasElement.prototype, 'toBlob').mockImplementation(function (cb) {
      cb(new Blob([new Uint8Array([137, 80, 78, 71])], { type: 'image/png' }))
    })
    pdfPages = 3
    const ctx = await openEdited()
    ctx.ownedSaveRequestId = 'captured-close-save'
    expect(
      await exportSaveAsCopy(ctx, {
        path: '/out/report.png',
        kind: 'png',
        dir: '/out',
        pdfPath: '/tmp/owned.pdf',
      }),
    ).toBe(true)
    expect(desktop.exportPdf.mock.calls[0][6]).toBe('captured-close-save')
    expect(desktop.takeExportPdf).toHaveBeenCalledWith('/tmp/owned.pdf', 'captured-close-save')
    expect(desktop.writeExportImage.mock.calls.map((call) => [call[1], call[3]])).toEqual([
      ['report-01.png', 'captured-close-save'],
      ['report-02.png', 'captured-close-save'],
      ['report-03.png', 'captured-close-save'],
    ])
    expectUntouched(ctx)
  })

  it('keeps the owned token through failed whole-print and bisected chunk retries', async () => {
    const sheet = document.querySelector('.pv-page')!
    document.body.append(sheet.cloneNode(true))
    desktop.exportPdf.mockResolvedValue({
      ok: false,
      error: 'print failed',
      path: '/out/report.pdf',
    })
    desktop.printPdfBuffer = vi
      .fn()
      .mockResolvedValueOnce({ ok: false, error: 'retry smaller' })
      .mockResolvedValue({ ok: true, base64: btoa('%PDF') })
    desktop.saveMergedPdf = vi.fn(async () => ({ ok: true, path: '/out/report.pdf' }))
    const ctx = await openEdited()
    ctx.ownedSaveRequestId = 'captured-close-save'
    expect(await exportSaveAsCopy(ctx, { path: '/out/report.pdf', kind: 'pdf' })).toBe(true)
    expect(desktop.exportPdf.mock.calls[0][6]).toBe('captured-close-save')
    expect(desktop.printPdfBuffer).toHaveBeenCalledTimes(3)
    expect(desktop.printPdfBuffer.mock.calls.map((call) => call[3])).toEqual([
      'captured-close-save',
      'captured-close-save',
      'captured-close-save',
    ])
    expect(desktop.saveMergedPdf.mock.calls[0][1]).toHaveLength(2)
    expect(desktop.saveMergedPdf.mock.calls[0][4]).toBe('captured-close-save')
    expect(document.querySelectorAll('.pv-print-skip, .pv-print-tail')).toHaveLength(0)
  })

  it('does not complete a deferred owned export before the actual preview print result', async () => {
    document.querySelectorAll('.pv-page').forEach((sheet) => sheet.remove())
    const ctx = await openEdited()
    ctx.ownedSaveRequestId = 'captured-close-save'
    let completed = false
    const saving = exportSaveAsCopy(ctx, { path: '/out/report.pdf', kind: 'pdf' }).then(
      (result) => {
        completed = true
        return result
      },
    )
    await vi.waitFor(() => expect(ctx.pendingMixedExportRef.current).toBeTruthy())
    const pending = ctx.pendingMixedExportRef.current
    if (!pending) throw new Error('missing preview export')
    expect(pending.how.ownedSaveRequestId).toBe('captured-close-save')
    expect(pending.awaitingPreviewMount).toBe(true)
    expect(completed).toBe(false)
    expect(desktop.exportPdf).not.toHaveBeenCalled()
    const sheet = document.createElement('div')
    sheet.className = 'pv-page'
    sheet.style.width = '816px'
    sheet.style.height = '1056px'
    document.body.append(sheet)
    let printDone: ((value: { ok: boolean; path: string }) => void) | undefined
    desktop.exportPdf.mockImplementation(
      () =>
        new Promise((resolve) => {
          printDone = resolve
        }),
    )
    ctx.pendingMixedExportRef.current = false
    const printing = exportPdf(ctx, pending.outPath, pending.how)
    await vi.waitFor(() => expect(printDone).toBeDefined())
    expect(completed).toBe(false)
    expect(desktop.exportPdf.mock.calls[0][6]).toBe('captured-close-save')
    printDone!({ ok: true, path: '/out/report.pdf' })
    pending.resolve(await printing)
    expect(await saving).toBe(true)
  })
})
