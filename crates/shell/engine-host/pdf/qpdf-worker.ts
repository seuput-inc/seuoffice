import createQpdf from '@neslinesli93/qpdf-wasm'
import { pdfiumOpens } from './pdfium-text'
import { repairPdf } from './repair'

interface Qpdf {
  callMain(args: string[]): number
  FS: { writeFile(path: string, bytes: Uint8Array): void; readFile(path: string): Uint8Array }
}

type Request =
  | { operation: 'encrypt'; bytes: ArrayBuffer; userPassword: string; ownerPassword?: string }
  | { operation: 'decrypt'; bytes: ArrayBuffer; password: string }
  | { operation: 'repair'; bytes: ArrayBuffer }

const worker = self as unknown as {
  location: Location
  onmessage: ((event: MessageEvent<Request>) => void) | null
  postMessage(message: unknown, transfer?: Transferable[]): void
}
worker.onmessage = async (event: MessageEvent<Request>) => {
  try {
    const request = event.data
    if (request.operation === 'repair') {
      const output = await repairPdf(new Uint8Array(request.bytes))
      const data = output.byteOffset === 0 && output.byteLength === output.buffer.byteLength && output.buffer instanceof ArrayBuffer
        ? output.buffer : output.slice().buffer
      worker.postMessage({ ok: data }, [data])
      return
    }
    if (request.operation === 'decrypt' && await pdfiumOpens(new Uint8Array(request.bytes), request.password) === 'wrong-password') {
      const error = new Error('the password does not open this PDF') as Error & { code: string }
      error.name = 'PdfLoadError'
      error.code = 'password-required'
      throw error
    }
    const init = createQpdf as unknown as (options: Record<string, unknown>) => Promise<Qpdf>
    const qpdf = await init({
      locateFile: () => new URL('./qpdf.wasm', worker.location.href).href,
      noInitialRun: true,
      thisProgram: 'qpdf',
      print: () => {},
      printErr: () => {},
    })
    qpdf.FS.writeFile('/in.pdf', new Uint8Array(request.bytes))
    const args = request.operation === 'encrypt'
      ? [
          '--encrypt', `--user-password=${request.userPassword}`,
          `--owner-password=${request.ownerPassword ?? crypto.randomUUID()}`, '--bits=256',
          ...(request.ownerPassword !== undefined ? ['--print=none', '--modify=none', '--extract=n', '--annotate=n'] : []),
          '--', '/in.pdf', '/out.pdf',
        ]
      : [`--password=${request.password}`, '--decrypt', '/in.pdf', '/out.pdf']
    let code: number
    try { code = qpdf.callMain(args) } catch { code = 2 }
    if (code !== 0 && code !== 3) throw new Error('qpdf could not write the file')
    let output: Uint8Array
    try { output = Uint8Array.from(qpdf.FS.readFile('/out.pdf')) }
    catch { throw new Error('qpdf could not write the file') }
    worker.postMessage({ ok: output.buffer }, [output.buffer])
  } catch (error) {
    const e = error instanceof Error ? error : new Error(String(error))
    const details = e as Error & { code?: unknown; reason?: unknown }
    worker.postMessage({ error: { name: e.name, message: e.message, code: details.code, reason: details.reason } })
  }
}
