import { StandardFonts } from 'pdf-lib'
import { compact, profile, type PdfEnvelope } from './pdf-envelope'

const standardFonts = new Set<string>(Object.values(StandardFonts))
const number = /^-?\d{1,5}(?:\.\d{1,5})?$/

// Permit passive rectangles and Core-14 text. No image, form XObject, inline image or embedded font decoder.
function checkPaint(text: string, fonts: Set<string>) {
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
    take('BT')
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
  const checkedFonts = new Set<number>()
  for (const kid of kids) {
    const page =
      /^<< \/Type \/Page(?: \/Parent (\d{1,3}) 0 R)? \/Resources <<(?: \/Font <<((?: \/[A-Za-z0-9_-]{1,80} \d{1,3} 0 R){0,16}) >> \/XObject << >> \/ExtGState << >>)? >> \/MediaBox \[ 0 0 (\d{1,5}(?:\.\d{1,8})?) (\d{1,5}(?:\.\d{1,8})?) \]((?: \/(?:Parent \d{1,3} 0 R|Rotate (?:0|90|180|270)|Contents (?:\d{1,3} 0 R|\[(?: \d{1,3} 0 R){1,16} \])|Annots \[ \])){0,4}) >>$/.exec(
        compact(body(Number(kid[1]))),
      )
    profile(page)
    const tailParent = / \/Parent (\d{1,3}) 0 R/.exec(page[5]!)
    profile(!(tailParent && page[1]) && Number(page[1] ?? tailParent?.[1]) === treeId)
    profile([Number(page[3]), Number(page[4])].every((n) => n > 0 && n <= 14400))
    const fonts = new Set<string>()
    for (const [, name, ref] of (page[2] ?? '').matchAll(/ (\/[A-Za-z0-9_-]+) (\d+) 0 R/g)) {
      profile(!fonts.has(name!))
      fonts.add(name!)
      const id = Number(ref)
      if (checkedFonts.has(id)) continue
      const font =
        /^<< \/Type \/Font \/Subtype \/Type1 \/BaseFont \/([A-Za-z-]+)(?: \/Encoding \/WinAnsiEncoding)? >>$/.exec(
          compact(body(id)),
        )
      profile(font && standardFonts.has(font[1]!))
      checkedFonts.add(id)
    }
    const extra = page[5]!
    const keys = extra.match(/\/[A-Za-z]+/g) ?? []
    profile(keys.length === new Set(keys).size)
    const content = extra.replace(/ \/Parent \d{1,3} 0 R/, '')
    for (const [, id] of content.matchAll(/ (\d+) 0 R/g)) {
      const s = body(Number(id))
      const start = /^<< \/Length (\d{1,4}) >>\nstream\n/.exec(s)
      profile(start)
      const length = Number(start[1]),
        from = start[0].length
      profile(length <= 2048 && s.slice(from + length) === '\nendstream')
      checkPaint(s.slice(from, from + length), fonts)
    }
  }
  profile(seen.size === objects.size)
  return kids.length
}
