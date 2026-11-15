import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { createHash } from 'node:crypto'
import { writeFileSync } from 'node:fs'
import { join } from 'node:path'
import { deflateSync } from 'node:zlib'
import tls from 'node:tls'
import forge from 'node-forge'
import {
  PDFArray,
  PDFDict,
  PDFDocument,
  PDFHexString,
  PDFName,
  PDFNumber,
  PDFRef,
  PDFString,
} from 'pdf-lib'
import { PNG } from 'pngjs'
import {
  signPdf,
  type LoadedCertificate,
  type SignPdfOptions,
} from '../../apps/pdf/src/main/pdf-sign'
import { collectSignatureFields, readPdfSignatures } from '../../apps/pdf/src/main/pdf-signatures'

// Keep the real verifier, but forbid its optional OS-store lookup in this
// isolated synthetic-key run. Trust-store parity is not part of this proof.
Object.defineProperty(tls, 'getCACertificates', {
  configurable: true,
  value: (kind: string) => {
    if (kind !== 'system') throw new Error('unexpected certificate store access')
    return []
  },
})

const executable = process.argv[2]
if (!executable) throw new Error('native codec executable required')
const hex = (bytes: Uint8Array) => Buffer.from(bytes).toString('hex')
type SignatureField = {
  name: string
  page: number
  range: number[] | null
  reason: string | null
  location: string | null
  contact: string | null
  certified: number | null
}
type NativeReply = { ok: boolean; value?: unknown; error?: string }
function native(request: object): NativeReply {
  const result = spawnSync(executable, [], {
    input: JSON.stringify(request) + '\n',
    encoding: 'utf8',
    maxBuffer: 64 * 1024 * 1024,
    timeout: 30_000,
    killSignal: 'SIGKILL',
  })
  if (result.error || result.status !== 0)
    throw new Error(`native runner failed: ${result.error ?? result.stderr}`)
  const lines = result.stdout.trim().split('\n')
  assert.equal(lines.length, 1, 'native runner must return exactly one response')
  const parsed: unknown = JSON.parse(lines[0]!)
  if (
    typeof parsed !== 'object' ||
    parsed === null ||
    !('ok' in parsed) ||
    typeof parsed.ok !== 'boolean'
  )
    throw new Error('invalid native response')
  if ('error' in parsed && typeof parsed.error !== 'string') throw new Error('invalid native error')
  return {
    ok: parsed.ok,
    value: 'value' in parsed ? parsed.value : undefined,
    error: 'error' in parsed ? parsed.error : undefined,
  }
}
function value(request: object): unknown {
  const response = native(request)
  assert.equal(response.ok, true, `native operation rejected: ${response.error}`)
  return response.value
}
const text = (dict: PDFDict, key: string) => {
  const entry = dict.lookup(PDFName.of(key))
  return entry instanceof PDFString || entry instanceof PDFHexString ? entry.decodeText() : null
}
const certLevel = (dict: PDFDict): number | null => {
  const refs = dict.lookupMaybe(PDFName.of('Reference'), PDFArray)
  if (!refs) return null
  for (const entry of refs.asArray()) {
    const ref = dict.context.lookupMaybe(entry, PDFDict)
    const method = ref?.lookup(PDFName.of('TransformMethod'))
    if (!(method instanceof PDFName) || method.decodeText() !== 'DocMDP') continue
    const params = ref?.lookupMaybe(PDFName.of('TransformParams'), PDFDict)
    const level = params?.lookup(PDFName.of('P'))
    const number = level instanceof PDFNumber ? level.asNumber() : 2
    return number === 1 || number === 3 ? number : 2
  }
  return null
}
async function reference(bytes: Uint8Array): Promise<{ fields: SignatureField[] }> {
  let pdf: PDFDocument
  try {
    pdf = await PDFDocument.load(bytes, { updateMetadata: false, throwOnInvalidObject: false })
  } catch {
    return { fields: [] }
  }
  const pages = pdf.getPages()
  return {
    fields: collectSignatureFields(pdf).map((field) => {
      const direct = field.widget.get(PDFName.of('P'))
      let page = direct instanceof PDFRef ? pages.findIndex((item) => item.ref === direct) : -1
      if (page < 0 && field.widgetRef)
        page = pages.findIndex((item) => {
          const annots = item.node.lookupMaybe(PDFName.of('Annots'), PDFArray)
          return annots?.asArray().some((entry) => entry === field.widgetRef)
        })
      const raw = field.value.lookup(PDFName.of('ByteRange'))
      const numbers =
        raw instanceof PDFArray
          ? raw.asArray().map((item) => (item instanceof PDFNumber ? item.asNumber() : NaN))
          : []
      const range =
        numbers.length === 4 && numbers.every((n) => Number.isInteger(n) && n >= 0) ? numbers : null
      return {
        name: field.name,
        page,
        range,
        reason: text(field.value, 'Reason'),
        location: text(field.value, 'Location'),
        contact: text(field.value, 'ContactInfo'),
        certified: certLevel(field.value),
      }
    }),
  }
}
async function compare(bytes: Uint8Array, label: string) {
  const expected = await reference(bytes)
  const actual = value({ op: 'inspect', pdf: hex(bytes) })
  assert.deepEqual(
    actual,
    expected,
    `${label}: native signature-field consumer differs from pdf-lib`,
  )
  return expected
}
function classic(badObject: boolean) {
  const chunks: Buffer[] = [Buffer.from('%PDF-1.7\n')]
  const offsets = [0]
  const append = (s: string) => chunks.push(Buffer.from(s, 'latin1'))
  const length = () => chunks.reduce((sum, b) => sum + b.length, 0)
  const bodies = [
    '<< /Type /Catalog /Pages 2 0 R >>',
    '<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 300 200] >>',
    '<< /Type /Page /Parent 2 0 R >>',
    badObject ? 'malformed-object' : '<< /Unrelated (preserved) >>',
  ]
  bodies.forEach((body, i) => {
    offsets.push(length())
    append(`${i + 1} 0 obj\n${body}\nendobj\n`)
  })
  const xref = length()
  append('xref\n0 5\n0000000000 65535 f \n')
  offsets.slice(1).forEach((offset) => append(`${String(offset).padStart(10, '0')} 00000 n \n`))
  append(`trailer\n<< /Size 5 /Root 1 0 R >>\nstartxref\n${xref}\n%%EOF\n`)
  return Buffer.concat(chunks)
}
function objectStream() {
  const head = Buffer.from(
    '%PDF-1.7\n1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj\n2 0 obj << /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 300 200] >> endobj\n',
  )
  const object = Buffer.from('3 0 << /Type /Page /Parent 2 0 R >>')
  const compressed = deflateSync(object)
  const stream = Buffer.concat([
    Buffer.from(
      `4 0 obj << /Type /ObjStm /N 1 /First 4 /Filter /FlateDecode /Length ${compressed.length} >> stream\n`,
    ),
    compressed,
    Buffer.from('\nendstream endobj\n'),
  ])
  const xref = head.length + stream.length
  return Buffer.concat([
    head,
    stream,
    Buffer.from(
      `xref\n0 1\n0000000000 65535 f \ntrailer << /Size 5 /Root 1 0 R >>\nstartxref\n${xref}\n%%EOF\n`,
    ),
  ])
}
function makeCertificate() {
  // Generated in memory for this run; no system key store or user key is read.
  const keys = forge.pki.rsa.generateKeyPair({ bits: 1024, e: 0x10001 })
  const certificate = forge.pki.createCertificate()
  certificate.publicKey = keys.publicKey
  certificate.serialNumber = '01'
  certificate.validity.notBefore = new Date('2020-01-01T00:00:00Z')
  certificate.validity.notAfter = new Date('2030-01-01T00:00:00Z')
  certificate.setSubject([{ name: 'commonName', value: 'Synthetic PDF proof' }])
  certificate.setIssuer(certificate.subject.attributes)
  certificate.sign(keys.privateKey, forge.md.sha256.create())
  return { privateKey: keys.privateKey, certificate, chain: [certificate] }
}

