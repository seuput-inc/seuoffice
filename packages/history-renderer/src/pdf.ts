import { HistoryError } from '@seuoffice/collab/client'
import { degrees, ParseSpeeds, PDFDict, PDFDocument, PDFName, PDFObjectCopier } from 'pdf-lib'
import { capturePdfInverse } from '../../../apps/pdf/src/web/collab-adapter'
import {
  effectiveOrder,
  emptySnapshot,
  foldPeer,
  type PdfOp,
} from '../../../apps/pdf/src/web/collab-model'
import { decodeOp } from '../../../apps/pdf/src/web/collab-wire'
import type { ByteHistoryAdapter } from './adapter'
import { classicPdf, profile, readPdfEnvelope } from './pdf-envelope'
import { writeMarkups } from './pdf-markups'
import { checkPages } from './pdf-pages'
import { writeStamps } from './pdf-stamps'

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
          p.b !== 'stampCfg' &&
          p.b !== 'markups' &&
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
      const captured = capturePdfInverse(confirmed, op, count)
      return (later) => {
        const inverse = captured(later)
        return inverse && [{ id: crypto.randomUUID(), puts: inverse }]
      }
    },
    async produceFile() {
      const { rotations, metadata, blankPages, deleted, order, markups, stampCfg } = confirmed
      if (
        !rotations.size &&
        !metadata &&
        !blankPages.length &&
        !deleted.size &&
        !order &&
        !markups.length &&
        !stampCfg
      )
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
      const visible = effectiveOrder(confirmed, count).filter((i) => !deleted.has(i))
      if (markups.length) writeMarkups(doc, markups, visible)
      if (stampCfg) await writeStamps(doc, stampCfg, visible)
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
        // A markup's optional /P points back to its page. Copying that cycle would also copy
        // a second, hidden page. Rebind only the back-references the original actually had.
        const parent = PDFName.of('P')
        const parents = visible.map((i) =>
          (pages[i]!.node.Annots()?.asArray() ?? []).map((ref) => {
            const annot = doc.context.lookup(ref, PDFDict)
            const hasParent = annot.has(parent)
            if (hasParent) annot.delete(parent)
            return hasParent
          }),
        )
        for (const [i, page] of (await result.copyPages(doc, visible)).entries()) {
          result.addPage(page)
          const annots = page.node.Annots()
          parents[i]!.forEach((hasParent, j) => {
            if (hasParent) annots!.lookup(j, PDFDict).set(parent, page.ref)
          })
        }
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
