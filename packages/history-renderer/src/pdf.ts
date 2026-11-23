import { HistoryError } from '@seuoffice/collab/client'
import { degrees, ParseSpeeds, PDFDocument, PDFObjectCopier } from 'pdf-lib'
import { diff, emptySnapshot, foldPeer, type PdfOp } from '../../../apps/pdf/src/web/collab-model'
import { decodeOp } from '../../../apps/pdf/src/web/collab-wire'
import type { ByteHistoryAdapter } from './adapter'
import { classicPdf, profile, readPdfEnvelope } from './pdf-envelope'
import { checkPages } from './pdf-pages'

export async function openPdf(base: Uint8Array): Promise<ByteHistoryAdapter<PdfOp>> {
  const envelope = await readPdfEnvelope(base)
  const count = checkPages(envelope)
  const doc = await PDFDocument.load(classicPdf(envelope), {
    updateMetadata: false,
    throwOnInvalidObject: true,
    parseSpeed: ParseSpeeds.Fastest,
  })
  profile(count === doc.getPageCount())
  let confirmed = emptySnapshot()
  return {
    decode(json) {
      const op = decodeOp(json)
      for (const p of op.puts) {
        if (
          p.b !== 'rotations' &&
          p.b !== 'metadata' &&
          p.b !== 'blankPages' &&
          p.b !== 'order' &&
          p.b !== 'deleted'
        )
          throw new HistoryError('unsupported', `PDF edit not supported: ${p.b}`)
        if (p.b === 'metadata' && p.v) {
          // The Info profile admits hex strings of at most 4096 digits, including the UTF-16 BOM.
          profile(Object.values(p.v).every((s) => typeof s === 'string' && s.length <= 1023))
        }
      }
      return op
    },
    encode: ({ id, puts }) => ({ id, puts }),
    integrate(remote, local) {
      if (local.length) throw new HistoryError('invalid_history')
      for (const op of remote) {
        const added = op.puts.filter((p) => p.b === 'blankPages').length
        profile(count + confirmed.blankPages.length + added <= 128)
        confirmed = foldPeer(confirmed, op.puts, count)
      }
      return []
    },
    captureUndo(op) {
      const pages = new Set<number>()
      let metadata = false
      for (const p of op.puts) {
        if (p.b === 'rotations') pages.add(p.k)
        else if (p.b === 'metadata') metadata = true
        else throw new HistoryError('unsupported', 'this PDF edit has no selective inverse')
      }
      const inverse = diff(foldPeer(confirmed, op.puts, count), confirmed, count)
      if (!inverse.length) throw new HistoryError('unsupported', 'PDF edit did not change state')
      return (later) =>
        later.some((change) =>
          change.puts.some(
            (p) =>
              (p.b === 'metadata' && metadata) ||
              ((p.b === 'rotations' || p.b === 'deleted') && pages.has(p.k)),
          ),
        )
          ? null
          : [{ id: crypto.randomUUID(), puts: inverse }]
    },
    async produceFile() {
      const { rotations, metadata, blankPages, deleted, order } = confirmed
      if (!rotations.size && !metadata && !blankPages.length && !deleted.size && !order)
        return { fileName: 'history.pdf', bytes: base }
      for (const { input } of blankPages) doc.addPage([input.width, input.height])
      const pages = doc.getPages()
      for (const [k, delta] of rotations) {
        const page = pages[k]!
        page.setRotation(degrees((page.getRotation().angle + delta) % 360))
      }
      // As in the PDF editor, an omitted field keeps the base's value. Keep its ModDate too:
      // replaying a retained prefix has no new edit time to put in the file.
      if (metadata?.title !== undefined) doc.setTitle(metadata.title)
      if (metadata?.subject !== undefined) doc.setSubject(metadata.subject)
      if (metadata?.author !== undefined) doc.setAuthor(metadata.author)
      if (metadata?.keywords !== undefined)
        doc.setKeywords(
          metadata.keywords
            .split(/[,，;；]/)
            .map((word) => word.trim())
            .filter(Boolean),
        )
      let result = doc
      if (deleted.size) {
        // removePage leaves the old page and its streams in the file. Copy only the visible graph;
        // the admitted catalog has no document-level objects other than Pages and the trailer's Info.
        result = await PDFDocument.create({ updateMetadata: false })
        const info = doc.context.trailerInfo.Info
        if (info)
          result.context.trailerInfo.Info = PDFObjectCopier.for(doc.context, result.context).copy(
            info,
          )
        const visible = (order ?? pages.map((_, i) => i)).filter((i) => !deleted.has(i))
        for (const page of await result.copyPages(doc, visible)) result.addPage(page)
      } else if (order) {
        const kids = doc.catalog.Pages().Kids()
        order.forEach((k, i) => kids.set(i, pages[k]!.ref))
      }
      const bytes = await result.save({
        useObjectStreams: false,
        updateFieldAppearances: false,
        objectsPerTick: Infinity,
      })
      checkPages(await readPdfEnvelope(bytes))
      return { fileName: 'history.pdf', bytes }
    },
  }
}
