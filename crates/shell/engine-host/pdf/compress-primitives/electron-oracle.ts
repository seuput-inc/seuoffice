import { deflateSync } from 'node:zlib'
import { nativeImage } from 'electron'
import { PDFArray, PDFDocument, PDFName, PDFRawStream, PDFRef, concatTransformationMatrix, drawObject, popGraphicsState, pushGraphicsState } from 'pdf-lib'
import { compressPdfBytes, LEVEL_TARGETS, samplesToBgra, sourceOf, type CompressLevel, type ImageRecoder, type SourceImage } from '../../../../../apps/pdf/src/main/compress'
import { readImageInfo } from '../../../../../apps/pdf/src/main/images-to-pdf'
import { recodeWithChromium } from '../../../../../apps/shell/src/main/pdf-tools/image-recode'
import type { CompressionCandidate } from './differential'

const base64 = (bytes: Uint8Array): string => Buffer.from(bytes).toString('base64')

function pixels(width: number, height: number, components: 1 | 3, seed: number): Uint8Array {
  const data = new Uint8Array(width * height * components)
  let state = seed >>> 0
  for (let i = 0; i < data.length; i++) {
    state ^= state << 13
    state ^= state >>> 17
    state ^= state << 5
    data[i] = state & 255
  }
  return data
}

function bitmap(width: number, height: number, transparent: boolean): Buffer {
  const rgb = pixels(width, height, 3, 0x27be18a3)
  const bgra = Buffer.alloc(width * height * 4)
  for (let i = 0; i < width * height; i++) {
    const alpha = transparent ? (i % 256) : 255
    bgra[i * 4] = Math.round(rgb[i * 3 + 2]! * alpha / 255)
    bgra[i * 4 + 1] = Math.round(rgb[i * 3 + 1]! * alpha / 255)
    bgra[i * 4 + 2] = Math.round(rgb[i * 3]! * alpha / 255)
    bgra[i * 4 + 3] = alpha
  }
  return bgra
}

function withOrientation(jpeg: Uint8Array, orientation: number): Uint8Array {
  const exif = Uint8Array.from([
    0xff, 0xe1, 0, 34, 0x45, 0x78, 0x69, 0x66, 0, 0,
    0x49, 0x49, 42, 0, 8, 0, 0, 0, 1, 0,
    0x12, 1, 3, 0, 1, 0, 0, 0, orientation, 0, 0, 0, 0, 0, 0, 0,
  ])
  const result = new Uint8Array(jpeg.length + exif.length)
  result.set(jpeg.subarray(0, 2))
  result.set(exif, 2)
  result.set(jpeg.subarray(2), 2 + exif.length)
  return result
}

/** Generated test profile, not a redistributed ICC asset. XYZ values describe
 * an approximate RGB display with D50 white; curves use a simple 2.2 gamma. */
