/**
 * An HTML tree painted on a canvas through an SVG `<foreignObject>`. An SVG drawn as an image
 * reads nothing from outside itself, so the tree takes along its stylesheet rules, its font
 * files and its pictures, all as data URLs. The SVG itself is a data URL too: both Chromium
 * and WebKit taint a canvas that drew a `<foreignObject>` from a blob URL.
 */

const XHTML = 'http://www.w3.org/1999/xhtml'

/** What XML 1.0 cannot hold, lone surrogates included: one such character makes the SVG unreadable */
const NOT_XML = /[^\t\n\r\x20-\uD7FF\uE000-\uFFFD\u{10000}-\u{10FFFF}]/gu

const CSS_URL = /url\(\s*(["']?)([^"')]+)\1\s*\)/g

/**
 * Font files one picture takes along at most, as data URL characters. A family whose faces do
 * not fit is left out whole, and its text is set in the next family of its list.
 */
const FONT_BUDGET = 6 * 1024 * 1024

/** How long a painting is given before it is compared with the next one, and how many times */
const SETTLE_MS = 50
const SETTLE_TRIES = 8

/** Font files already read, by address: a second picture of the same document reads none again */
const fontFiles = new Map<string, Promise<string | null>>()

function dataUrlOf(blob: Blob): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader()
    reader.onload = () => resolve(String(reader.result))
    reader.onerror = () => reject(reader.error ?? new Error('the file could not be read'))
    reader.readAsDataURL(blob)
  })
}

/** The file at `url` as a data URL; null when it cannot be read, and what refers to it stays as it was */
async function fetchedDataUrl(url: string): Promise<string | null> {
  try {
    const response = await fetch(url)
    return response.ok ? await dataUrlOf(await response.blob()) : null
  } catch {
    return null
  }
}

/** Every picture the tree shows, held by the tree itself */
async function inlinePictures(tree: HTMLElement, signal: AbortSignal): Promise<void> {
  const jobs: Promise<void>[] = []
  const refer = (element: Element, attribute: string): void => {
    const url = element.getAttribute(attribute)
    if (!url || url.startsWith('data:')) return
    jobs.push(
      fetchedDataUrl(new URL(url, document.baseURI).href).then((data) => {
        if (data) element.setAttribute(attribute, data)
      }),
    )
  }
  for (const img of Array.from(tree.querySelectorAll('img'))) {
    // the one file the SVG can hold: the candidates of a srcset are other addresses
    img.removeAttribute('srcset')
    img.removeAttribute('loading')
    refer(img, 'src')
  }
  for (const image of Array.from(tree.querySelectorAll('image'))) {
    refer(image, image.hasAttribute('href') ? 'href' : 'xlink:href')
  }
  await Promise.all(jobs)
  signal.throwIfAborted()
}

