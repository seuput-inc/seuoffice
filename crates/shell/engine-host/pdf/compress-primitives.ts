import {
  PDFArray,
  PDFDict,
  PDFDocument,
  PDFName,
  PDFNumber,
  PDFRawStream,
  PDFRef,
  decodePDFRawStream,
} from 'pdf-lib'

const decode = (value: string): Uint8Array =>
  Uint8Array.from(atob(value), (char) => char.charCodeAt(0))
const encode = (bytes: Uint8Array): string => {
  let out = ''
  for (let at = 0; at < bytes.length; at += 8192)
    out += String.fromCharCode(...bytes.subarray(at, at + 8192))
  return btoa(out)
}
const key = (name: string) => PDFName.of(name)
const n = (dict: PDFDict, name: string): number | null => {
  const value = dict.lookup(key(name))
  return value instanceof PDFNumber ? value.asNumber() : null
}
const singleName = (value: unknown): string | null =>
  value instanceof PDFName ? value.decodeText() : null

export async function enumeratePdfStreams(bytesBase64: string): Promise<unknown> {
  const doc = await PDFDocument.load(decode(bytesBase64), { updateMetadata: false })
  const objects = doc.context.enumerateIndirectObjects()
  const masks = new Set<string>()
  for (const [, object] of objects) {
    const mask = object instanceof PDFRawStream ? object.dict.get(key('SMask')) : undefined
    if (mask instanceof PDFRef) masks.add(mask.toString())
  }
  const streams: unknown[] = []
  for (const [ref, object] of objects) {
    if (!(object instanceof PDFRawStream)) continue
    try {
      const dict = object.dict
      const filterValue = dict.lookup(key('Filter'))
      const filter =
        filterValue instanceof PDFArray
          ? filterValue.size() === 0
            ? ''
            : filterValue.size() === 1
              ? singleName(filterValue.lookup(0))
              : null
          : filterValue
            ? singleName(filterValue)
            : ''
      const color = dict.lookup(key('ColorSpace'))
      let components: number | null = null
      let icc = false
      if (color instanceof PDFName) {
        if (color.decodeText() === 'DeviceRGB') components = 3
        if (color.decodeText() === 'DeviceGray') components = 1
      } else if (
        color instanceof PDFArray &&
        color.size() === 2 &&
        singleName(color.lookup(0)) === 'ICCBased'
      ) {
        const profile = color.lookup(1)
        if (profile instanceof PDFRawStream) {
          components = n(profile.dict, 'N')
          icc = true
        }
      }
      const parms = dict.lookup(key('DecodeParms'))
      const predictor =
        parms instanceof PDFDict
          ? n(parms, 'Predictor')
          : parms instanceof PDFArray
            ? n(parms.lookupMaybe(0, PDFDict) ?? doc.context.obj({}), 'Predictor')
            : null
      streams.push({
        ref: ref.toString(),
        subtype: singleName(dict.lookup(key('Subtype'))),
        mask: masks.has(ref.toString()),
        imageMask: dict.lookup(key('ImageMask'))?.toString() === 'true',
        hasMask: dict.has(key('Mask')),
        hasDecode: dict.has(key('Decode')),
        bits: n(dict, 'BitsPerComponent'),
        width: n(dict, 'Width'),
        height: n(dict, 'Height'),
        size: object.getContentsSize(),
        components,
        icc,
        filter,
        predictor,
      })
    } catch {
      // A damaged image is not a compression candidate.
    }
  }
  return streams
}

export async function readPdfStream(
  bytesBase64: string,
  wanted: string,
  inflated: boolean,
): Promise<string | null> {
  const doc = await PDFDocument.load(decode(bytesBase64), { updateMetadata: false })
  const entry = doc.context.enumerateIndirectObjects().find(([ref]) => ref.toString() === wanted)
  if (!entry || !(entry[1] instanceof PDFRawStream)) throw new Error('PDF stream is missing')
  try {
    return encode(inflated ? decodePDFRawStream(entry[1]).decode() : entry[1].getContents())
  } catch {
    return null
  }
}

interface Replacement {
  ref: string
  jpegBase64: string
  width: number
  height: number
}
const DROPPED = new Set([
  'Length',
  'Filter',
  'DecodeParms',
  'Width',
  'Height',
  'BitsPerComponent',
  'ColorSpace',
])

export async function replacePdfStreams(
  bytesBase64: string,
  replacements: Replacement[],
): Promise<string> {
  const doc = await PDFDocument.load(decode(bytesBase64), { updateMetadata: false })
  const wanted = new Map(replacements.map((value) => [value.ref, value]))
  for (const [ref, stream] of doc.context.enumerateIndirectObjects()) {
    const replacement = wanted.get(ref.toString())
    if (!replacement || !(stream instanceof PDFRawStream)) continue
    const oldColor = stream.dict.lookup(key('ColorSpace'))
    const profile =
      oldColor instanceof PDFArray &&
      oldColor.size() === 2 &&
      singleName(oldColor.lookup(0)) === 'ICCBased'
        ? oldColor.lookup(1)
        : undefined
    const keep =
      profile instanceof PDFRawStream && n(profile.dict, 'N') === 3
        ? stream.dict.get(key('ColorSpace'))
        : undefined
    const dict = doc.context.obj({})
    for (const [name, value] of stream.dict.entries())
      if (!DROPPED.has(name.decodeText())) dict.set(name, value)
    dict.set(key('Width'), PDFNumber.of(replacement.width))
    dict.set(key('Height'), PDFNumber.of(replacement.height))
    dict.set(key('BitsPerComponent'), PDFNumber.of(8))
    dict.set(key('ColorSpace'), keep ?? key('DeviceRGB'))
    dict.set(key('Filter'), key('DCTDecode'))
    doc.context.assign(ref, PDFRawStream.of(dict, decode(replacement.jpegBase64)))
    wanted.delete(ref.toString())
  }
  if (wanted.size) throw new Error('PDF stream replacement target is missing')
  return encode(await doc.save({ useObjectStreams: true }))
}
