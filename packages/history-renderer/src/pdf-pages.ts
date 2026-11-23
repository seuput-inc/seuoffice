import { StandardFonts } from 'pdf-lib'
import { compact, profile, type PdfEnvelope } from './pdf-envelope'
import { checkMarkup } from './pdf-markup-profile'
import { array, dict, dictionary, num, ref } from './pdf-object-profile'
import { stampGlyphPath } from './pdf-stamp-outlines'

const standardFonts = new Set<string>(Object.values(StandardFonts))
const number = /^-?\d{1,5}(?:\.\d{1,5})?$/
type Font = { name: string; winAnsi: boolean }

// Page programs may use the original rectangle/text grammar, fixed stamp text or a known glyph outline.
function checkPaint(text: string, fonts: Map<string, Font>, states: Set<string>) {
  const t = compact(text).split(' ')
  if (t[0] === '') return
  let i = 0
  const take = (word: string) => profile(t[i++] === word)
  const nums = (n: number) => {
    for (let j = 0; j < n; j++) profile(number.test(t[i++] ?? ''))
  }
  for (let blocks = 0; i < t.length; blocks++) {
    profile(blocks < 64)
    if (t[i] !== 'q') {
      nums(4)
      take('re')
      take('f')
      continue
    }
    take('q')
    const gs = states.has(t[i]!)
    if (gs) {
      i++
      take('gs')
    }
    if (t[i] !== 'BT') {
      for (let j = 0; j < 3; j++) num(t[i++], 0, 1)
      take('rg')
      const matrix = i
      nums(6)
      const [a, b, c, d] = t.slice(matrix, matrix + 4).map(Number)
      const scale = Math.hypot(a!, b!)
      profile(a === d && b === -c! && scale > 0 && scale <= 7.032)
      take('cm')
      take('/Span')
      take('<<')
      take('/ActualText')
      const glyph = /^<FEFF([0-9A-Fa-f]{4})>$/.exec(t[i++] ?? '')
      profile(glyph)
      const code = parseInt(glyph[1]!, 16)
      profile((code >= 0x21 && code <= 0x7e) || (code >= 0xac00 && code <= 0xd7a3))
      take('>>')
      take('BDC')
      const end = t.indexOf('f', i)
      profile(end > i)
      for (let j = i; j < end; j++) {
        const word = t[j]!
        profile(
          /^[mlch]$/.test(word) || (/^-?\d{1,4}$/.test(word) && Math.abs(Number(word)) <= 8192),
        )
      }
      const path = t.slice(i, end).join(' ')
      profile((gs && stampGlyphPath(code, true) === path) || stampGlyphPath(code, false) === path)
      i = end + 1
      take('EMC')
      take('Q')
      continue
    }
    take('BT')
    if (gs) {
      for (let j = 0; j < 3; j++) num(t[i++], 0, 1)
      take('rg')
      const key = t[i++]!
      const face = fonts.get(key)
      profile(
        face?.winAnsi &&
          ((/^\/HRStampRegular\d{0,3}$/.test(key) && face.name === '/Helvetica') ||
            (/^\/HRStampBold\d{0,3}$/.test(key) && face.name === '/Helvetica-Bold')),
      )
      profile(num(t[i++], 0, 14400) > 0)
      take('Tf')
      const matrix = i
      nums(6)
      const [a, b, c, d] = t.slice(matrix, matrix + 4).map(Number)
      profile(a === d && b === -c! && Math.abs(Math.hypot(a!, b!) - 1) <= 0.0001)
      take('Tm')
      profile(/^<(?:[\da-fA-F]{2})+>$/.test(t[i++] ?? ''))
      take('Tj')
      take('ET')
      take('Q')
      continue
    }
    let color = 0
    while (number.test(t[i] ?? '')) {
      nums(1)
      color++
    }
    profile(
      (color === 1 && t[i] === 'g') ||
        (color === 3 && t[i] === 'rg') ||
        (color === 4 && t[i] === 'k'),
    )
    i++
    profile(fonts.has(t[i++]!))
    nums(1)
    take('Tf')
    if (t[i + 1] === 'TL') {
      nums(1)
      take('TL')
    }
    nums(6)
    take('Tm')
    while (t[i]?.startsWith('<')) {
      profile(/^<(?:[\da-fA-F]{2})*>$/.test(t[i++]!))
      take('Tj')
      if (t[i] === 'T*') i++
    }
    take('ET')
    take('Q')
  }
}

