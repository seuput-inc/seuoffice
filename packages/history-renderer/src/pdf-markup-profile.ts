import { markupPaint, quadBounds } from '../../../apps/pdf/src/main/pdf-markup'
import type { MarkupInput } from '../../../apps/pdf/src/shared/ipc'
import { compact, profile } from './pdf-envelope'
import { array, dict, dictionary, num, ref } from './pdf-object-profile'

const types = {
  '/Highlight': 'highlight',
  '/Underline': 'underline',
  '/StrikeOut': 'strikeout',
  '/Squiggly': 'squiggly',
} as const

/** The form may paint this annotation only. It can neither call another form nor inherit page resources. */
export function checkMarkup(source: string, owner: number, body: (id: number) => string): void {
  const annot = dict(
    dictionary(source),
    '/Type',
    '/Subtype',
    '/Rect',
    '/QuadPoints',
    '/C',
    '/F',
    '/T',
    '/P',
    '/AP',
    '/Subj',
  )
  const type = annot.get('/Subtype')
  profile(
    annot.get('/Type') === '/Annot' && annot.get('/F') === '4' && ref(annot.get('/P')) === owner,
  )
  profile(
    type === '/Highlight' || type === '/Underline' || type === '/StrikeOut' || type === '/Squiggly',
  )
  for (const key of ['/T', '/Subj']) {
    if (key === '/Subj' && !annot.has(key)) continue
    const text = annot.get(key)
    profile(typeof text === 'string' && /^<FEFF(?:[0-9A-Fa-f]{4}){0,400}>$/i.test(text))
    let points = 0
    for (let pos = 5; pos < text.length - 1; pos += 4) {
      const unit = parseInt(text.slice(pos, pos + 4), 16)
      profile(++points <= 200 && !(unit >= 0xdc00 && unit <= 0xdfff))
      if (unit >= 0xd800 && unit <= 0xdbff) {
        pos += 4
        const low = parseInt(text.slice(pos, pos + 4), 16)
        profile(low >= 0xdc00 && low <= 0xdfff)
      }
    }
  }
  const coords = array(annot.get('/QuadPoints'), 8, 16 * 8).map((v) => num(v, -14400, 14400))
  profile(coords.length % 8 === 0)
  const rect = array(annot.get('/Rect'), 4, 4).map((v) => num(v, -14400, 14400))
  const extent = [Infinity, Infinity, -Infinity, -Infinity]
  const quads: number[][] = []
  let horizontal = 0,
    vertical = 0
  for (let i = 0; i < coords.length; i += 8) {
    const quad = coords.slice(i, i + 8)
    quads.push(quad)
    const [l, b, r, t] = quadBounds(quad)
    horizontal += Math.ceil((r - l) / 2)
    vertical += Math.ceil((t - b) / 2)
    profile(l < r && b < t)
    extent[0] = Math.min(extent[0]!, l)
    extent[1] = Math.min(extent[1]!, b)
    extent[2] = Math.max(extent[2]!, r)
    extent[3] = Math.max(extent[3]!, t)
  }
  profile(rect.every((n, i) => n === extent[i]))
  const channels = array(annot.get('/C'), 3, 3)
  const color: MarkupInput['color'] = [
    num(channels[0], 0, 1),
    num(channels[1], 0, 1),
    num(channels[2], 0, 1),
  ]
  const form = body(ref(dict(annot.get('/AP'), '/N').get('/N')))
  const start = /^(<<[\x20-\x7e\t\r\n]{0,1024}?>>)\r?\nstream\r?\n/.exec(form)
  profile(start)
  const fields = dict(dictionary(start[1]!), '/Type', '/Subtype', '/BBox', '/Resources', '/Length')
  profile(fields.get('/Type') === '/XObject' && fields.get('/Subtype') === '/Form')
  profile(array(fields.get('/BBox'), 4, 4).every((v, i) => num(v, -14400, 14400) === rect[i]))
  const highlight = type === '/Highlight'
  if (highlight) {
    const resources = dict(fields.get('/Resources'), '/ExtGState')
    const state = dict(dict(resources.get('/ExtGState'), '/GsM').get('/GsM'), '/Type', '/BM', '/ca')
    profile(
      state.get('/Type') === '/ExtGState' &&
        state.get('/BM') === '/Multiply' &&
        state.get('/ca') === '1',
    )
  } else dict(fields.get('/Resources'))
  const length = fields.get('/Length')
  profile(typeof length === 'string' && /^\d{1,4}$/.test(length) && Number(length) <= 2048)
  const end = start[0].length + Number(length)
  profile(/^\r?\nendstream$/.test(form.slice(end)))
  const paint = compact(form.slice(start[0].length, end))
    .split(' ')
    .map((token) => (/^[A-Za-z/]+$/.test(token) ? token : String(num(token, -30000, 30000))))
    .join(' ')
  // The page may have been rotated since this annotation was written. Try only the four
  // original orientations, bounding the number of cubics before invoking the shared writer.
  const markup = { type: types[type], quads, color }
  profile(
    (highlight ? [0] : [0, 90, 180, 270]).some((rotation) => {
      const curves = rotation % 180 === 0 ? horizontal : vertical
      return (
        (markup.type !== 'squiggly' || 14 * curves <= Number(length)) &&
        paint === compact(markupPaint(markup, rotation))
      )
    }),
  )
}
