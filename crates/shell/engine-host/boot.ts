// The engine page's first script: a classic script that runs before the bundle (main.js).
//
// It tells the shell what the page is doing with plain same-origin GETs of REPORT_PATH, which the
// shell answers itself (src/engine_host.rs, `page_report`). Readiness needs the Tauri IPC, and a
// page that cannot reach it (a CSP that blocks it, a bundle that fails to load or to evaluate)
// would otherwise say nothing at all. These reports need none of that, so they are what a failed
// start leaves behind. No document data ever passes through here.
;(function () {
  const REPORT_PATH = '/__engine-host-log'
  const MAX_REPORTS = 60
  const MAX_CHARS = 600
  const started = performance.now()
  let sent = 0

  function encode(text: string): string {
    try {
      return encodeURIComponent(text)
    } catch {
      // a lone surrogate: keep the report, lose the character
      return encodeURIComponent(text.replace(/[^\x20-\x7e]/g, '?'))
    }
  }

  function report(message: string): void {
    if (sent > MAX_REPORTS) return
    sent += 1
    try {
      const text =
        sent > MAX_REPORTS
          ? `more than ${MAX_REPORTS} reports, the rest are dropped`
          : `+${Math.round(performance.now() - started)}ms ${message}`
      fetch(`${REPORT_PATH}?m=${encode(text.slice(0, MAX_CHARS))}`, { cache: 'no-store' }).catch(
        () => {},
      )
    } catch {
      // reporting is best effort and must never be the reason the page fails
    }
  }

  function describe(value: unknown): string {
    try {
      if (value instanceof Error) {
        const frame = (value.stack ?? '').split('\n').find((line) => /:\d+:\d+/.test(line))
        return `${value.name}: ${value.message}${frame ? ` (${frame.trim()})` : ''}`
      }
      return typeof value === 'object' && value !== null ? JSON.stringify(value) : String(value)
    } catch {
      return 'unprintable value'
    }
  }

  window.addEventListener(
    'error',
    (event) => {
      const target = event.target as { tagName?: string; src?: string; href?: string } | null
      if (target && typeof target.tagName === 'string') {
        report(
          `failed to load <${target.tagName.toLowerCase()}> ${target.src ?? target.href ?? ''}`,
        )
        return
      }
      report(
        `uncaught ${describe(event.error ?? event.message)} at ` +
          `${event.filename}:${event.lineno}:${event.colno}`,
      )
    },
    true,
  )
  window.addEventListener('unhandledrejection', (event) => {
    report(`unhandled rejection: ${describe(event.reason)}`)
  })
  document.addEventListener(
    'securitypolicyviolation',
    (event) => {
      const where = event.sourceFile
        ? ` from ${event.sourceFile}:${event.lineNumber}:${event.columnNumber}`
        : ''
      report(`csp blocked ${event.effectiveDirective} ${event.blockedURI || '(inline)'}${where}`)
    },
    true,
  )

  const page = window as unknown as {
    __TAURI_INTERNALS__?: { convertFileSrc?: (path: string, protocol: string) => string }
    __SEUOFFICE_ENGINE_TOKEN__?: unknown
    __seuofficeCheck?: unknown
    __seuofficeEngineLog?: (message: string) => void
  }
  const internals = page.__TAURI_INTERNALS__
  report(
    `boot ${location.href}; ` +
      `tauri internals ${internals ? `[${Object.getOwnPropertyNames(internals).join(',')}]` : 'missing'}; ` +
      `bridge ${typeof page.__seuofficeCheck}; ` +
      `token ${typeof page.__SEUOFFICE_ENGINE_TOKEN__ === 'string' ? 'present' : 'missing'}; ` +
      `isolated ${window.crossOriginIsolated}; ${navigator.userAgent}`,
  )
  page.__seuofficeEngineLog = report

  // Can a fetch leave this page for Tauri's ipc protocol at all? The bridge needs exactly that.
  // The protocol answers a GET itself with 405, running no command and checking nothing, so a 405
  // means it was reached, and a failure (usually the page's CSP, with its own report above) means
  // the bridge cannot work, whatever it says when the invoke later rejects.
  if (internals?.convertFileSrc) {
    const url = internals.convertFileSrc('engine_host_probe', 'ipc')
    fetch(url).then(
      (response) => report(`ipc probe ${url}: HTTP ${response.status}`),
      (error) => report(`ipc probe ${url}: ${describe(error)}`),
    )
  } else {
    report('ipc probe skipped: __TAURI_INTERNALS__.convertFileSrc is missing')
  }
})()
