import {
  PDFArray,
  PDFHexString,
  PDFName,
  type PDFDocument,
  type PDFPage,
  type PDFRef,
} from 'pdf-lib'
import type { MarkupInput } from '../shared/ipc'

const num = (v: number) => Math.round(v * 100) / 100
export const DEFAULT_ANNOT_AUTHOR = 'SeuOffice'

export const quadBounds = (q: number[]) => {
  const xs = [q[0]!, q[2]!, q[4]!, q[6]!]
  const ys = [q[1]!, q[3]!, q[5]!, q[7]!]
  return [Math.min(...xs), Math.min(...ys), Math.max(...xs), Math.max(...ys)] as const
}

/**
 * Hand-written appearance stream (/AP /N), so viewers don't self-draw from QuadPoints —
 * Acrobat/Preview/pdfjs all render from AP for consistent results.
 * Highlight uses Multiply blending to mimic a highlighter; underline/strikeout are stroked
 * segments drawn along the "visual bottom edge" (pageRot is the page's final /Rotate;
 * at 90/270 line height runs along the x axis); squiggly is a repeating wave on the
 * same edge.
 */
export function markupPaint(
  m: Pick<MarkupInput, 'type' | 'color' | 'quads'>,
  pageRot: number,
): string {
  const [r, g, b] = m.color
  const ops: string[] = []
  if (m.type === 'highlight') {
    ops.push('/GsM gs', `${r} ${g} ${b} rg`)
    for (const q of m.quads) {
      const [x1, y1, x2, y2] = quadBounds(q)
      ops.push(`${num(x1)} ${num(y1)} ${num(x2 - x1)} ${num(y2 - y1)} re f`)
    }
  } else if (m.type === 'squiggly') {
    const t = 0.08 // same visual bottom-edge offset as underline
    ops.push(`${r} ${g} ${b} RG`)
    for (const q of m.quads) {
      const [x1, y1, x2, y2] = quadBounds(q)
      const h = pageRot % 180 === 0 ? y2 - y1 : x2 - x1
      ops.push(`${Math.max(0.8, num(h * 0.06))} w`)
      // The perpendicular points the way the wave bulges: from the visual bottom
      // edge into the text, which is -x at rot 90 and -y at rot 180
      if (pageRot === 90) {
        squigglyOps(ops, x2 - h * t, y1, 0, 1, -1, 0, y2 - y1)
      } else if (pageRot === 270) {
        squigglyOps(ops, x1 + h * t, y1, 0, 1, 1, 0, y2 - y1)
      } else {
        const y = pageRot === 180 ? y2 - h * t : y1 + h * t
        squigglyOps(ops, x1, y, 1, 0, 0, pageRot === 180 ? -1 : 1, x2 - x1)
      }
    }
  } else {
    const t = m.type === 'underline' ? 0.08 : 0.46
    ops.push(`${r} ${g} ${b} RG`)
    for (const q of m.quads) {
      const [x1, y1, x2, y2] = quadBounds(q)
      const h = pageRot % 180 === 0 ? y2 - y1 : x2 - x1
      ops.push(`${Math.max(0.8, num(h * 0.06))} w`)
      if (pageRot === 90) {
        const x = x2 - h * t
        ops.push(`${num(x)} ${num(y1)} m ${num(x)} ${num(y2)} l S`)
      } else if (pageRot === 270) {
        const x = x1 + h * t
        ops.push(`${num(x)} ${num(y1)} m ${num(x)} ${num(y2)} l S`)
      } else {
        const y = pageRot === 180 ? y2 - h * t : y1 + h * t
        ops.push(`${num(x1)} ${num(y)} m ${num(x2)} ${num(y)} l S`)
      }
    }
  }
  return ops.join('\n')
}

function markupAppearance(pdfDoc: PDFDocument, m: MarkupInput, rect: number[], pageRot: number) {
  return pdfDoc.context.stream(markupPaint(m, pageRot), {
    Type: 'XObject',
    Subtype: 'Form',
    BBox: rect,
    Resources:
      m.type === 'highlight'
        ? { ExtGState: { GsM: { Type: 'ExtGState', BM: 'Multiply', ca: 1 } } }
        : {},
  })
}

const SUBTYPE: Record<MarkupInput['type'], string> = {
  highlight: 'Highlight',
  underline: 'Underline',
  strikeout: 'StrikeOut',
  squiggly: 'Squiggly',
}