function syntheticRgbProfile(): Uint8Array {
  const xyz = (values: readonly number[]) => {
    const out = Buffer.alloc(20)
    out.write('XYZ ', 0, 'ascii')
    values.forEach((value, index) => out.writeInt32BE(Math.round(value * 65536), 8 + index * 4))
    return out
  }
  const curve = Buffer.alloc(14)
  curve.write('curv', 0, 'ascii')
  curve.writeUInt32BE(1, 8)
  curve.writeUInt16BE(Math.round(2.2 * 256), 12)
  const name = Buffer.from('Synthetic RGB D50\0', 'ascii')
  const description = Buffer.alloc(12 + name.length + 4 + 4 + 2 + 1 + 67)
  description.write('desc', 0, 'ascii')
  description.writeUInt32BE(name.length, 8)
  name.copy(description, 12)
  const copyright = Buffer.from('text\0\0\0\0Synthetic test profile\0', 'ascii')
  const tags: Array<[string, Buffer]> = [
    ['rXYZ', xyz([0.436075, 0.222505, 0.013932])],
    ['gXYZ', xyz([0.385065, 0.716879, 0.097105])],
    ['bXYZ', xyz([0.143080, 0.060617, 0.714173])],
    ['wtpt', xyz([0.9642, 1, 0.8249])],
    ['rTRC', curve], ['gTRC', curve], ['bTRC', curve],
    ['desc', description], ['cprt', copyright],
  ]
  const tableEnd = 132 + tags.length * 12
  const offsets: number[] = []
  let length = tableEnd
  for (const [, data] of tags) {
    offsets.push(length)
    length += (data.length + 3) & ~3
  }
  const profile = Buffer.alloc(length)
  profile.writeUInt32BE(length, 0)
  profile.writeUInt32BE(0x02100000, 8)
  profile.write('mntr', 12, 'ascii')
  profile.write('RGB ', 16, 'ascii')
  profile.write('XYZ ', 20, 'ascii')
  profile.writeUInt16BE(2024, 24)
  profile.writeUInt16BE(1, 26)
  profile.writeUInt16BE(1, 28)
  profile.write('acsp', 36, 'ascii')
  profile.writeInt32BE(Math.round(0.9642 * 65536), 68)
  profile.writeInt32BE(65536, 72)
  profile.writeInt32BE(Math.round(0.8249 * 65536), 76)
  profile.writeUInt32BE(tags.length, 128)
  tags.forEach(([name, data], index) => {
    const entry = 132 + index * 12
    profile.write(name, entry, 'ascii')
    profile.writeUInt32BE(offsets[index]!, entry + 4)
    profile.writeUInt32BE(data.length, entry + 8)
    data.copy(profile, offsets[index]!)
  })
  return profile
}

/** A single deterministic document reaches both the recoder and skip paths. */
export async function compressionFixture(): Promise<Uint8Array> {
  const doc = await PDFDocument.create({ updateMetadata: false })
  const page = doc.addPage([600, 600])
  const width = 2001, height = 43
  const profile = deflateSync(syntheticRgbProfile())
  const profileRef = doc.context.register(PDFRawStream.of(doc.context.obj({ N: 3, Alternate: 'DeviceRGB', Filter: 'FlateDecode', Length: profile.length }), profile))
  for (const [index, components] of ([3, 1, 3] as const).entries()) {
    const encoded = deflateSync(pixels(width, height, components, 0x1001 + index), { level: 1 })
    const colorSpace = index === 2 ? doc.context.obj([PDFName.of('ICCBased'), profileRef]) : PDFName.of(components === 3 ? 'DeviceRGB' : 'DeviceGray')
    const dict = doc.context.obj({ Type: 'XObject', Subtype: 'Image', Width: width, Height: height,
      ColorSpace: colorSpace, BitsPerComponent: 8,
      Filter: 'FlateDecode', Length: encoded.length })
    const ref = doc.context.register(PDFRawStream.of(dict, encoded))
    const name = page.node.newXObject('Fixture', ref)
    page.pushOperators(pushGraphicsState(), concatTransformationMatrix(500, 0, 0, 30, 20, 520 - index * 40), drawObject(name), popGraphicsState())
  }
  const image = nativeImage.createFromBitmap(bitmap(257, 259, false), { width: 257, height: 259 })
  const jpeg = image.toJPEG(95)
  for (const [index, orientation] of [1, 6].entries()) {
    const embedded = await doc.embedJpg(withOrientation(jpeg, orientation))
    page.drawImage(embedded, { x: 20 + index * 270, y: 170, width: 250, height: 250 })
  }
  const transparent = nativeImage.createFromBitmap(bitmap(257, 259, true), { width: 257, height: 259 })
  const png = await doc.embedPng(transparent.toPNG())
  page.drawImage(png, { x: 20, y: 20, width: 140, height: 140 })
  const bytes = await doc.save({ useObjectStreams: false })
  const loaded = await PDFDocument.load(bytes, { updateMetadata: false })
  const images = loaded.context.enumerateIndirectObjects().map(([, value]) => value)
    .filter((value): value is PDFRawStream => value instanceof PDFRawStream && value.dict.lookup(PDFName.of('Subtype'))?.toString() === '/Image')
  const candidates = images.map(image => sourceOf(image)).filter((image): image is SourceImage => image !== null)
  const oriented = images.map(image => readImageInfo(image.getContents())?.orientation)
  const hasMask = images.some(image => image.dict.get(PDFName.of('SMask')) instanceof PDFRef)
  const hasProfile = images.some(image => image.dict.lookup(PDFName.of('ColorSpace')) instanceof PDFArray)
  if (!hasMask || !hasProfile || !oriented.includes(1) || !oriented.includes(6) ||
      candidates.filter(image => image.kind === 'raw' && image.components === 3).length < 2 ||
      !candidates.some(image => image.kind === 'raw' && image.components === 1) ||
      !candidates.some(image => image.kind === 'jpeg')) {
    throw new Error('compression fixture did not exercise RGB, gray, ICC, JPEG, EXIF and alpha paths')
  }
  return bytes
}