function signSyntheticRevision(
  file: Buffer,
  range: number[],
  loaded: LoadedCertificate,
  date: Date,
): Buffer {
  const [start, length, second, tail] = range
  assert.equal(start, 0)
  const content = Buffer.concat([
    file.subarray(start, start + length),
    file.subarray(second, second + tail),
  ])
  const p7 = forge.pkcs7.createSignedData()
  p7.content = forge.util.createBuffer(content.toString('binary'))
  p7.addCertificate(loaded.certificate)
  p7.addSigner({
    key: loaded.privateKey,
    certificate: loaded.certificate,
    digestAlgorithm: forge.pki.oids.sha256,
    authenticatedAttributes: [
      { type: forge.pki.oids.contentType, value: forge.pki.oids.data },
      { type: forge.pki.oids.messageDigest },
      // forge accepts Date here; its published type incorrectly requires string.
      { type: forge.pki.oids.signingTime, value: date as unknown as string },
    ],
  })
  p7.sign({ detached: true })
  const cms = Buffer.from(forge.asn1.toDer(p7.toAsn1()).getBytes(), 'binary')
    .toString('hex')
    .toUpperCase()
  assert(cms.length <= second - length - 2, 'synthetic CMS exceeds native placeholder')
  const signed = Buffer.from(file)
  signed.write(cms, length + 1, 'ascii')
  return signed
}

