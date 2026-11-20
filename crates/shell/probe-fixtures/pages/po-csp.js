// The shell pages' own CSP (apps/shell/src/renderer/index.html) plus the probe sink: no
// `unsafe-eval`, no inline script. Page operations must still run on it.
window.__probeLoaded = true
P.log('po.csp.load', {})
P.done({})
