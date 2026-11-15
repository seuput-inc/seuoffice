'use strict'

const engine = new URLSearchParams(location.search).get('engine')
window.__probeOutcome = { status: 'running' }
const frame = document.getElementById('editor')
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms))
const post = (message, transfer = []) =>
  frame.contentWindow.postMessage(
    { source: 'seuoffice', version: 1, ...message },
    location.origin,
    transfer,
  )
const pending = new Set()
let savedBytes = null
let failure = null
window.addEventListener('message', (event) => {
  if (event.origin !== location.origin || event.source !== frame.contentWindow) return
  const data = event.data
  if (!data || data.source !== 'seuoffice' || data.version !== 1) return
  if (data.type === 'error') failure = new Error(`editor ${data.code}: ${data.message}`)
  if (data.type === 'save') {
    if (!(data.bytes instanceof ArrayBuffer) || typeof data.saveId !== 'number') {
      failure = new Error('invalid save message')
      return
    }
    savedBytes = new Uint8Array(data.bytes)
    post({ type: 'saved', saveId: data.saveId })
  }
  for (const listener of [...pending]) listener(data)
})
function message(predicate) {
  return new Promise((resolve, reject) => {
    const listener = (data) => {
      if (data.type !== 'error' && !predicate(data)) return
      pending.delete(listener)
      if (data.type === 'error') reject(failure)
      else resolve(data)
    }
    pending.add(listener)
  })
}
const sha256 = async (bytes) =>
  [...new Uint8Array(await crypto.subtle.digest('SHA-256', bytes))]
    .map((n) => n.toString(16).padStart(2, '0'))
    .join('')
