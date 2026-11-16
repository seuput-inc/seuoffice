// What probe-check.mjs measures in the artifacts of a probe run: PDFs (page count, media boxes,
// filled rectangles with their colors, the unicode of the embedded fonts) and PNGs (size, pixels).
// No dependencies: Node's zlib and a small content-stream interpreter. It reads what the print
// and snapshot paths of the two system webviews write (Skia on Windows, Quartz on macOS); a file
// it cannot read is reported as such, never guessed at.

import { inflateSync } from 'node:zlib'

// ---------------------------------------------------------------------------------------- PNG

const CHANNELS = { 0: 1, 2: 3, 4: 2, 6: 4 }

export function decodePng(buffer) {
  const signature = [137, 80, 78, 71, 13, 10, 26, 10]
  if (buffer.length < 8 || signature.some((byte, index) => buffer[index] !== byte))
    throw new Error('not a PNG')
  let position = 8
  let header
  const data = []
  while (position + 8 <= buffer.length) {
    const length = buffer.readUInt32BE(position)
    const type = buffer.toString('latin1', position + 4, position + 8)
    const body = buffer.subarray(position + 8, position + 8 + length)
    if (type === 'IHDR') {
      header = {
        width: body.readUInt32BE(0),
        height: body.readUInt32BE(4),
        depth: body[8],
        colorType: body[9],
        interlace: body[12],
      }
    } else if (type === 'IDAT') data.push(body)
    else if (type === 'IEND') break
    position += 12 + length
  }
  if (!header) throw new Error('no IHDR')
  if (header.depth !== 8 || header.interlace !== 0 || !CHANNELS[header.colorType]) {
    throw new Error(
      `unsupported PNG (depth ${header.depth}, color type ${header.colorType}, interlace ${header.interlace})`,
    )
  }
  const channels = CHANNELS[header.colorType]
  const stride = header.width * channels
  const raw = inflateSync(Buffer.concat(data))
  const pixels = Buffer.alloc(header.height * stride)
  for (let row = 0; row < header.height; row += 1) {
    const filter = raw[row * (stride + 1)]
    const source = raw.subarray(row * (stride + 1) + 1, (row + 1) * (stride + 1))
    for (let index = 0; index < stride; index += 1) {
      const left = index >= channels ? pixels[row * stride + index - channels] : 0
      const up = row > 0 ? pixels[(row - 1) * stride + index] : 0
      const upLeft =
        row > 0 && index >= channels ? pixels[(row - 1) * stride + index - channels] : 0
      let predictor = 0
      if (filter === 1) predictor = left
      else if (filter === 2) predictor = up
      else if (filter === 3) predictor = (left + up) >> 1
      else if (filter === 4) {
        const estimate = left + up - upLeft
        const toLeft = Math.abs(estimate - left)
        const toUp = Math.abs(estimate - up)
        const toUpLeft = Math.abs(estimate - upLeft)
        predictor = toLeft <= toUp && toLeft <= toUpLeft ? left : toUp <= toUpLeft ? up : upLeft
      }
      pixels[row * stride + index] = (source[index] + predictor) & 255
    }
  }
  return {
    width: header.width,
    height: header.height,
    channels,
    // [r, g, b, a], each 0-255
    pixel(x, y) {
      const at = y * stride + x * channels
      if (channels === 4) return [pixels[at], pixels[at + 1], pixels[at + 2], pixels[at + 3]]
      if (channels === 3) return [pixels[at], pixels[at + 1], pixels[at + 2], 255]
      if (channels === 2) return [pixels[at], pixels[at], pixels[at], pixels[at + 1]]
      return [pixels[at], pixels[at], pixels[at], 255]
    },
  }
}

// ---------------------------------------------------------------------------------------- PDF

const compose = (a, b) => [
  a[0] * b[0] + a[1] * b[2],
  a[0] * b[1] + a[1] * b[3],
  a[2] * b[0] + a[3] * b[2],
  a[2] * b[1] + a[3] * b[3],
  a[4] * b[0] + a[5] * b[2] + b[4],
  a[4] * b[1] + a[5] * b[3] + b[5],
]
const apply = (m, x, y) => [m[0] * x + m[2] * y + m[4], m[1] * x + m[3] * y + m[5]]

