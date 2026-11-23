import {
  PDFArray,
  PDFDict,
  PDFName,
  PDFNumber,
  StandardFonts,
  type PDFDocument,
  type PDFFont,
} from 'pdf-lib'
import type { EditSnapshot as PdfSnapshot } from '../../../apps/pdf/src/renderer/edit-state'
import { profile } from './pdf-envelope'
import { hangulRun, stampGlyphPath } from './pdf-stamp-outlines'

function decimal(value: number): number {
  const n = Number(value.toFixed(5))
  profile(Number.isFinite(n) && Math.abs(n) < 100_000)
  return n
}

function color(hex: string, opacity = 1) {
  let digits = hex.slice(1)
  if (digits.length <= 4) digits = [...digits].map((d) => d + d).join('')
  const channels = digits.match(/../g)!.map((pair) => parseInt(pair, 16) / 255)
  return {
    rgb: channels.slice(0, 3).map(decimal).join(' '),
    alpha: decimal((channels[3] ?? 1) * opacity),
  }
}

function unusedKey(dict: PDFDict, prefix: string) {
  profile(dict.keys().length < 16)
  for (let i = 0; ; i++) {
    const key = PDFName.of(i ? `${prefix}${i}` : prefix)
    if (!dict.has(key)) return key
  }
}

