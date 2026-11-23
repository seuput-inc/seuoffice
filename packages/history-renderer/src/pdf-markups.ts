import type { PDFDocument } from 'pdf-lib'
import { addMarkup, DEFAULT_ANNOT_AUTHOR, quadBounds } from '../../../apps/pdf/src/main/pdf-markup'
import type { EditSnapshot as PdfSnapshot } from '../../../apps/pdf/src/renderer/edit-state'
import { profile } from './pdf-envelope'

const decimal = /^-?\d{1,5}(?:\.\d{1,24})?$/
const maxAppearance = 2048

/** The browser wire has checked the item shape. Bound the PDF geometry before expanding a wave. */
export function writeMarkups(
  doc: PDFDocument,
  markups: PdfSnapshot['markups'],
  visible: readonly number[],
): void {
  profile(markups.length <= 128)
  for (const m of markups) {
    profile(visible.includes(m.pageIndex) && m.quads.length <= 16)
    profile(m.color.every((v) => decimal.test(String(v))))
    profile(!m.subject || !/[\uD800-\uDFFF]/u.test(m.subject))
    const page = doc.getPage(m.pageIndex)
    const vertical = page.getRotation().angle % 180 !== 0
    let curves = 0
    for (const quad of m.quads) {
      profile(quad.every((v) => Math.abs(v) <= 14400 && decimal.test(String(v))))
      const [left, bottom, right, top] = quadBounds(quad)
      profile(left < right && bottom < top)
      if (m.type === 'squiggly') {
        curves += Math.ceil((vertical ? top - bottom : right - left) / 2)
        // Even the shortest six-operand cubic takes 14 bytes including its separator.
        profile(curves * 14 <= maxAppearance)
      }
    }
    profile(addMarkup(doc, page, m, DEFAULT_ANNOT_AUTHOR).getContentsSize() <= maxAppearance)
  }
}