function readObjects(buffer) {
  const text = buffer.toString('latin1')
  const objects = new Map()
  const pattern = /(\d+)\s+(\d+)\s+obj\b/g
  let match
  while ((match = pattern.exec(text))) {
    const start = pattern.lastIndex
    const stream = text.indexOf('stream', start)
    const end = text.indexOf('endobj', start)
    if (end < 0) break
    let dictionary = text.slice(start, end)
    let body
    if (stream >= 0 && stream < end) {
      dictionary = text.slice(start, stream)
      let from = stream + 6
      if (text[from] === '\r') from += 1
      if (text[from] === '\n') from += 1
      const to = text.lastIndexOf('endstream', end)
      let finish = to >= from ? to : end
      const declared = /\/Length\s+(\d+)(?!\s+\d+\s+R)/.exec(dictionary)
      if (declared && from + Number(declared[1]) <= finish) {
        finish = from + Number(declared[1])
      } else {
        // an indirect length: drop the one end-of-line before `endstream`
        if (text[finish - 1] === '\n') finish -= 1
        if (text[finish - 1] === '\r') finish -= 1
      }
      body = buffer.subarray(from, finish)
    }
    objects.set(Number(match[1]), { dictionary, body })
    pattern.lastIndex = Math.max(pattern.lastIndex, end)
  }
  return objects
}

function streamOf(object) {
  if (!object || !object.body) return undefined
  const filter = /\/Filter\s*(\[[^\]]*\]|\/\w+)/.exec(object.dictionary)
  if (!filter) return object.body
  const names = filter[1].match(/\/\w+/g) || []
  if (names.length === 1 && names[0] === '/FlateDecode') {
    try {
      return inflateSync(object.body)
    } catch {
      return undefined
    }
  }
  return undefined
}

function refsOf(text) {
  return [...text.matchAll(/(\d+)\s+\d+\s+R/g)].map((match) => Number(match[1]))
}

function inherited(objects, object, key) {
  let current = object
  for (let depth = 0; current && depth < 16; depth += 1) {
    const found = new RegExp(`/${key}\\s*(\\[[^\\]]*\\]|[-\\d.]+)`).exec(current.dictionary)
    if (found) return found[1]
    const parent = /\/Parent\s+(\d+)\s+\d+\s+R/.exec(current.dictionary)
    current = parent ? objects.get(Number(parent[1])) : undefined
  }
  return undefined
}

function resourcesOf(objects, page) {
  let current = page
  for (let depth = 0; current && depth < 16; depth += 1) {
    const found = /\/Resources\s*(?:(\d+)\s+\d+\s+R|(<<[\s\S]*>>))/.exec(current.dictionary)
    if (found) return found[1] ? objects.get(Number(found[1]))?.dictionary || '' : found[2]
    const parent = /\/Parent\s+(\d+)\s+\d+\s+R/.exec(current.dictionary)
    current = parent ? objects.get(Number(parent[1])) : undefined
  }
  return ''
}

function xobject(objects, resources, name) {
  const section = /\/XObject\s*(?:(\d+)\s+\d+\s+R|<<([^<>]*)>>)/.exec(resources)
  if (!section) return undefined
  const table = section[1] ? objects.get(Number(section[1]))?.dictionary || '' : section[2]
  const entry = new RegExp(
    `/${name.replace(/[^\w]/g, (c) => `\\${c}`)}\\s+(\\d+)\\s+\\d+\\s+R`,
  ).exec(table)
  return entry ? objects.get(Number(entry[1])) : undefined
}