/** A family as a stylesheet and a computed style both name it: no quotes, no case */
export const fontFamilyKey = (name: string): string =>
  name
    .trim()
    .replace(/^["']|["']$/g, '')
    .toLowerCase()

/** One face as the page names it: the family, weight, style and the characters it covers */
function faceKey(family: string, weight: string, style: string, range: string): string {
  const numeric = { normal: '400', bold: '700' }[weight.trim()] ?? (weight.trim() || '400')
  const covered = range.replace(/\s+/g, '').toUpperCase() || 'U+0-10FFFF'
  return [fontFamilyKey(family), numeric, style.trim() || 'normal', covered].join('|')
}

/** Whether a `unicode-range` descriptor covers one of `characters` (code points); none covers all */
export function rangeCovers(range: string, characters: ReadonlySet<number>): boolean {
  const spans = range
    .split(',')
    .map((span) => span.trim().replace(/^u\+/i, ''))
    .filter(Boolean)
  if (spans.length === 0) return true
  return spans.some((span) => {
    const [from = '', to = from] = span.split('-')
    const low = Number.parseInt(from.replaceAll('?', '0'), 16)
    const high = Number.parseInt(to.replaceAll('?', 'F'), 16)
    for (const character of characters) if (character >= low && character <= high) return true
    return false
  })
}

/** A `@font-face` rule with its font files inside it */
async function inlineFontFace(rule: CSSFontFaceRule): Promise<string> {
  const base = rule.parentStyleSheet?.href ?? document.baseURI
  const files = new Map<string, string | null>()
  for (const [, , url] of rule.cssText.matchAll(CSS_URL)) {
    if (url === undefined || url.startsWith('data:') || files.has(url)) continue
    const address = new URL(url, base).href
    let file = fontFiles.get(address)
    if (!file) {
      file = fetchedDataUrl(address)
      fontFiles.set(address, file)
    }
    files.set(url, await file)
  }
  return rule.cssText.replace(CSS_URL, (whole, _quote: string, url: string) => {
    const file = files.get(url)
    return file ? `url("${file}")` : whole
  })
}

export interface PageCssNeeds {
  /** the families the tree's text is set in (`fontFamilyKey`), the ones that matter most first */
  families: ReadonlySet<string>
  /** the tree's text */
  text: string
  signal: AbortSignal
}

/**
 * The rules of the page's stylesheets as one text, for a tree that is laid out like the page.
 * A page declares hundreds of `@font-face` rules. Only the faces the tree needs come along,
 * each with its file: a face the page has loaded, of a family the tree names, that covers a
 * character of its text.
 */
export async function pageCss({ families, text, signal }: PageCssNeeds): Promise<string> {
  const characters = new Set(Array.from(text, (character) => character.codePointAt(0)!))
  const loaded = new Set<string>()
  for (const face of document.fonts) {
    if (face.status === 'loaded') {
      loaded.add(faceKey(face.family, face.weight, face.style, face.unicodeRange))
    }
  }
  const rules: string[] = []
  const faces = new Map<string, Promise<string>[]>()
  const collect = (list: CSSRuleList): void => {
    for (const rule of Array.from(list)) {
      if (rule instanceof CSSFontFaceRule) {
        const { style } = rule
        const family = fontFamilyKey(style.getPropertyValue('font-family'))
        const range = style.getPropertyValue('unicode-range')
        const key = faceKey(
          family,
          style.getPropertyValue('font-weight'),
          style.getPropertyValue('font-style'),
          range,
        )
        if (families.has(family) && loaded.has(key) && rangeCovers(range, characters)) {
          faces.set(family, [...(faces.get(family) ?? []), inlineFontFace(rule)])
        }
      } else if (rule instanceof CSSImportRule) {
        if (rule.styleSheet) collect(rule.styleSheet.cssRules)
      } else rules.push(rule.cssText)
    }
  }
  for (const sheet of Array.from(document.styleSheets)) {
    try {
      collect(sheet.cssRules)
    } catch {
      // a stylesheet of another origin keeps its rules to itself
    }
  }
  const inlined: string[] = []
  let left = FONT_BUDGET
  for (const family of families) {
    const ofFamily = await Promise.all(faces.get(family) ?? [])
    const size = ofFamily.reduce((sum, face) => sum + face.length, 0)
    if (size > left) continue
    left -= size
    inlined.push(...ofFamily)
  }
  signal.throwIfAborted()
  return [...inlined, ...rules].join('\n')
}

export interface HtmlPicture {
  /** the tree to paint; it is changed (its pictures become data URLs) and must not be on the page */
  tree: HTMLElement
  /** every rule the tree is laid out under: nothing of the page's own stylesheets applies */
  css: string
  /** the size the tree is laid out in, in CSS pixels */
  width: number
  height: number
  /** the canvas, in its own pixels: the layout is scaled to it */
  canvasWidth: number
  canvasHeight: number
  signal: AbortSignal
}

/** `tree` laid out `width` by `height` under `css`, on a canvas of its own */
export async function paintHtml({
  tree,
  css,
  width,
  height,
  canvasWidth,
  canvasHeight,
  signal,
}: HtmlPicture): Promise<HTMLCanvasElement> {
  await inlinePictures(tree, signal)
  const frame = document.createElementNS(XHTML, 'div')
  frame.setAttribute(
    'style',
    `position:relative;width:${width}px;height:${height}px;overflow:hidden;background:#fff`,
  )
  const style = document.createElementNS(XHTML, 'style')
  style.textContent = css
  frame.append(style, tree)
  const markup = new XMLSerializer().serializeToString(frame).replace(NOT_XML, '')
  const svg =
    `<svg xmlns="http://www.w3.org/2000/svg" width="${width}" height="${height}">` +
    `<foreignObject width="100%" height="100%">${markup}</foreignObject></svg>`
  const image = new Image()
  image.src = `data:image/svg+xml;charset=utf-8,${encodeURIComponent(svg)}`
  await image.decode()
  signal.throwIfAborted()
  const canvas = document.createElement('canvas')
  canvas.width = canvasWidth
  canvas.height = canvasHeight
  const context = canvas.getContext('2d', { willReadFrequently: true })
  if (!context) throw new Error('this page cannot draw')
  const paint = (): Uint8ClampedArray => {
    context.clearRect(0, 0, canvasWidth, canvasHeight)
    context.drawImage(image, 0, 0, canvasWidth, (height * canvasWidth) / width)
    return context.getImageData(0, 0, canvasWidth, canvasHeight).data
  }
  // Chromium starts loading an SVG's fonts when it first paints it, and leaves their text out
  // until they are in. The first painting only starts that; the picture is the first painting
  // that comes out the same as the one before it.
  paint()
  let before: Uint8ClampedArray | undefined
  for (let tries = 0; tries < SETTLE_TRIES; tries++) {
    await new Promise((resolve) => setTimeout(resolve, SETTLE_MS))
    signal.throwIfAborted()
    const after = paint()
    const earlier = before
    if (earlier && after.every((value, index) => value === earlier[index])) break
    before = after
  }
  return canvas
}