/**
 * Markup inputs arrive from the renderer/AI layer: reject colors outside 0-1,
 * empty quad lists, and non-finite quad coordinates before they reach the
 * appearance stream (Math.min(...[]) is Infinity, NaN poisons BBox/Rect).
 */
export function validMarkup(m: MarkupInput): boolean {
  if (!Array.isArray(m.color) || m.color.length !== 3) return false
  if (!m.color.every((c) => typeof c === 'number' && Number.isFinite(c) && c >= 0 && c <= 1)) {
    return false
  }
  if (!Array.isArray(m.quads) || m.quads.length === 0) return false
  return m.quads.every(
    (q) =>
      Array.isArray(q) &&
      q.length === 8 &&
      q.every((v) => typeof v === 'number' && Number.isFinite(v)),
  )
}

export function addMarkup(pdfDoc: PDFDocument, page: PDFPage, m: MarkupInput, author: string) {
  const xs = m.quads.flatMap((q) => [q[0]!, q[2]!, q[4]!, q[6]!])
  const ys = m.quads.flatMap((q) => [q[1]!, q[3]!, q[5]!, q[7]!])
  const rect = [Math.min(...xs), Math.min(...ys), Math.max(...xs), Math.max(...ys)]
  const pageRot = ((page.getRotation().angle % 360) + 360) % 360
  const appearance = markupAppearance(pdfDoc, m, rect, pageRot)
  const apRef = pdfDoc.context.register(appearance)
  const annot = pdfDoc.context.obj({
    Type: 'Annot',
    Subtype: SUBTYPE[m.type],
    Rect: rect,
    QuadPoints: m.quads.flat(),
    C: m.color,
    F: 4, // print
    T: PDFHexString.fromText(author),
    P: page.ref,
    AP: { N: apRef },
  })
  // an accepted AI highlight's category; without it nothing changes in the bytes
  const subject = markupSubject(m)
  if (subject !== null) annot.set(PDFName.of('Subj'), PDFHexString.fromText(subject))
  appendAnnot(pdfDoc, page, pdfDoc.context.register(annot))
  return appearance
}

/** longest /Subj written, in characters (code points); a category name is a few words */
const MAX_SUBJECT_CHARS = 200

/** the markup's /Subj text, or null when it has none (empty or not a string) */
function markupSubject(m: MarkupInput): string | null {
  if (typeof m.subject !== 'string') return null
  // by code point: a cut never leaves half of a surrogate pair
  const text = Array.from(m.subject.trim()).slice(0, MAX_SUBJECT_CHARS).join('')
  return text === '' ? null : text
}

export function appendAnnot(pdfDoc: PDFDocument, page: PDFPage, annotRef: PDFRef): void {
  const existing = page.node.lookupMaybe(PDFName.of('Annots'), PDFArray)
  if (existing) {
    existing.push(annotRef)
  } else {
    page.node.set(PDFName.of('Annots'), pdfDoc.context.obj([annotRef]))
  }
}

/**
 * One squiggly (wavy underline) stroke: repeating quadratic Bezier humps along a
 * baseline, started and ended on the wave's midline — period ≈ 4 user units,
 * amplitude ≈ 1.5. The midline sits one amplitude above the underline position so
 * troughs stay inside the quad (the /AP BBox clips). (dx,dy) walks the line, (px,py)
 * is its unit perpendicular.
 */
function squigglyOps(
  ops: string[],
  ax: number,
  ay: number,
  dx: number,
  dy: number,
  px: number,
  py: number,
  length: number,
): void {
  const half = 2 // half period
  const amp = 1.5
  const at = (d: number, off: number) =>
    `${num(ax + dx * d + px * off)} ${num(ay + dy * d + py * off)}`
  ops.push(`${at(0, amp)} m`)
  let side = 1
  let from = 0
  while (from < length) {
    const to = Math.min(from + half, length)
    // The quadratic hump PDF has no operator for, written as the cubic that
    // reproduces it: control points at 1/3 and 2/3 of the segment, 4/3 of the
    // apex above the midline (a symmetric cubic peaks at 3/4 of its control
    // height). A single 'v' control point skewed the hump and pushed the
    // troughs below the midline.
    const ctrl = amp + (side * 4 * amp) / 3
    const third = (to - from) / 3
    ops.push(`${at(from + third, ctrl)} ${at(from + 2 * third, ctrl)} ${at(to, amp)} c`)
    from = to
    side = -side
  }
  ops.push('S')
}