const TOKEN =
  /\((?:\\[\s\S]|[^\\)])*\)|<<|>>|<[0-9A-Fa-f\s]*>|\[|\]|\/[^\s/[\]<>()]*|[-+]?(?:\d+\.?\d*|\.\d+)|[A-Za-z*'"]+/g

const toRgb = (operands) => {
  const numbers = operands.filter((value) => typeof value === 'number')
  const byte = (value) => Math.round(Math.min(1, Math.max(0, value)) * 255)
  if (numbers.length === 1) return [byte(numbers[0]), byte(numbers[0]), byte(numbers[0])]
  if (numbers.length === 3) return numbers.map(byte)
  if (numbers.length === 4) {
    const [c, m, y, k] = numbers
    return [byte((1 - c) * (1 - k)), byte((1 - m) * (1 - k)), byte((1 - y) * (1 - k))]
  }
  return undefined
}

// Interprets one content stream; every filled path becomes {bbox, color}. Text and images are
// not drawn here; their positions are not needed.
function interpret(objects, resources, content, matrix, fills, depth) {
  const text = content.toString('latin1')
  const stack = []
  let ctm = matrix
  let color = [0, 0, 0]
  let path = []
  let operands = []
  let inArray = 0
  TOKEN.lastIndex = 0
  let match
  const paint = () => {
    if (path.length) {
      const points = path.map(([x, y]) => apply(ctm, x, y))
      const xs = points.map((point) => point[0])
      const ys = points.map((point) => point[1])
      fills.push({
        bbox: {
          x0: Math.min(...xs),
          y0: Math.min(...ys),
          x1: Math.max(...xs),
          y1: Math.max(...ys),
        },
        color,
      })
    }
    path = []
  }
  while ((match = TOKEN.exec(text))) {
    const token = match[0]
    if (token === '[') {
      inArray += 1
      continue
    }
    if (token === ']') {
      inArray = Math.max(0, inArray - 1)
      continue
    }
    if (inArray) continue
    if (token === '<<' || token === '>>' || token[0] === '(' || token[0] === '<') continue
    if (token[0] === '/') {
      operands.push(token)
      continue
    }
    if (/^[-+.\d]/.test(token)) {
      operands.push(Number(token))
      continue
    }
    const n = operands.filter((value) => typeof value === 'number')
    switch (token) {
      case 'q':
        stack.push({ ctm, color })
        break
      case 'Q': {
        const state = stack.pop()
        if (state) ({ ctm, color } = state)
        break
      }
      case 'cm':
        if (n.length === 6) ctm = compose(n, ctm)
        break
      case 're':
        if (n.length === 4) {
          const [x, y, w, h] = n
          path.push([x, y], [x + w, y], [x + w, y + h], [x, y + h])
        }
        break
      case 'm':
      case 'l':
        if (n.length === 2) path.push([n[0], n[1]])
        break
      case 'c':
        if (n.length === 6) path.push([n[4], n[5]])
        break
      case 'v':
      case 'y':
        if (n.length === 4) path.push([n[2], n[3]])
        break
      case 'f':
      case 'F':
      case 'f*':
      case 'B':
      case 'B*':
      case 'b':
      case 'b*':
        paint()
        break
      case 'S':
      case 's':
      case 'n':
        path = []
        break
      case 'rg':
      case 'g':
      case 'k':
      case 'sc':
      case 'scn': {
        const rgb = toRgb(operands)
        if (rgb) color = rgb
        break
      }
      case 'BI': {
        const end = text.indexOf('EI', TOKEN.lastIndex)
        TOKEN.lastIndex = end < 0 ? text.length : end + 2
        break
      }
      case 'Do': {
        const name = operands.find((value) => typeof value === 'string')
        const form = name && depth < 6 ? xobject(objects, resources, name.slice(1)) : undefined
        if (form && /\/Subtype\s*\/Form/.test(form.dictionary)) {
          const nested = streamOf(form)
          const own = /\/Matrix\s*\[([^\]]*)\]/.exec(form.dictionary)
          const formMatrix = own ? own[1].trim().split(/\s+/).map(Number) : [1, 0, 0, 1, 0, 0]
          const formResources = /\/Resources\s*(?:(\d+)\s+\d+\s+R|(<<[\s\S]*>>))/.exec(
            form.dictionary,
          )
          const nestedResources = formResources
            ? formResources[1]
              ? objects.get(Number(formResources[1]))?.dictionary || ''
              : formResources[2]
            : resources
          if (nested)
            interpret(objects, nestedResources, nested, compose(formMatrix, ctm), fills, depth + 1)
        }
        break
      }
      default:
    }
    operands = []
  }
}

function unicodeOf(objects) {
  const found = new Set()
  const add = (hex) => {
    const bytes = Buffer.from(hex.replace(/\s+/g, ''), 'hex')
    for (let index = 0; index + 1 < bytes.length; index += 2) {
      const unit = bytes.readUInt16BE(index)
      if (unit >= 0xd800 && unit < 0xdc00 && index + 3 < bytes.length) {
        const low = bytes.readUInt16BE(index + 2)
        found.add(0x10000 + ((unit - 0xd800) << 10) + (low - 0xdc00))
        index += 2
      } else found.add(unit)
    }
  }
  for (const object of objects.values()) {
    const data = streamOf(object)
    if (!data) continue
    const text = data.toString('latin1')
    if (!text.includes('beginbf')) continue
    for (const block of text.matchAll(/beginbfchar([\s\S]*?)endbfchar/g)) {
      for (const pair of block[1].matchAll(/<[0-9A-Fa-f\s]+>\s*<([0-9A-Fa-f\s]+)>/g)) add(pair[1])
    }
    for (const block of text.matchAll(/beginbfrange([\s\S]*?)endbfrange/g)) {
      for (const row of block[1].matchAll(
        /<([0-9A-Fa-f]+)>\s*<([0-9A-Fa-f]+)>\s*(?:<([0-9A-Fa-f\s]+)>|\[([^\]]*)\])/g,
      )) {
        const low = parseInt(row[1], 16)
        const high = parseInt(row[2], 16)
        if (row[4] !== undefined) {
          for (const item of row[4].matchAll(/<([0-9A-Fa-f\s]+)>/g)) add(item[1])
        } else if (row[3]) {
          const first = parseInt(row[3].replace(/\s+/g, ''), 16)
          const width = row[3].replace(/\s+/g, '').length
          for (let code = 0; code <= Math.min(high - low, 70000); code += 1) {
            add((first + code).toString(16).padStart(width, '0'))
          }
        }
      }
    }
  }
  return found
}

