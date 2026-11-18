// Runs in every page of the bundle (serve.mjs adds it to each index.html). The host page cannot
// read a cross-origin frame, so the frame measures itself and posts to its own origin.
;(() => {
  const frame = location.pathname
  const DOC = '.ProseMirror, .textLayer, .reading-view, [contenteditable="true"]'

  /** what the page is asked to try in place, by the phase the collector names */
  const VARIANTS = {
    'as-shown': () => '',
    'adjust-100': () => 'html{-webkit-text-size-adjust:100%;text-size-adjust:100%}',
    'adjust-none': () => 'html{-webkit-text-size-adjust:none;text-size-adjust:none}',
    // the page's zoom as the percentage, on the element that carries the zoom
    'adjust-zoom': () =>
      `html{-webkit-text-size-adjust:100%}.doc-zoom{-webkit-text-size-adjust:${docZoom() * 100}%}`,
  }

  const post = (body) =>
    fetch('/__report', { method: 'POST', body: JSON.stringify({ frame, ...body }) }).catch(() => {})
  const where = (el) => {
    const parts = []
    for (let e = el, i = 0; e && e !== document.body && i < 4; e = e.parentElement, i++) {
      const cls = typeof e.className === 'string' ? e.className.trim().split(/\s+/).slice(0, 2) : []
      parts.unshift(e.tagName.toLowerCase() + (e.id ? `#${e.id}` : '') + cls.map((c) => `.${c}`).join(''))
    }
    return parts.join('>')
  }
  /** the docs page scales its paper with CSS zoom on this element */
  const docZoom = () => {
    const el = document.querySelector('.doc-zoom')
    return el ? parseFloat(getComputedStyle(el).zoom) || 1 : 1
  }
  const env = () => ({
    dpr: devicePixelRatio,
    visualViewport: window.visualViewport
      ? { width: visualViewport.width, height: visualViewport.height, scale: visualViewport.scale }
      : null,
    inner: [innerWidth, innerHeight],
    screen: [screen.width, screen.height],
    htmlTextSizeAdjust: getComputedStyle(document.documentElement).webkitTextSizeAdjust ?? null,
    htmlTextSizeAdjustStd: getComputedStyle(document.documentElement).textSizeAdjust ?? null,
    docZoom: docZoom(),
    // "3/12" in the status bar: how many pages the page laid the document out on
    pages: document.body.innerText.match(/\b\d+\s*\/\s*\d+\b/)?.[0] ?? null,
    ua: navigator.userAgent,
  })

  /** every text node a person can see, with the font size the engine used for it */
  function textNodes() {
    const out = []
    const walker = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT)
    const range = document.createRange()
    for (let node = walker.nextNode(); node; node = walker.nextNode()) {
      const text = node.textContent.trim()
      const el = node.parentElement
      if (!text || !el) continue
      range.selectNodeContents(node)
      const rects = range.getClientRects()
      if (!rects.length || !(rects[0].width > 0)) continue
      const style = getComputedStyle(el)
      if (style.visibility === 'hidden') continue
      const block = el.closest('p, li, td, th, h1, h2, h3, h4, div') ?? el
      out.push({
        text,
        region: el.closest(DOC) ? 'doc' : 'ui',
        fontSize: parseFloat(style.fontSize),
        lineHeight: style.lineHeight,
        adjust: style.webkitTextSizeAdjust ?? null,
        editable: el.isContentEditable,
        rectHeight: Math.round(rects[0].height * 10) / 10,
        lines: new Set([...rects].map((r) => Math.round(r.top))).size,
        blockWidth: Math.round(block.getBoundingClientRect().width),
        where: where(el),
      })
    }
    return out
  }

  /** one row per region and font size */
  function groups(nodes) {
    const map = new Map()
    for (const n of nodes) {
      const key = `${n.region}|${n.fontSize}`
      let g = map.get(key)
      if (!g) {
        const { text, ...first } = n
        map.set(key, (g = { ...first, chars: 0, nodes: 0, sample: text.slice(0, 40) }))
      }
      g.chars += n.text.length
      g.nodes++
    }
    return [...map.values()].sort((a, b) => b.chars - a.chars)
  }

  // the host names the frame it embeds after its case; a frame inside it (the hwp studio) asks up
  const mine = (() => {
    try {
      let w = window
      while (w.parent !== w.top) w = w.parent
      return w === w.top ? '' : w.name
    } catch {
      return ''
    }
  })()
  let done = 'loading'
  let busy = false
  async function tick() {
    if (busy) return
    let state
    try {
      state = await (await fetch('/__phase', { cache: 'no-store' })).json()
    } catch {
      return
    }
    // a tab of an earlier case keeps running in the background: it must stay silent
    if (!mine || state.case !== mine || state.phase === done || !VARIANTS[state.phase]) return
    busy = true
    done = state.phase
    document.getElementById('__probe_variant')?.remove()
    const css = VARIANTS[state.phase]()
    if (css) {
      const style = document.createElement('style')
      style.id = '__probe_variant'
      style.textContent = css
      document.head.appendChild(style)
    }
    // the page lays the document out again (pagination follows the sizes): read it once it rests
    let nodes = []
    let last = ''
    for (let i = 0; i < 20; i++) {
      await new Promise((r) => setTimeout(r, 1000))
      nodes = textNodes()
      const now = nodes.map((n) => `${n.fontSize}:${n.lines}:${n.text.length}`).join()
      if (now === last) break
      last = now
    }
    post({ case: mine, phase: state.phase, css, env: env(), groups: groups(nodes) })
    busy = false
  }
  setInterval(tick, 400)
})()
