// The host page of the repro: embeds one document from the bundle's origin and tells the collector
// when it opened. `SeuofficeEmbed` is packages/embed built as one classic script.
;(async () => {
  const q = new URLSearchParams(location.search)
  const env = () => ({
    dpr: devicePixelRatio,
    visualViewport: window.visualViewport
      ? { width: visualViewport.width, height: visualViewport.height, scale: visualViewport.scale }
      : null,
    inner: [innerWidth, innerHeight],
    screen: [screen.width, screen.height],
    ua: navigator.userAgent,
  })
  const report = (body) =>
    fetch('/__report', {
      method: 'POST',
      body: JSON.stringify({ frame: 'host', case: q.get('case'), ...body }),
    })
  try {
    const fileName = q.get('file')
    const bytes = new Uint8Array(await (await fetch(`/files/${fileName}`)).arrayBuffer())
    const viewer = document.getElementById('viewer')
    // the frame's name tells probe.js which case it belongs to; WebKit reads it only at insertion
    const append = viewer.appendChild.bind(viewer)
    viewer.appendChild = (node) => {
      if (node instanceof HTMLIFrameElement) node.name = q.get('case')
      return append(node)
    }
    const base = { baseUrl: q.get('bundle'), bytes, fileName }
    const doc =
      q.get('mode') === 'edit'
        ? SeuofficeEmbed.openDocument(viewer, { ...base, mode: 'edit', onSave: async () => {} })
        : SeuofficeEmbed.openDocument(viewer, base)
    await doc.opened
    const rect = doc.iframe.getBoundingClientRect()
    await report({ event: 'opened', env: env(), iframe: [rect.width, rect.height] })
  } catch (error) {
    await report({ event: 'error', message: String(error?.code ?? error?.message ?? error), env: env() })
  }
})()
