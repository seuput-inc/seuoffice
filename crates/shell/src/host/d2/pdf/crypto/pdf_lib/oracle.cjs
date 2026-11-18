// Hosted differential-test oracle only; never loaded by the native host.
// JSON lines: {kind:"png"|"pdf", bytes:"base64"} -> one JSON result per line.
const readline = require('node:readline')
const UPNG = require('@pdf-lib/upng').default
const pdf = require('pdf-lib')

const b64 = (value) => Buffer.from(value).toString('base64')
function object(value) {
  if (value === undefined) return null
  if (value === pdf.PDFNull) return { kind: 'null' }
  if (value instanceof pdf.PDFBool) return { kind: 'bool', value: value.asBoolean() }
  if (value instanceof pdf.PDFNumber) return { kind: 'number', value: value.asNumber() }
  if (value instanceof pdf.PDFName) return { kind: 'name', bytes: b64(value.asBytes()) }
  if (value instanceof pdf.PDFString) return { kind: 'string', bytes: b64(Buffer.from(value.asString(), 'latin1')) }
  if (value instanceof pdf.PDFHexString) return { kind: 'hex', bytes: b64(Buffer.from(value.asString(), 'latin1')) }
  if (value instanceof pdf.PDFRef) return { kind: 'ref', object: value.objectNumber, generation: value.generationNumber }
  if (value instanceof pdf.PDFArray) return { kind: 'array', values: value.asArray().map(object) }
  if (value instanceof pdf.PDFRawStream) return { kind: 'stream', dict: object(value.dict), bytes: b64(value.getContents()) }
  if (value instanceof pdf.PDFInvalidObject) {
    const bytes = new Uint8Array(value.sizeInBytes())
    value.copyBytesInto(bytes, 0)
    return { kind: 'invalid', bytes: b64(bytes) }
  }
  if (value instanceof pdf.PDFDict) return { kind: 'dict', entries: value.entries().map(([key, item]) => [b64(key.asBytes()), object(item)]) }
  throw new Error(`unexpected pdf-lib object ${value?.constructor?.name}`)
}
async function run(request) {
  const bytes = Buffer.from(request.bytes, 'base64')
  if (request.kind === 'png') {
    const decoded = UPNG.decode(bytes)
    const frames = UPNG.toRGBA8(decoded)
    if (frames.length > 1) throw new Error('Animated PNGs are not supported')
    return { width: decoded.width, height: decoded.height, rgba: b64(new Uint8Array(frames[0])) }
  }
  if (request.kind === 'pdf') {
    const doc = await pdf.PDFDocument.load(bytes, { updateMetadata: false, throwOnInvalidObject: false })
    const context = doc.context
    return {
      largest: context.largestObjectNumber,
      root: object(context.trailerInfo.Root),
      info: object(context.trailerInfo.Info),
      id: object(context.trailerInfo.ID),
      encrypt: object(context.trailerInfo.Encrypt),
      objects: context.enumerateIndirectObjects().map(([ref, value]) => ({ ref: object(ref), value: object(value) })),
      pages: doc.getPages().map((page) => object(page.ref)),
    }
  }
  throw new Error('unknown fixture kind')
}
const input = readline.createInterface({ input: process.stdin, crlfDelay: Infinity })
let queue = Promise.resolve()
input.on('line', (line) => {
  queue = queue.then(async () => {
    try { process.stdout.write(JSON.stringify({ ok: await run(JSON.parse(line)) }) + '\n') }
    catch (error) { process.stdout.write(JSON.stringify({ error: String(error?.message ?? error) }) + '\n') }
  })
})
