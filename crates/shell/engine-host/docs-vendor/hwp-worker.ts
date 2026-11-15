import { initEngine } from '@seuoffice/engine-wasm'
import { Docx2HwpError, HwpEngine, docxToHwp } from '@seuoffice/docx2hwp'

interface Job {
  docx: ArrayBuffer
  format: 'hwp' | 'hwpx'
}

const worker = self as unknown as {
  onmessage: ((event: MessageEvent<Job>) => void) | null
  postMessage(message: unknown, transfer?: Transferable[]): void
}

worker.onmessage = async ({ data }) => {
  try {
    const [office, rhwp] = await Promise.all([
      fetch(new URL('./office.wasm', import.meta.url)),
      fetch(new URL('./rhwp.wasm', import.meta.url)),
    ])
    if (!office.ok || !rhwp.ok) throw new Error('Hangul conversion engine assets could not be loaded')
    await initEngine(office)
    const engine = await HwpEngine.start(new Uint8Array(await rhwp.arrayBuffer()))
    const result = await docxToHwp(new Uint8Array(data.docx), data.format, engine)
    const bytes = result.bytes.slice().buffer
    worker.postMessage({ ok: true, bytes, losses: result.losses }, [bytes])
  } catch (error) {
    worker.postMessage({
      ok: false,
      code: error instanceof Docx2HwpError ? error.code : 'failed',
      error: error instanceof Error ? error.message : String(error),
    })
  }
}