async function main() {
  const loaded = makeCertificate()
  const date = new Date('2025-01-01T00:00:00Z')
  const time = 'D:20250101000000Z'
  const streamDocument = await PDFDocument.create()
  streamDocument.addPage([300, 200])
  const streamPdf = await streamDocument.save({ useObjectStreams: true })
  const image = new PNG({ width: 2, height: 1 })
  image.data.set([255, 0, 0, 255, 0, 0, 255, 100])
  const png = PNG.sync.write(image)
  type ProofOptions = Pick<
    SignPdfOptions,
    'reason' | 'location' | 'contactInfo' | 'certifyLevel' | 'visible'
  >
  const fixtures: { name: string; bytes: Uint8Array; options: ProofOptions }[] = [
    { name: 'classic', bytes: classic(false), options: {} },
    {
      name: 'unused-malformed-object',
      bytes: classic(true),
      options: {
        reason: 'approval (한글)',
        location: 'Seoul',
        contactInfo: 'test@example.invalid',
      },
    },
    { name: 'object-stream', bytes: objectStream(), options: { certifyLevel: 2 } },
    {
      name: 'xref-stream-visible-alpha',
      bytes: streamPdf,
      options: { visible: { pageIndex: 0, position: 'bottom-right', width: 80, height: 40, png } },
    },
    { name: 'certification-level-1', bytes: classic(false), options: { certifyLevel: 1 } },
    { name: 'certification-level-3', bytes: classic(false), options: { certifyLevel: 3 } },
  ]
  let assertions = 0
  const results: {
    name: string
    inputSha256: string
    nativeRevisionSha256: string
    nativeSignedSha256: string
    originalSignedSha256: string
  }[] = []
  const digest = (bytes: Uint8Array) => createHash('sha256').update(bytes).digest('hex')
  for (const fixture of fixtures) {
    await compare(fixture.bytes, `${fixture.name} original`)
    const options = fixture.options.visible
      ? {
          ...fixture.options,
          visible: { ...fixture.options.visible, png: hex(fixture.options.visible.png) },
        }
      : fixture.options
    const request = { pdf: hex(fixture.bytes), options, time }
    const assembledValue = value({ op: 'assemble', ...request, placeholder: 16 * 1024 })
    if (
      typeof assembledValue !== 'object' ||
      assembledValue === null ||
      !('pdf' in assembledValue) ||
      typeof assembledValue.pdf !== 'string' ||
      !('range' in assembledValue) ||
      !Array.isArray(assembledValue.range) ||
      assembledValue.range.length !== 4
    )
      throw new Error('invalid native assembly response')
    const range = assembledValue.range.map((part: unknown) => {
      if (typeof part !== 'number' || !Number.isSafeInteger(part) || part < 0)
        throw new Error('invalid native ByteRange')
      return part
    })
    const assembled = { pdf: assembledValue.pdf, range }
    const nativePdf = Buffer.from(assembled.pdf, 'hex')
    assert(
      nativePdf.subarray(0, fixture.bytes.length).equals(fixture.bytes),
      `${fixture.name}: original prefix changed`,
    )
    assert.deepEqual(
      value({ op: 'validate', ...request, revised: assembled.pdf, range: assembled.range }),
      { valid: true },
    )
    const observed = await compare(nativePdf, `${fixture.name} native revision`)
    assert.equal(
      observed.fields.length,
      1,
      `${fixture.name}: original consumer must see native field`,
    )
    assert.deepEqual(observed.fields[0].range, assembled.range)
    const placeholderInfo = await readPdfSignatures(nativePdf)
    assert.equal(placeholderInfo.length, 1)
    assert(
      placeholderInfo[0]!.problems.includes('malformed'),
      'zero-filled placeholder is not a real signature',
    )
    const signedNative = signSyntheticRevision(nativePdf, assembled.range, loaded, date)
    await compare(signedNative, `${fixture.name} signed native revision`)
    const nativeInfo = await readPdfSignatures(signedNative)
    assert.equal(nativeInfo.length, 1)
    assert(
      !nativeInfo[0]!.problems.some((problem) =>
        [
          'malformed',
          'digest-mismatch',
          'signature-invalid',
          'signer-certificate-missing',
        ].includes(problem),
      ),
      `${fixture.name}: original consumer must verify synthetic signature over native bytes`,
    )

    const signed = Buffer.from(
      await signPdf(fixture.bytes, { ...fixture.options, loaded, signingTime: date }),
    )
    const signedFields = await compare(signed, `${fixture.name} pdf-lib signed revision`)
    assert.equal(signedFields.fields.length, 1)
    const info = await readPdfSignatures(signed)
    assert.equal(info.length, 1)
    assert(
      !info[0]!.problems.some((problem) =>
        ['malformed', 'digest-mismatch', 'signature-invalid'].includes(problem),
      ),
      `${fixture.name}: synthetic signature must verify`,
    )
    const field = signedFields.fields[0]!
    for (const key of ['name', 'page', 'reason', 'location', 'contact', 'certified'] as const)
      assert.deepEqual(observed.fields[0]![key], field[key], `${fixture.name}: ${key}`)
    assert(field.range)
    const [zero, first, second, tail] = field.range
    assert.equal(zero, 0)
    assert.equal(second + tail, signed.length)
    const unsigned = Buffer.from(signed)
    unsigned.fill(0x30, first + 1, second - 1)
    assert.deepEqual(
      value({ op: 'validate', ...request, revised: hex(unsigned), range: field.range }),
      { valid: true },
      `${fixture.name}: original assembly must be accepted`,
    )
    const wrongRange = [...assembled.range]
    wrongRange[1]++
    assert.equal(
      native({ op: 'validate', ...request, revised: assembled.pdf, range: wrongRange }).ok,
      false,
      `${fixture.name}: ByteRange mutation accepted`,
    )
    const wrongPrefix = Buffer.from(nativePdf)
    wrongPrefix[0] ^= 1
    assert.equal(
      native({ op: 'validate', ...request, revised: hex(wrongPrefix), range: assembled.range }).ok,
      false,
      `${fixture.name}: original content mutation accepted`,
    )
    const page = nativePdf.indexOf(Buffer.from('/Type /Page'), fixture.bytes.length)
    assert(page >= 0, `${fixture.name}: expected changed page`)
    const wrongPage = Buffer.from(nativePdf)
    wrongPage[page + '/Type /Pag'.length] = 0x7a
    assert.equal(
      native({ op: 'validate', ...request, revised: hex(wrongPage), range: assembled.range }).ok,
      false,
      `${fixture.name}: unauthorized page change accepted`,
    )
    const malformedRange = Buffer.from(signed)
    const rangeAt = malformedRange.indexOf(Buffer.from('/ByteRange ['), fixture.bytes.length)
    assert(rangeAt >= 0)
    const digitAt = rangeAt + '/ByteRange [0 '.length
    malformedRange[digitAt] = 0x2d
    await compare(malformedRange, `${fixture.name} malformed range`)
    const malformedInfo = await readPdfSignatures(malformedRange)
    assert.equal(malformedInfo.length, 1)
    assert(malformedInfo[0]!.problems.includes('malformed'))
    results.push({
      name: fixture.name,
      inputSha256: digest(fixture.bytes),
      nativeRevisionSha256: digest(nativePdf),
      nativeSignedSha256: digest(signedNative),
      originalSignedSha256: digest(signed),
    })
    assertions++
    console.log(
      `PASS ${fixture.name}: original consumer, native assembly, signatures, ranges, forbidden edits`,
    )
  }
  assert.equal(assertions, fixtures.length)

  const prior = Buffer.from(await signPdf(classic(false), { loaded, signingTime: date }))
  const later = value({
    op: 'assemble',
    pdf: hex(prior),
    options: {},
    time,
    placeholder: 16 * 1024,
  })
  if (
    typeof later !== 'object' ||
    later === null ||
    !('pdf' in later) ||
    typeof later.pdf !== 'string'
  )
    throw new Error('invalid second revision')
  const laterPdf = Buffer.from(later.pdf, 'hex')
  assert(laterPdf.subarray(0, prior.length).equals(prior), 'existing signed revision changed')
  const laterFields = await compare(laterPdf, 'previously signed incremental revision')
  assert.equal(laterFields.fields.length, 2)
  const previousInfo = await readPdfSignatures(laterPdf)
  assert.equal(previousInfo.length, 2)
  assert(previousInfo[0]!.problems.includes('document-changed-after-signing'))
  assert(previousInfo[1]!.problems.includes('malformed'))
  console.log('PASS previously signed incremental revision')
  const scratch = process.env.PDF_PROOF_SCRATCH
  if (!scratch) throw new Error('PDF_PROOF_SCRATCH required for evidence')
  writeFileSync(
    join(scratch, 'differential-evidence.json'),
    JSON.stringify(
      {
        proof: 'native-codec-vs-original-pdf-lib-consumer',
        syntheticKeysOnly: true,
        systemCertificateStore: 'excluded',
        trustParityClaimed: false,
        fixtures: results,
        previousRevision: { inputSha256: digest(prior), nativeRevisionSha256: digest(laterPdf) },
      },
      null,
      2,
    ) + '\n',
  )
}

main().catch((error) => {
  console.error(error)
  process.exitCode = 1
})
