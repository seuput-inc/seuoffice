// Runs in every page of the bundle (serve.mjs adds it to each index.html). The host page cannot
// read a cross-origin frame, so the frame measures itself and posts to its own origin.
;(() => {
  const frame = location.pathname
  const DOC = '.ProseMirror, .textLayer, .reading-view, [contenteditable="true"]'
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
  const env = () => ({
    dpr: devicePixelRatio,
    visualViewport: window.visualViewport
      ? { width: visualViewport.width, height: visualViewport.height, scale: visualViewport.scale }
      : null,
    inner: [innerWidth, innerHeight],
    client: [document.documentElement.clientWidth, document.documentElement.clientHeight],
    screen: [screen.width, screen.height],
    htmlTextSizeAdjust: getComputedStyle(document.documentElement).webkitTextSizeAdjust ?? null,
    htmlTextSizeAdjustStd: getComputedStyle(document.documentElement).textSizeAdjust ?? null,
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
        el,
        text,
        region: el.closest(DOC) ? 'doc' : 'ui',
        fontSize: parseFloat(style.fontSize),
        lineHeight: style.lineHeight,
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
      if (!g)
        map.set(
          key,
          (g = {
            region: n.region,
            fontSize: n.fontSize,
            chars: 0,
            nodes: 0,
            sample: n.text.slice(0, 40),
            where: n.where,
            lineHeight: n.lineHeight,
            rectHeight: n.rectHeight,
            lines: n.lines,
            blockWidth: n.blockWidth,
          }),
        )
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
  let done = ''
  let measured = []
  async function tick() {
    let state
    try {
      state = await (await fetch('/__phase', { cache: 'no-store' })).json()
    } catch {
      return
    }
    // a tab of an earlier case keeps running in the background: it must stay silent
    if (!mine || state.case !== mine || state.phase === done) return
    if (state.phase === 'measure') {
      done = 'measure'
      measured = textNodes()
      post({ case: mine, phase: 'measure', env: env(), groups: groups(measured) })
    } else if (state.phase === 'force') {
      done = 'force'
      // the hypothesis, tested in place: with autosizing off the same nodes go back to their own size
      document.documentElement.style.setProperty('-webkit-text-size-adjust', '100%')
      document.documentElement.style.setProperty('text-size-adjust', '100%')
      await new Promise((r) => setTimeout(r, 1500))
      const changed = new Map()
      for (const n of measured) {
        if (!n.el.isConnected) continue
        const now = parseFloat(getComputedStyle(n.el).fontSize)
        if (Math.abs(now - n.fontSize) < 0.05) continue
        const key = `${n.region}|${n.fontSize}|${now}`
        let c = changed.get(key)
        if (!c)
          changed.set(
            key,
            (c = { region: n.region, from: n.fontSize, to: now, chars: 0, nodes: 0, sample: n.text.slice(0, 40), where: n.where }),
          )
        c.chars += n.text.length
        c.nodes++
      }
      post({
        case: mine,
        phase: 'force',
        env: env(),
        groups: groups(textNodes()),
        changed: [...changed.values()].sort((a, b) => b.chars - a.chars),
      })
    }
  }
  setInterval(tick, 400)
})()
