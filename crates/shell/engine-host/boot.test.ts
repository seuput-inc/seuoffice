import { afterEach, describe, expect, it, vi } from 'vitest'

type Listener = (event: unknown) => void

const FAILED_FETCH = Object.assign(new TypeError('Failed to fetch'), { stack: '' })

/** A page whose events the test fires. `fetch` answers like Tauri's ipc protocol does a GET. */
function stubPage({ bridge = true, ipcReachable = true } = {}) {
  const listeners = new Map<string, Listener[]>()
  const events = {
    addEventListener(type: string, listener: Listener) {
      listeners.set(type, [...(listeners.get(type) ?? []), listener])
    },
  }
  const window = {
    ...events,
    __TAURI_INTERNALS__: bridge
      ? { convertFileSrc: (path: string, protocol: string) => `${protocol}://localhost/${path}` }
      : {},
    __SEUOFFICE_ENGINE_TOKEN__: 'secret-token',
    __seuofficeCheck: () => true,
    crossOriginIsolated: true,
    __seuofficeEngineLog: undefined as undefined | ((message: string) => void),
  }
  const fetch = vi.fn((url: string) =>
    url.startsWith('ipc:') && !ipcReachable
      ? Promise.reject(FAILED_FETCH)
      : Promise.resolve({ status: 405 }),
  )
  vi.stubGlobal('window', window)
  vi.stubGlobal('document', events)
  vi.stubGlobal('location', { href: 'genoffice-app-engine-host://localhost/index.html' })
  vi.stubGlobal('navigator', { userAgent: 'test-agent' })
  vi.stubGlobal('fetch', fetch)
  return {
    window,
    fire: (type: string, event: object) => listeners.get(type)?.forEach((fn) => fn(event)),
    /** what the shell was told, as its trace shows it */
    reports: () =>
      fetch.mock.calls
        .map(([url]) => new URL(url, 'http://page'))
        .filter((url) => url.pathname === '/__engine-host-log')
        .map((url) => (url.searchParams.get('m') ?? '').replace(/^\+\d+ms /, '')),
  }
}

// boot.ts does its work when it is loaded, so each page is stubbed first and the module loaded
// afresh: a static import could not run it more than once
const boot = async () => {
  vi.resetModules()
  await import('./boot')
  // the probe's answer is a promise the stub handed out: boot.ts's handlers on it run first
  await Promise.allSettled(vi.mocked(fetch).mock.results.map((result) => result.value))
}

afterEach(() => vi.unstubAllGlobals())

describe('the engine page boot script', () => {
  it('tells the shell the state of the bridge when it starts, never the token', async () => {
    const page = stubPage()
    await boot()
    expect(page.reports()[0]).toBe(
      'boot genoffice-app-engine-host://localhost/index.html; ' +
        'tauri internals [convertFileSrc]; bridge function; token present; isolated true; test-agent',
    )
    expect(page.reports().join('\n')).not.toContain('secret-token')
  })

  it('tells the shell whether a fetch reaches the ipc protocol', async () => {
    const reached = stubPage()
    await boot()
    expect(reached.reports()).toContain('ipc probe ipc://localhost/engine_host_probe: HTTP 405')
    vi.unstubAllGlobals()

    const blocked = stubPage({ ipcReachable: false })
    await boot()
    expect(blocked.reports()).toContain(
      'ipc probe ipc://localhost/engine_host_probe: TypeError: Failed to fetch',
    )
    vi.unstubAllGlobals()

    const missing = stubPage({ bridge: false })
    await boot()
    expect(missing.reports()).toContain(
      'ipc probe skipped: __TAURI_INTERNALS__.convertFileSrc is missing',
    )
  })

  it('reports what keeps the page from becoming ready', async () => {
    const page = stubPage()
    await boot()
    page.fire('error', {
      target: page.window,
      error: Object.assign(new TypeError('x is not a function'), {
        stack: 'TypeError: x is not a function\n    at run (main.js:9:9)',
      }),
      message: 'x is not a function',
      filename: 'main.js',
      lineno: 3,
      colno: 7,
    })
    page.fire('error', {
      target: { tagName: 'SCRIPT', src: 'genoffice-app-engine-host://localhost/main.js' },
    })
    page.fire('unhandledrejection', { reason: 'native document context is unavailable' })
    page.fire('securitypolicyviolation', {
      effectiveDirective: 'connect-src',
      blockedURI: 'ipc://localhost/engine_host_ready',
      sourceFile: 'main.js',
      lineNumber: 1,
      columnNumber: 2,
    })
    const reports = page.reports()
    expect(reports).toContain(
      'uncaught TypeError: x is not a function (at run (main.js:9:9)) at main.js:3:7',
    )
    expect(reports).toContain(
      'failed to load <script> genoffice-app-engine-host://localhost/main.js',
    )
    expect(reports).toContain('unhandled rejection: native document context is unavailable')
    expect(reports).toContain(
      'csp blocked connect-src ipc://localhost/engine_host_ready from main.js:1:2',
    )
  })

  it('lets the bundle report too, up to a cap, and never fails the page', async () => {
    const page = stubPage()
    await boot()
    page.window.__seuofficeEngineLog?.('main: the bundle is evaluated')
    expect(page.reports()).toContain('main: the bundle is evaluated')

    vi.mocked(fetch).mockImplementation(() => Promise.reject(FAILED_FETCH))
    for (let n = 0; n < 100; n++) page.window.__seuofficeEngineLog?.(`more ${n}`)
    await Promise.allSettled(vi.mocked(fetch).mock.results.map((result) => result.value))
    // the boot report, the probe, the bundle's, then every report up to the cap
    expect(page.reports().length).toBe(61)
    expect(page.reports().at(-1)).toBe('more than 60 reports, the rest are dropped')
  })
})
