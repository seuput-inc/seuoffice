// Differential oracle for the PDFium CFF repair in apps/pdf/src/main/text-edit.ts.
import { PDFDict, PDFDocument, PDFName, PDFRawStream, decodePDFRawStream } from 'pdf-lib'

export async function relabelOpenTypeOracle(bytes: Uint8Array): Promise<Uint8Array> {
  const doc = await PDFDocument.load(bytes)
  let touched = false
  for (const [, object] of doc.context.enumerateIndirectObjects()) {
    if (!(object instanceof PDFDict)) continue
    if (object.get(PDFName.of('Type')) !== PDFName.of('FontDescriptor')) continue
    const ref = object.get(PDFName.of('FontFile'))
    if (!ref) continue
    const stream = doc.context.lookup(ref)
    if (!(stream instanceof PDFRawStream)) continue
    let program: Uint8Array
    try {
      program = decodePDFRawStream(stream).decode()
    } catch {
      continue
    }
    if (program[0] !== 0x4f || program[1] !== 0x54 || program[2] !== 0x54 || program[3] !== 0x4f)
      continue
    object.delete(PDFName.of('FontFile'))
    object.set(PDFName.of('FontFile3'), ref)
    stream.dict.set(PDFName.of('Subtype'), PDFName.of('OpenType'))
    touched = true
  }
  return touched ? doc.save({ useObjectStreams: false }) : bytes
}
