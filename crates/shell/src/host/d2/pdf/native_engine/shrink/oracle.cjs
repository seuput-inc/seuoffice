// Hosted differential oracle. Run with the original Electron binary in Node mode.
const path = require('node:path')
const fs = require('node:fs')
const Module = require('node:module')
const zlib = require('node:zlib')
const root = process.env.PDF_SHRINK_REPO_ROOT
if (!root) throw new Error('PDF_SHRINK_REPO_ROOT is required')
const source = path.join(root, 'apps/pdf/src/main/pdf-shrink.ts')
const appRequire = Module.createRequire(path.join(root, 'apps/pdf/package.json'))
const ts = appRequire('typescript')
const pdf = appRequire('pdf-lib')
function loadTypescript(filename) {
  const compiled = ts.transpileModule(fs.readFileSync(filename, 'utf8'), { compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 } }).outputText
  const target = new Module(filename, module)
  target.filename = filename
  target.paths = Module._nodeModulePaths(path.dirname(filename))
  target.require = id => id === 'pdf-lib' ? pdf : Module.prototype.require.call(target, id)
  target._compile(compiled, filename)
  return target.exports
}
const { shrinkDocument } = loadTypescript(source)
const b64 = bytes => Buffer.from(bytes).toString('base64')
const refKey = ref => `${ref.objectNumber}:${ref.generationNumber}`
const asInt = value => { const n = Number(String(value ?? '').replace(/^\//, '')); return Number.isFinite(n) ? n : 0 }
const name = value => pdf.PDFName.of(value)
function records(doc) {
  const context = doc.context
  return context.enumerateIndirectObjects().flatMap(([ref, obj]) => {
    if (!(obj instanceof pdf.PDFRawStream)) return []
    const d = obj.dict, mask = d.get(name('SMask')), color = context.lookup(d.get(name('ColorSpace')))
    return [{ ref: refKey(ref), subtype: String(d.get(name('Subtype')) ?? ''), filter: String(d.get(name('Filter')) ?? ''),
      width: asInt(d.get(name('Width'))), height: asInt(d.get(name('Height'))), bitsPerComponent: asInt(d.get(name('BitsPerComponent')) ?? 8),
      colorSpace: color instanceof pdf.PDFName ? color.asString() : null, decodeParms: d.get(name('DecodeParms')) !== undefined,
      matte: d.get(name('Matte')) !== undefined, maskPresent: mask !== undefined, maskRef: mask instanceof pdf.PDFRef ? refKey(mask) : null,
      maskIsStream: mask instanceof pdf.PDFRef && context.lookup(mask) instanceof pdf.PDFRawStream, bytesBase64: b64(obj.contents),
      intent: String(d.get(name('Intent')) ?? ''), decode: String(d.get(name('Decode')) ?? '') }]
  })
}
async function apply() {
  const input = JSON.parse(fs.readFileSync(0, 'utf8'))
  const file = path.join(root, 'crates/shell/engine-host/pdf/document-edit-primitives.ts')
  const { PdfDocumentPrimitives } = loadTypescript(file)
  const doc = await PdfDocumentPrimitives.open(input.sourceBase64)
  await doc.execute({op:'shrinkStreams',metadataOnly:true})
  await doc.execute({op:'replaceRawStreams',replacements:input.replacements})
  const encoded = await doc.execute({op:'save',useObjectStreams:true})
  const output = Buffer.from(encoded, 'base64')
  const reopened = await pdf.PDFDocument.load(output, {updateMetadata:false})
  process.stdout.write(JSON.stringify({after:records(reopened),outputBase64:encoded,objectStreams:output.includes(Buffer.from('/ObjStm'))}))
}
async function main() {
  const doc = await pdf.PDFDocument.create({ updateMetadata: false })
  doc.addPage([300, 300])
  function add(bytes, extras = {}) {
    const dict = doc.context.obj({ Filter: name('FlateDecode'), ...extras })
    return doc.context.register(pdf.PDFRawStream.of(dict, bytes))
  }
  let seed = 0x7a031d21
  const random = n => { const b = Buffer.alloc(n); for (let i = 0; i < n; i++) { seed ^= seed << 13; seed ^= seed >>> 17; seed ^= seed << 5; b[i] = seed & 255 } return b }
  for (const size of [0, 1, 2, 3, 4, 8, 12, 16, 24, 32, 48, 64, 128, 256, 1024, 4096]) {
    for (const bytes of [Buffer.alloc(size, 65), random(size), Buffer.from(Array.from({length:size}, (_, i) => (i * 17 + (i >> 4)) & 255))]) {
      for (const level of [0, 1, 6, 9]) add(zlib.deflateSync(bytes, {level}))
    }
  }
  add(Buffer.from([1, 2, 3, 4]))
  add(zlib.deflateSync(Buffer.alloc(128, 7), {level:0}), {DecodeParms: {Predictor: 1}})
  const pixels = Buffer.alloc(2481 * 2 * 3); for (let i = 0; i < pixels.length; i++) pixels[i] = (i * 7) & 255
  const mask = add(zlib.deflateSync(Buffer.alloc(2481 * 2, 127)), {Subtype:name('Image'),Width:2481,Height:2,BitsPerComponent:8,ColorSpace:name('DeviceGray')})
  add(zlib.deflateSync(pixels), {Subtype:name('Image'),Width:2481,Height:2,BitsPerComponent:8,ColorSpace:name('DeviceRGB'),SMask:mask,Intent:name('RelativeColorimetric'),Decode:[1,0,1,0,1,0]})
  add(zlib.deflateSync(pixels), {Subtype:name('Image'),Width:2481,Height:2,BitsPerComponent:8,ColorSpace:name('DeviceRGB'),DecodeParms:{Predictor:1}})
  add(zlib.deflateSync(pixels), {Subtype:name('Image'),Width:2481,Height:2,BitsPerComponent:8,ColorSpace:[name('Indexed'),name('DeviceRGB'),255,pdf.PDFHexString.of('000000')]})
  add(zlib.deflateSync(Buffer.alloc(2481, 1)), {Subtype:name('Image'),Width:2481,Height:1,BitsPerComponent:8,ColorSpace:name('DeviceGray'),Matte:[0]})
  const sourceBytes = await doc.save({useObjectStreams:true})
  const original = await pdf.PDFDocument.load(sourceBytes, {updateMetadata:false})
  const before = records(original)
  const counts = shrinkDocument(original)
  const output = await original.save({useObjectStreams:true})
  const reopened = await pdf.PDFDocument.load(output, {updateMetadata:false})
  process.stdout.write(JSON.stringify({ node:process.versions.node, electron:process.versions.electron ?? null, zlib:process.versions.zlib, counts, before, after:records(reopened), sourceBase64:b64(sourceBytes), outputBase64:b64(output), objectStreams:Buffer.from(output).includes(Buffer.from('/ObjStm')) }))
}
(process.argv[2] === 'apply' ? apply() : main()).catch(error => { console.error(error); process.exitCode = 1 })