function resizedPixels(source: SourceImage, longSide: number): { width: number; height: number; bgraBase64: string } | null {
  const image = source.kind === 'jpeg'
    ? nativeImage.createFromBuffer(Buffer.from(source.bytes))
    : nativeImage.createFromBitmap(Buffer.from(samplesToBgra(source.samples, source.components, source.width * source.height)), { width: source.width, height: source.height })
  if (image.isEmpty()) return null
  const size = image.getSize()
  if (size.width !== source.width || size.height !== source.height) return null
  const scale = Math.min(1, longSide / Math.max(size.width, size.height))
  const fitted = scale < 1 ? image.resize({ width: Math.max(1, Math.round(size.width * scale)), height: Math.max(1, Math.round(size.height * scale)), quality: 'good' }) : image
  const output = fitted.getSize()
  return { ...output, bgraBase64: base64(fitted.toBitmap()) }
}

/** Run in Electron main, never in the CEF dependency service. */
export async function compressionElectronOracle(source: Uint8Array, level: CompressLevel) {
  if (process.versions.electron !== '43.8.3') throw new Error(`Expected Electron 43.8.3, got ${process.versions.electron ?? 'none'}`)
  const candidates: CompressionCandidate[] = []
  const progress: Array<[number, number]> = []
  const recode: ImageRecoder = async (image, target) => {
    const input = image.kind === 'jpeg' ? image.bytes : image.samples
    const before = resizedPixels(image, target.longSide)
    const result = await recodeWithChromium(image, target)
    const decoded = result && nativeImage.createFromBuffer(Buffer.from(result.jpeg))
    candidates.push({ kind: image.kind, width: image.width, height: image.height,
      components: image.kind === 'raw' ? image.components : null, dataBase64: base64(input), target, before,
      jpegBase64: result ? base64(result.jpeg) : null, outputWidth: result?.width ?? null, outputHeight: result?.height ?? null,
      decodedBgraBase64: decoded && !decoded.isEmpty() ? base64(decoded.toBitmap()) : null })
    return result
  }
  const result = await compressPdfBytes(source, level, recode, { onProgress: (done, total) => progress.push([done, total]) })
  return {
    runtime: { engine: 'Electron', version: process.versions.electron, chromium: process.versions.chrome ?? 'unknown', platform: process.platform, arch: process.arch },
    target: LEVEL_TARGETS[level], sourceBase64: base64(source), candidates, progress, recoded: result.recoded,
    outcome: result.bytes.length >= source.length ? 'not-smaller' as const : 'written' as const,
    outputBase64: result.bytes.length >= source.length ? null : base64(result.bytes),
    sourceLength: source.length, outputLength: result.bytes.length, rendered: null,
  }
}