export async function writeStamps(
  doc: PDFDocument,
  { wm, hf }: NonNullable<PdfSnapshot['stampCfg']>,
  visible: readonly number[],
): Promise<void> {
  const ctx = doc.context
  let regular: PDFFont | undefined
  let bold: PDFFont | undefined
  const widths = [new Map<string, number>(), new Map<string, number>()]
  const wmInk = wm && color(wm.color, wm.opacity)
  const barInk = hf && color(hf.color)
  const pages = doc.getPages()
  for (const [display, index] of visible.entries()) {
    const page = pages[index]!
    const { width, height } = page.getSize()
    const resources = page.node.Resources()!
    const fonts = resources.lookupMaybe(PDFName.Font, PDFDict) ?? ctx.obj({})
    const states = resources.lookupMaybe(PDFName.ExtGState, PDFDict) ?? ctx.obj({})
    const streams: string[] = []
    let blocks = 0
    function append(block: string) {
      const paint = `\nq ${block} Q\n`
      profile(paint.length <= 2048)
      const i = streams.length - 1
      if (i < 0 || streams[i]!.length + paint.length > 2048 || blocks === 64) {
        profile(streams.length < 16)
        streams.push(paint)
        blocks = 1
      } else {
        streams[i] += paint
        blocks++
      }
    }
    function draw(text: string, x: number, y: number, align: number, size: number, heavy: boolean) {
      if (!text.trim()) return
      // Canvas fillText turns ASCII whitespace into spaces, not line breaks.
      text = text.replace(/[\t\n\f\r]/g, ' ')
      profile(text.length <= 1024 && Math.round(width * 4) > 0)
      profile(heavy ? Math.round(height * 4) > 0 : Math.round(size * 2.2) > 0)
      const run = /[\uac00-\ud7a3]/u.test(text) ? hangulRun(text, heavy) : undefined
      let fontKey: PDFName | undefined
      let encoded: string | undefined
      let advance: number
      let middle: number
      if (run) {
        advance = run.advance / run.em
        middle = run.middle / run.em
      } else {
        const font = heavy
          ? (bold ??= doc.embedStandardFont(StandardFonts.HelveticaBold))
          : (regular ??= doc.embedStandardFont(StandardFonts.Helvetica))
        try {
          encoded = font.encodeText(text).toString()
        } catch {
          profile(false)
        }
        // Tj advances by the glyph widths; it does not apply the AFM kerning pairs.
        advance = 0
        const cache = widths[heavy ? 1 : 0]!
        for (const ch of text) {
          let w = cache.get(ch)
          if (w === undefined) cache.set(ch, (w = font.widthOfTextAtSize(ch, 1)))
          advance += w
        }
        middle = font.heightAtSize(1, { descender: false }) - font.heightAtSize(1) / 2
        fontKey = fonts.entries().find(([, ref]) => ref === font.ref)?.[0]
        if (!fontKey) {
          fontKey = unusedKey(fonts, heavy ? 'HRStampBold' : 'HRStampRegular')
          fonts.set(fontKey, font.ref)
        }
      }
      if (heavy) size = Math.min(size, (0.8 * Math.hypot(width, height)) / advance)
      size = decimal(size)
      profile(size > 0 && size <= 14400)
      const { rgb, alpha } = heavy ? wmInk! : barInk!
      let gs = ''
      if (heavy || alpha !== 1) {
        let key = states
          .entries()
          .find(
            ([name, value]) =>
              /^\/HRStampOpacity\d{0,3}$/.test(name.toString()) &&
              value instanceof PDFDict &&
              value.lookupMaybe(PDFName.of('ca'), PDFNumber)?.asNumber() === alpha,
          )?.[0]
        if (!key) {
          key = unusedKey(states, 'HRStampOpacity')
          states.set(key, ctx.obj({ Type: 'ExtGState', ca: alpha }))
        }
        gs = `${key} gs `
      }
      const radians = heavy ? (wm!.angle * Math.PI) / 180 : 0
      const cos = Math.cos(radians),
        sin = Math.sin(radians)
      const dx = -advance * size * align,
        dy = -middle * size
      const left = x + dx * cos - dy * sin,
        baseline = y + dx * sin + dy * cos
      if (run) {
        const scale = size / run.em,
          a = decimal(cos * scale),
          b = decimal(sin * scale)
        profile(a !== 0 || b !== 0)
        for (const { at, code } of run.glyphs) {
          const matrix = [
            a,
            b,
            -b,
            a,
            decimal(left + at * scale * cos),
            decimal(baseline + at * scale * sin),
          ]
          const hex = code.toString(16).padStart(4, '0').toUpperCase()
          const path = stampGlyphPath(code, heavy)
          profile(path)
          append(
            `${gs}${rgb} rg ${matrix.join(' ')} cm /Span << /ActualText <FEFF${hex}> >> BDC ${path} f EMC`,
          )
        }
      } else {
        const matrix = [cos, sin, -sin, cos, left, baseline].map(decimal)
        append(`${gs}BT ${rgb} rg ${fontKey} ${size} Tf ${matrix.join(' ')} Tm ${encoded} Tj ET`)
      }
    }
    if (wm) draw(wm.text.trim(), width / 2, height / 2, 0.5, width * wm.sizeRatio, true)
    if (hf) {
      const no = hf.startAt + (display + 1) - 1
      const fill = (s: string) =>
        s.replaceAll('{page}', String(no)).replaceAll('{total}', String(visible.length))
      const inset = Math.min(height * 0.035, 26) + hf.fontSize * 1.1
      for (const [parts, y] of [
        [[hf.headerLeft, hf.headerCenter, hf.headerRight], height - inset],
        [
          [
            hf.footerLeft,
            hf.pageNumber ? `${no} / ${visible.length}` : hf.footerCenter,
            hf.footerRight,
          ],
          inset,
        ],
      ] as const) {
        for (const [i, text] of parts.entries())
          draw(fill(text), width * [0.06, 0.5, 0.94][i]!, y, i / 2, hf.fontSize, false)
      }
    }
    if (!streams.length) continue
    // Page normalization would wrap existing contents in separate q/Q streams and mutate their graph.
    resources.set(PDFName.Font, fonts)
    if (!resources.has(PDFName.XObject)) resources.set(PDFName.XObject, ctx.obj({}))
    resources.set(PDFName.ExtGState, states)
    const old = page.node.get(PDFName.Contents)
    const contents = old instanceof PDFArray ? old : ctx.obj(old ? [old] : [])
    profile(contents.size() + streams.length <= 16)
    for (const s of streams) contents.push(ctx.register(ctx.stream(s)))
    page.node.set(PDFName.Contents, contents)
  }
  // The caller may copy visible pages to another document before save registers the fonts.
  if (regular) await regular.embed()
  if (bold) await bold.embed()
}