function base64(bytes) {
  let text = ''
  for (let i = 0; i < bytes.length; i += 32768)
    text += String.fromCharCode(...bytes.subarray(i, i + 32768))
  return btoa(text)
}
function geometry(element) {
  const doc = element.ownerDocument
  const style = doc.defaultView.getComputedStyle(element)
  const canvas = doc.createElement('canvas').getContext('2d')
  canvas.font = style.font
  if ('fontKerning' in canvas) canvas.fontKerning = style.fontKerning
  const walker = doc.createTreeWalker(element, NodeFilter.SHOW_TEXT)
  const chars = []
  let node,
    offset = 0,
    previousTop = null
  const lineStarts = []
  while ((node = walker.nextNode())) {
    for (let i = 0; i < node.length;) {
      const length = node.data.codePointAt(i) > 0xffff ? 2 : 1
      const range = doc.createRange()
      range.setStart(node, i)
      range.setEnd(node, i + length)
      const rects = [...range.getClientRects()].map((r) => ({
        x: r.x,
        y: r.y,
        width: r.width,
        height: r.height,
      }))
      if (rects.length && (previousTop === null || Math.abs(rects[0].y - previousTop) > 1)) {
        lineStarts.push(offset + i)
        previousTop = rects[0].y
      }
      chars.push({ offset: offset + i, text: node.data.slice(i, i + length), rects })
      i += length
    }
    offset += node.length
  }
  return {
    text: element.textContent,
    canvasWidth: canvas.measureText(element.textContent).width,
    chars,
    lineStarts,
    computed: {
      font: style.font,
      fontFamily: style.fontFamily,
      fontKerning: style.fontKerning,
      letterSpacing: style.letterSpacing,
      wordSpacing: style.wordSpacing,
      width: style.width,
      lineHeight: style.lineHeight,
      whiteSpace: style.whiteSpace,
      wordBreak: style.wordBreak,
      overflowWrap: style.overflowWrap,
    },
  }
}
async function settle() {
  const doc = frame.contentDocument
  await doc.fonts.ready
  let last = '',
    count = 0
  for (;;) {
    if (failure) throw failure
    const debug = frame.contentWindow.__pageDebug
    const sig = debug && debug.pageCutSig
    if (
      typeof sig === 'string' &&
      sig.length &&
      doc.body.textContent.includes('SYSTEM-WEBVIEW-LAST')
    ) {
      count = sig === last ? count + 1 : 1
      last = sig
      if (count >= 3) return debug
    } else {
      last = ''
      count = 0
    }
    await sleep(300)
  }
}
async function main() {
  if (!['electron', 'wkwebview'].includes(engine)) throw new Error('invalid engine')
  const response = await fetch('/probe/fixture.docx')
  if (!response.ok) throw new Error(`fixture HTTP ${response.status}`)
  const bytes = await response.arrayBuffer()
  const input = {
    fileName: 'fixture.docx',
    byteLength: bytes.byteLength,
    sha256: await sha256(bytes),
  }
  const ready = message((data) => data.type === 'ready')
  frame.src = '/docs/index.html?mode=edit&lang=ko'
  await ready
  const opened = message((data) => data.type === 'opened')
  post({ type: 'open', fileName: input.fileName, bytes, ai: false, collab: false }, [bytes])
  await opened
  await settle()
  const doc = frame.contentDocument
  const win = frame.contentWindow
  const fontCases = [
    ['SeuOffice Sans GO', 'Latin ABC'],
    ['SeuOffice Sans KR', '한글 가나다'],
  ]
  const fonts = []
  for (const [family, text] of fontCases) {
    const spec = `16px "${family}"`
    const loaded = await doc.fonts.load(spec, text)
    const checked = doc.fonts.check(spec, text)
    if (!loaded.length || !checked || loaded.some((face) => face.status !== 'loaded'))
      throw new Error(`required font failed: ${family}`)
    fonts.push({
      family,
      text,
      loaded: loaded.length,
      checked,
      statuses: loaded.map((face) => face.status),
    })
  }
  await doc.fonts.ready
  const debug = await settle()
  if (debug.loopStops || debug.sliceIterationsCapped || debug.sliceRunaway) {
    throw new Error(
      `pagination did not settle normally: ${JSON.stringify({ loopStops: debug.loopStops, iterationsCapped: debug.sliceIterationsCapped, runaway: debug.sliceRunaway, pageCutSig: debug.pageCutSig })}`,
    )
  }
  const requests = win.performance
    .getEntriesByType('resource')
    .map((entry) => entry.name)
    .filter((name) => /\.(woff2?|ttf|otf)(?:[?#]|$)/i.test(name))
  const markers = [
    'SYSTEM-WEBVIEW-FIRST',
    'SYSTEM-WEBVIEW-GO',
    'SYSTEM-WEBVIEW-KR',
    'SYSTEM-WEBVIEW-FALLBACK',
  ]
  const editor = doc.querySelector('[contenteditable="true"]')
  if (!editor) throw new Error('editable missing')
  const paragraphs = [...editor.querySelectorAll('p')]
  const editorCases = markers.map((marker) => {
    const paragraph = paragraphs.find((p) => p.textContent.includes(marker))
    if (!paragraph) throw new Error(`missing paragraph ${marker}`)
    return { marker, ...geometry(paragraph) }
  })
  const raw = doc.createElement('div')
  raw.style.cssText = 'position:fixed;left:-10000px;top:0;visibility:hidden;'
  doc.body.appendChild(raw)
  const rawCases = []
  for (const [family, text] of [
    ...fontCases,
    ['Calibri, "SeuOffice Sans GO", "SeuOffice Sans KR", sans-serif', '한글 Latin, (가나다)! ABC'],
  ]) {
    for (const width of [159, 160, 161]) {
      const element = doc.createElement('div')
      element.style.cssText = `font: 16px ${family.includes(',') ? family : `"${family}"`};width:${width}px;white-space:normal;overflow-wrap:normal;`
      element.textContent = text.repeat(4)
      raw.appendChild(element)
      rawCases.push({ family, width, ...geometry(element) })
    }
  }
  raw.remove()
  const layout = {
    pageCuts: debug.pageCuts,
    pageCutSig: debug.pageCutSig,
    slices: debug.slices,
    loopStops: debug.loopStops,
    iterationsCapped: debug.sliceIterationsCapped,
    runaway: debug.sliceRunaway,
  }
  const cases = {
    fonts,
    fontResourceUrls: requests,
    cssSupports: {
      fontKerning: win.CSS.supports('font-kerning', 'normal'),
      overflowWrap: win.CSS.supports('overflow-wrap', 'anywhere'),
    },
    raw: rawCases,
    editor: editorCases,
  }
  const first = paragraphs.find((p) => p.textContent.includes('SYSTEM-WEBVIEW-FIRST'))
  const walker = doc.createTreeWalker(first, NodeFilter.SHOW_TEXT)
  let node
  while ((node = walker.nextNode()) && !node.data.includes('SYSTEM-WEBVIEW-FIRST')) {}
  if (!node) throw new Error('edit target missing')
  const selection = win.getSelection()
  const range = doc.createRange()
  range.setStart(node, node.data.indexOf('SYSTEM-WEBVIEW-FIRST') + 'SYSTEM-WEBVIEW-FIRST'.length)
  range.collapse(true)
  editor.focus()
  selection.removeAllRanges()
  selection.addRange(range)
  const marker = 'SYSTEM-WEBVIEW-EDIT-20261116'
  const inserted = doc.execCommand('insertText', false, marker)
  if (!inserted || !editor.textContent.includes(marker)) throw new Error('insertText failed')
  await sleep(500)
  const done = message((data) => data.type === 'request_save_done' && data.requestId === 1)
  post({ type: 'request_save', requestId: 1 })
  const result = await done
  if (!result.ok || !savedBytes || !savedBytes.length)
    throw new Error(`save failed: ${result.message || 'missing bytes'}`)
  const editSave = {
    marker,
    inserted,
    savedBase64: base64(savedBytes),
    savedSha256: await sha256(savedBytes),
    savedBytes: savedBytes.length,
  }
  const editedDebug = await settle()
  if (editedDebug.loopStops || editedDebug.sliceIterationsCapped || editedDebug.sliceRunaway) {
    throw new Error(
      `edited pagination did not settle normally: ${JSON.stringify({ loopStops: editedDebug.loopStops, iterationsCapped: editedDebug.sliceIterationsCapped, runaway: editedDebug.sliceRunaway, pageCutSig: editedDebug.pageCutSig })}`,
    )
  }
  const editedLayout = {
    pageCuts: editedDebug.pageCuts,
    pageCutSig: editedDebug.pageCutSig,
    slices: editedDebug.slices,
    loopStops: editedDebug.loopStops,
    iterationsCapped: editedDebug.sliceIterationsCapped,
    runaway: editedDebug.sliceRunaway,
  }
  window.__prepareProbePrint = async () => {
    const source = frame.contentDocument
    const css = []
    for (const sheet of source.styleSheets) {
      try {
        for (const rule of sheet.cssRules)
          css.push(
            rule.cssText.replace(
              /url\(\s*(['"]?)([^'")]+)\1\s*\)/g,
              (match, quote, url) => `url("${new URL(url, sheet.href || source.baseURI).href}")`,
            ),
          )
      } catch (error) {
        throw new Error(`cannot clone stylesheet: ${error}`)
      }
    }
    const stylesheetCount = source.styleSheets.length
    const base = document.createElement('base')
    base.href = source.baseURI
    const style = document.createElement('style')
    style.textContent =
      css.join('\n') +
      '\n@page { size: A4; margin: 0 } html, body { overflow: visible !important; height: auto !important }'
    document.head.replaceChildren(base, style)
    document.documentElement.className = source.documentElement.className
    document.body.className = source.body.className
    document.body.replaceChildren(
      ...[...source.body.childNodes].map((child) => document.importNode(child, true)),
    )
    await document.fonts.ready
    await new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve)))
    return JSON.stringify({
      mode: 'diagnostic-cloned-dom',
      paper: 'A4',
      margins: 0,
      stylesheetCount,
      limits:
        'Cloned editor DOM and CSS; not the application PDF consumer. Native print pagination and platform fonts may differ.',
    })
  }
  window.__probeOutcome = {
    status: 'complete',
    engine,
    input,
    layout,
    editedLayout,
    cases,
    editSave,
  }
}
main().catch((error) => {
  window.__probeOutcome = {
    status: 'failed',
    engine,
    error: String((error && error.stack) || error),
  }
})
