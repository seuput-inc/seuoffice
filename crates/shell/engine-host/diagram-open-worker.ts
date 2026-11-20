import { openDiagramStep } from './markup'

const worker = self as unknown as {
  onmessage: ((event: MessageEvent<unknown>) => void) | null
  postMessage(message: unknown): void
}

worker.onmessage = ({ data }) => {
  try {
    worker.postMessage({ ok: true, value: openDiagramStep(data) })
  } catch (error) {
    const failure = error instanceof Error ? error : new Error(String(error))
    worker.postMessage({ ok: false, name: failure.name, message: failure.message })
  }
}