/** What a PDF holds: `pages` with each page's media box, filled rectangles and swatch, and
 * `unicode`, the code points of its fonts' ToUnicode maps. Throws when nothing can be read. */
export function measurePdf(buffer) {
  if (buffer.subarray(0, 5).toString('latin1') !== '%PDF-') throw new Error('not a PDF')
  const objects = readObjects(buffer)
  const pages = []
  for (const [id, object] of objects) {
    if (!/\/Type\s*\/Page(?![A-Za-z])/.test(object.dictionary)) continue
    const box = inherited(objects, object, 'MediaBox')
    const numbers = box ? box.replace(/[[\]]/g, '').trim().split(/\s+/).map(Number) : undefined
    const rotate = Number(inherited(objects, object, 'Rotate') || 0)
    const contents = /\/Contents\s*(\[[^\]]*\]|\d+\s+\d+\s+R)/.exec(object.dictionary)
    const fills = []
    if (contents) {
      const resources = resourcesOf(objects, object)
      const parts = refsOf(contents[1])
        .map((ref) => streamOf(objects.get(ref)))
        .filter(Boolean)
      if (parts.length)
        interpret(
          objects,
          resources,
          Buffer.concat(parts.flatMap((part) => [part, Buffer.from('\n')])),
          [1, 0, 0, 1, 0, 0],
          fills,
          0,
        )
    }
    pages.push({ id, mediaBox: numbers, rotate, fills })
  }
  if (!pages.length) throw new Error('no page objects found')
  // page order: the Kids order of the page tree when it can be read, else object order
  const order = []
  const walk = (id, depth) => {
    const object = objects.get(id)
    if (!object || depth > 16) return
    if (/\/Type\s*\/Pages/.test(object.dictionary)) {
      const kids = /\/Kids\s*\[([^\]]*)\]/.exec(object.dictionary)
      if (kids) for (const ref of refsOf(kids[1])) walk(ref, depth + 1)
    } else order.push(id)
  }
  const root = [...objects.entries()].find(([, object]) =>
    /\/Type\s*\/Catalog/.test(object.dictionary),
  )
  const rootPages = root && /\/Pages\s+(\d+)\s+\d+\s+R/.exec(root[1].dictionary)
  if (rootPages) walk(Number(rootPages[1]), 0)
  const byId = new Map(pages.map((page) => [page.id, page]))
  const ordered =
    order.length === pages.length && order.every((id) => byId.has(id))
      ? order.map((id) => byId.get(id))
      : pages
  return { pages: ordered, unicode: unicodeOf(objects) }
}

/** The page's size in points, with its rotation applied. */
export function pageSize(page) {
  if (!page.mediaBox || page.mediaBox.length < 4) return undefined
  const [x0, y0, x1, y1] = page.mediaBox
  const [width, height] = [Math.abs(x1 - x0), Math.abs(y1 - y0)]
  return page.rotate % 180 === 90 ? [height, width] : [width, height]
}

/** The first dark filled square of the page that is no larger than `max` points: the marker
 * the print corpus puts at the top left of its content box. Offsets are from the page's edges. */
export function marker(page, max = 80) {
  if (!page.mediaBox || page.mediaBox.length < 4) return undefined
  const [x0, , , y1] = page.mediaBox
  const fill = page.fills.find(
    ({ bbox, color }) =>
      color.every((channel) => channel < 40) &&
      bbox.x1 - bbox.x0 > 4 &&
      bbox.x1 - bbox.x0 <= max &&
      bbox.y1 - bbox.y0 > 4 &&
      bbox.y1 - bbox.y0 <= max,
  )
  if (!fill) return undefined
  return {
    left: fill.bbox.x0 - x0,
    top: y1 - fill.bbox.y1,
    width: fill.bbox.x1 - fill.bbox.x0,
    height: fill.bbox.y1 - fill.bbox.y0,
  }
}

/** The fills of a page whose color is within `tolerance` per channel of `rgb`. */
export function fillsNear(page, rgb, tolerance = 4) {
  return page.fills.filter(({ color }) =>
    color.every((channel, index) => Math.abs(channel - rgb[index]) <= tolerance),
  )
}