/** All non-container objects must have one known role in this flat page tree. */
export function checkPages({ objects, root, info }: PdfEnvelope): number {
  const seen = new Set<number>()
  function body(id: number) {
    const s = objects.get(id)
    profile(s !== undefined && !seen.has(id) && !/[^\x20-\x7e\t\r\n]/.test(s))
    seen.add(id)
    return s
  }
  if (info !== undefined) {
    const s = body(info)
    profile(s.startsWith('<<') && s.endsWith('>>'))
    const field =
      /[\r\n\t ]+\/(Title|Author|Subject|Keywords|Creator|Producer|CreationDate|ModDate)[\r\n\t ]+(?:<[\da-fA-F]{0,4096}>|\([^()\\\r\n]{0,1024}\))/y
    const keys = new Set<string>()
    let pos = 2
    while (pos < s.length - 2) {
      field.lastIndex = pos
      const m = field.exec(s)
      if (!m) break
      profile(!keys.has(m[1]!))
      pos = field.lastIndex
      keys.add(m[1]!)
    }
    profile(/^[\r\n\t ]*>>$/.test(s.slice(pos)))
  }
  const catalog = /^<< \/Type \/Catalog \/Pages (\d{1,3}) 0 R >>$/.exec(compact(body(root)))
  profile(catalog)
  const treeId = Number(catalog[1])
  const tree =
    /^<< \/Type \/Pages \/Kids \[((?: \d{1,3} 0 R){1,128}) \] \/Count (\d{1,3}) >>$/.exec(
      compact(body(treeId)),
    )
  profile(tree)
  const kids = [...tree[1]!.matchAll(/ (\d+) 0 R/g)]
  profile(kids.length === Number(tree[2]))
  const checkedFonts = new Map<number, Font>()
  for (const kid of kids) {
    const owner = Number(kid[1])
    const page = dict(
      dictionary(body(owner)),
      '/Type',
      '/Parent',
      '/Resources',
      '/MediaBox',
      '/Rotate',
      '/Contents',
      '/Annots',
    )
    profile(page.get('/Type') === '/Page' && ref(page.get('/Parent')) === treeId)
    const box = array(page.get('/MediaBox'), 4, 4)
    profile(box[0] === '0' && box[1] === '0')
    for (const v of box.slice(2)) {
      profile(typeof v === 'string' && /^\d{1,5}(?:\.\d{1,8})?$/.test(v) && num(v, 0, 14400) > 0)
    }
    const rotation = page.get('/Rotate')
    profile(
      rotation === undefined ||
        rotation === '0' ||
        rotation === '90' ||
        rotation === '180' ||
        rotation === '270',
    )
    const resources = dict(page.get('/Resources'), '/Font', '/XObject', '/ExtGState')
    if (resources.has('/XObject')) dict(resources.get('/XObject'))
    const fonts = new Map<string, Font>()
    if (resources.has('/Font')) {
      const entries = resources.get('/Font')
      profile(entries instanceof Map && entries.size <= 16)
      for (const [key, value] of entries) {
        const id = ref(value)
        let face = checkedFonts.get(id)
        if (face === undefined) {
          const font = dict(dictionary(body(id)), '/Type', '/Subtype', '/BaseFont', '/Encoding')
          const base = font.get('/BaseFont'),
            encoding = font.get('/Encoding')
          profile(
            font.get('/Type') === '/Font' &&
              font.get('/Subtype') === '/Type1' &&
              typeof base === 'string' &&
              base.startsWith('/') &&
              standardFonts.has(base.slice(1)) &&
              (encoding === undefined || encoding === '/WinAnsiEncoding'),
          )
          face = { name: base, winAnsi: encoding === '/WinAnsiEncoding' }
          checkedFonts.set(id, face)
        }
        fonts.set(key, face)
      }
    }
    const states = new Set<string>()
    if (resources.has('/ExtGState')) {
      const entries = resources.get('/ExtGState')
      profile(entries instanceof Map && entries.size <= 16)
      for (const [name, value] of entries) {
        profile(/^\/HRStampOpacity\d{0,3}$/.test(name))
        const state = dict(value, '/Type', '/ca')
        profile(state.get('/Type') === '/ExtGState')
        const opacity = state.get('/ca')
        profile(typeof opacity === 'string' && number.test(opacity))
        num(opacity, 0, 1)
        states.add(name)
      }
    }
    if (page.has('/Annots')) {
      for (const id of array(page.get('/Annots'), 0, 128)) checkMarkup(body(ref(id)), owner, body)
    }
    const content = page.get('/Contents')
    if (content === undefined) continue
    for (const value of Array.isArray(content) ? array(content, 1, 16) : [content]) {
      const s = body(ref(value))
      const start = /^<< \/Length (\d{1,4}) >>\nstream\n/.exec(s)
      profile(start)
      const length = Number(start[1]),
        from = start[0].length
      profile(length <= 2048 && s.slice(from + length) === '\nendstream')
      checkPaint(s.slice(from, from + length), fonts, states)
    }
  }
  profile(seen.size === objects.size)
  return kids.length
}
