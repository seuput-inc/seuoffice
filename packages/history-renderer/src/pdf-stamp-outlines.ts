import { Buffer } from 'node:buffer'
import { parse, type Font, type Glyph, type PathCommand } from 'opentype.js'
import bold from '../../ui/template-fonts/Pretendard-Bold.otf'
import regular from '../../ui/template-fonts/Pretendard-Regular.otf'
import { profile } from './pdf-envelope'

let regularFace: Font | undefined
let boldFace: Font | undefined

function face(heavy: boolean): Font {
  const cached = heavy ? boldFace : regularFace
  if (cached) return cached
  const data = Buffer.from(heavy ? bold : regular, 'base64')
  const bytes =
    data.byteLength === data.buffer.byteLength
      ? data.buffer
      : data.buffer.slice(data.byteOffset, data.byteOffset + data.byteLength)
  const parsed = parse(bytes, { lowMemory: true })
  if (heavy) boldFace = parsed
  else regularFace = parsed
  return parsed
}

function contour(command: PathCommand) {
  switch (command.type) {
    case 'M':
      return `${command.x} ${command.y} m`
    case 'L':
      return `${command.x} ${command.y} l`
    case 'C':
      return `${command.x1} ${command.y1} ${command.x2} ${command.y2} ${command.x} ${command.y} c`
    case 'Z':
      return 'h'
    default:
      return profile(false)
  }
}

export function stampGlyphPath(code: number, heavy: boolean): string | undefined {
  if (
    !Number.isInteger(code) ||
    !((code >= 0x21 && code <= 0x7e) || (code >= 0xac00 && code <= 0xd7a3))
  )
    return undefined
  const glyph = face(heavy).charToGlyph(String.fromCharCode(code))
  const commands = glyph.path.commands
  if (!glyph.index || !commands.length || commands.length > 128) return undefined
  return commands.map(contour).join(' ')
}

/** Only precomposed Hangul plus printable ASCII in mixed runs. No host fallback or shaping guess. */
export function hangulRun(text: string, heavy: boolean) {
  profile(text.length <= 128 && /^[\x20-\x7e\uac00-\ud7a3]+$/u.test(text))
  const font = face(heavy)
  const glyphs: { at: number; code: number }[] = []
  let advance = 0
  let last: Glyph | undefined
  for (const char of text) {
    const glyph = font.charToGlyph(char)
    profile(glyph.index !== 0 && glyph.advanceWidth !== undefined)
    if (last) advance += font.getKerningValue(last, glyph)
    if (char !== ' ') glyphs.push({ at: advance, code: char.charCodeAt(0) })
    advance += glyph.advanceWidth
    last = glyph
  }
  return { glyphs, em: font.unitsPerEm, advance, middle: (font.ascender + font.descender) / 2 }
}
