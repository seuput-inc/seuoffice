import { readDocument } from './backend'
import { markupSteps } from './markup'
import { runDiagramOpen } from './diagram-open-runner'
import { openMindmap } from './xmind'
import { parseWorkbookSchema } from './sheets'
import { runLegacyDoc } from './legacy-doc-runner'
import { shortDatePattern } from './locale'
import { runHwp } from './docs-vendor'
import { runHtml } from './docs-vendor/html-runner'
import { createRequestCallbacks, type NativeCallback } from './transport'
import { hasPdfOperation, pdfBinaryService, pdfStructureService } from './pdf'
import { runPdfPageOperation } from './pdf/page-runner'
import { runPdfiumSession } from './pdf/pdfium-session'
import { subsetFont } from './pdf/font-subset'
import { canonicalLocale, latexMathMl } from './pdf/math'
import { renderAll, renderSlide, templateCatalog } from './slides'
import { measurePageText, measureAutofit } from './slides-layout'
import { runDocumentEditSession } from './pdf/document-edit-primitives'

type HostReply =
  | { ok: unknown }
  | {
      error: {
        name: string
        message: string
        code?: unknown
        reason?: unknown
        validRange?: unknown
      }
    }
interface ServiceWindow extends Window {
  __SEUOFFICE_ENGINE_TOKEN__: string
  __TAURI_INTERNALS__: {
    invoke(command: string, args: Record<string, unknown>): Promise<unknown>
  }
  __seuofficeEngineRequest(id: number, name: string, input: unknown, nonce: string): Promise<void>
  __seuofficeEngineCancel(id: number): void
  /** boot.js: what this page is doing, told to the shell without the IPC that readiness needs */
  __seuofficeEngineLog?(message: string): void
}
const service = window as unknown as ServiceWindow
const token = service.__SEUOFFICE_ENGINE_TOKEN__
const requests = new Map<number, AbortController>()

export async function answer(
  name: string,
  input: unknown,
  signal: AbortSignal = new AbortController().signal,
  callback?: NativeCallback,
): Promise<HostReply> {
  try {
    signal?.throwIfAborted()
    if (name === 'locale.shortDate') return { ok: shortDatePattern(input) }
    if (name === 'pdf.latexMathMl') return { ok: latexMathMl(input) }
    if (name === 'pdf.canonicalLocale') return { ok: canonicalLocale(input) }
    if (name === 'pdf.subsetFont') return { ok: await subsetFont(input, signal) }
    if (name === 'slides.templateCatalog') return { ok: templateCatalog() }
    if (
      name === 'slides.renderAll' ||
      name === 'slides.renderSlide' ||
      name === 'slides.measurePageText' ||
      name === 'slides.measureAutofit'
    ) {
      if (!callback) throw new Error('the slide operation has no native request capability')
      const operation = {
        'slides.renderAll': renderAll,
        'slides.renderSlide': renderSlide,
        'slides.measurePageText': measurePageText,
        'slides.measureAutofit': measureAutofit,
      }[name]
      return { ok: await operation(input, callback, signal) }
    }
    if (name === 'pdf.documentEditSession') {
      if (!callback) throw new Error('the PDF document session has no native request capability')
      return { ok: await runDocumentEditSession(input, signal, callback) }
    }
    if (name === 'pdf.pdfiumSession' || name === 'pdf.structureOperation') {
      if (!callback) throw new Error('the PDF operation has no native request capability')
      return {
        ok:
          name === 'pdf.pdfiumSession'
            ? await runPdfiumSession(input, signal, callback)
            : await pdfStructureService(input, signal, callback),
      }
    }
    if (name === 'pdf.qpdfEncrypt' || name === 'pdf.qpdfDecrypt' || name === 'pdf.repair') {
      return { ok: await pdfBinaryService(name, input, signal) }
    }
    if (hasPdfOperation(name)) return { ok: await runPdfPageOperation(name, input, signal) }
    if (name === 'docs.hwp') return { ok: await runHwp(input, signal) }
    if (name === 'docs.htmlToDocx') {
      if (!callback) throw new Error('the HTML conversion has no native request capability')
      if (
        !input ||
        typeof input !== 'object' ||
        !('path' in input) ||
        typeof input.path !== 'string'
      ) {
        throw new TypeError('invalid HTML conversion path')
      }
      const naturalTableWidth = 'naturalTableWidth' in input ? input.naturalTableWidth : true
      if (typeof naturalTableWidth !== 'boolean')
        throw new TypeError('invalid HTML table width option')
      return { ok: await runHtml(naturalTableWidth, signal, callback) }
    }
    if (name === 'legacy-doc.convert') return { ok: await runLegacyDoc(input, signal) }
    if (name === 'reader.pdf' || name === 'reader.xmind') {
      return { ok: await readDocument(name, input) }
    }
    if (name === 'diagram.open') return { ok: await runDiagramOpen(input, signal) }
    if (name === 'mindmap.open') return { ok: await openMindmap(input) }
    if (name === 'sheets.schema') return { ok: parseWorkbookSchema(input) }
    const step = markupSteps[name]
    if (!step) {
      return { error: { name: 'HostUnavailable', message: `no host step ${name}` } }
    }
    return { ok: step(input) ?? null }
  } catch (error) {
    const thrown = error instanceof Error ? error : new Error(String(error))
    const details = thrown as Error & { code?: unknown; reason?: unknown; validRange?: unknown }
    return {
      error: {
        name: thrown.name,
        message: thrown.message,
        code: details.code,
        reason: details.reason,
        validRange: details.validRange,
      },
    }
  }
}

service.__seuofficeEngineRequest = async (id, name, input, nonce) => {
  if (!Number.isSafeInteger(id) || id <= 0 || requests.has(id)) {
    throw new TypeError('invalid engine request identity')
  }
  const controller = new AbortController()
  requests.set(id, controller)
  try {
    const callback = createRequestCallbacks(id, name, nonce, controller.signal)
    const reply = await answer(name, input, controller.signal, callback)
    if (!controller.signal.aborted) {
      await service.__TAURI_INTERNALS__.invoke('engine_host_reply', {
        token,
        id,
        name,
        nonce,
        reply,
      })
    }
  } finally {
    requests.delete(id)
  }
}
service.__seuofficeEngineCancel = (id) => requests.get(id)?.abort()
window.addEventListener('pagehide', () => {
  for (const controller of requests.values()) controller.abort()
  requests.clear()
})
service.__seuofficeEngineLog?.('main: the bundle is evaluated, sending engine_host_ready')
const unanswered = setTimeout(
  () => service.__seuofficeEngineLog?.('main: engine_host_ready has no answer after 10 s'),
  10_000,
)
try {
  await service.__TAURI_INTERNALS__.invoke('engine_host_ready', { token })
} finally {
  clearTimeout(unanswered)
}
service.__seuofficeEngineLog?.('main: engine_host_ready acknowledged')
