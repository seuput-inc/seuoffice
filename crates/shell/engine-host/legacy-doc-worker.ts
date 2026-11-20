import { initEngine } from '@seuoffice/engine-wasm'
import { convertLegacyDoc } from './legacy-doc'

const worker = self as unknown as {
  onmessage: ((event: MessageEvent<unknown>) => void) | null
  postMessage(message: unknown): void
}

worker.onmessage = async ({ data }) => {
  try {
    const office = await fetch(new URL('./docs-vendor/office.wasm', import.meta.url))
    if (!office.ok) throw new Error('Legacy DOC conversion engine could not be loaded')
    await initEngine(office)
    worker.postMessage({ ok: true, value: await convertLegacyDoc(data) })
  } catch (error) {
    const failure = error instanceof Error ? error : new Error(String(error))
    worker.postMessage({ ok: false, name: failure.name, message: failure.message })
  }
}
