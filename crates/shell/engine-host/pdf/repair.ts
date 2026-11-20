import { PDFArray, PDFDocument, PDFName, PDFRef } from 'pdf-lib'
import {
  repairPdfBytes,
  RepairFailedError,
  type RepairPdfium,
} from '../../../../apps/pdf/src/main/repair'
import { withPdfium } from './pdfium-text'

async function salvage(bytes: Uint8Array): Promise<Uint8Array | null> {
  const marker = new TextEncoder().encode('endobj')
  let end = -1
  for (let i = bytes.length - marker.length; i >= 0; i--) {
    if (marker.every((byte, offset) => bytes[i + offset] === byte)) {
      end = i + marker.length
      break
    }
  }
  const read = async (input: Uint8Array) => {
    try {
      return await PDFDocument.load(input, { updateMetadata: false })
    } catch {
      return null
    }
  }
  const source = (await read(bytes)) ?? (end < 0 ? null : await read(bytes.subarray(0, end)))
  if (!source) return null
  let indices: number[]
  try {
    indices = source.getPageIndices()
  } catch {
    return null
  }
  const out = await PDFDocument.create({ updateMetadata: false })
  let pages = 0
  for (const index of indices) {
    try {
      const contents = source.getPage(index).node.get(PDFName.of('Contents'))
      const resolved = contents instanceof PDFRef ? source.context.lookup(contents) : contents
      if (contents && !resolved) continue
      if (
        resolved instanceof PDFArray &&
        !resolved
          .asArray()
          .every((item) => !(item instanceof PDFRef) || !!source.context.lookup(item))
      )
        continue
      const [page] = await out.copyPages(source, [index])
      out.addPage(page!)
      pages++
    } catch {
      /* a page with missing objects cannot be salvaged */
    }
  }
  return pages === 0 ? null : out.save()
}

export async function repairPdf(bytes: Uint8Array): Promise<Uint8Array> {
  try {
    return await withPdfium((module) => repairPdfBytes(module as RepairPdfium, bytes).bytes)
  } catch (error) {
    if (!(error instanceof RepairFailedError) || error.reason !== 'unreadable') throw error
    const recovered = await salvage(bytes)
    if (!recovered) throw error
    return recovered
  }
}
