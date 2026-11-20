// The meta CSP of every renderer page the shell serves must let the bridge reach Tauri's IPC and
// let the page use the custom schemes it names, in both platform shapes (WKWebView
// `<scheme>://localhost`, WebView2 `http://<scheme>.localhost`). No app needed.
//   cd crates/shell/ci && npx vitest run renderer-csp.test.mjs

import fs from 'node:fs'
import path from 'node:path'
import { describe, expect, it } from 'vitest'

const appsDir = path.resolve(import.meta.dirname, '../../../apps')

// Pages of `app_scheme::hosts()` other than the engine host, which has its own CSP.
const pages = fs
  .readdirSync(appsDir)
  .map((app) => path.join(appsDir, app, 'src', 'renderer'))
  .filter((dir) => fs.existsSync(dir))
  .flatMap((dir) =>
    fs
      .readdirSync(dir)
      .filter((name) => name.endsWith('.html'))
      .map((name) => path.join(dir, name)),
  )

function policyOf(file) {
  const html = fs.readFileSync(file, 'utf8')
  const match = html.match(/http-equiv="Content-Security-Policy"\s+content="([^"]*)"/)
  if (!match) return null
  const directives = new Map()
  for (const part of match[1].split(';')) {
    const [name, ...sources] = part.trim().split(/\s+/)
    if (name) directives.set(name, sources)
  }
  return directives
}

/** Whether `directive` (falling back to `default-src`) allows `url`; `'self'` is not modelled. */
function allows(policy, directive, urlText) {
  const sources = policy.get(directive) ?? policy.get('default-src') ?? []
  const url = new URL(urlText)
  return sources.some((source) => {
    if (source.endsWith(':')) return url.protocol === source
    if (source.startsWith("'")) return false
    if (source.includes('*')) return false
    const parsed = new URL(source)
    return parsed.protocol === url.protocol && parsed.host === url.host
  })
}

const IPC_URLS = [
  'ipc://localhost/ipc_invoke',
  'http://ipc.localhost/ipc_invoke',
  'seuoffice-ipc://localhost/outbox/1',
  'http://seuoffice-ipc.localhost/outbox/1',
]

// Schemes the shell registers (main.rs); each is `http://<scheme>.localhost` on Windows.
const TAURI_SCHEMES = [
  'drawio-app',
  'seuoffice-docx-media',
  'html-asset',
  'md-asset',
  'seuoffice-slide-media',
  'html-preview',
  'epub-asset',
]

describe('renderer page CSP', () => {
  it('finds the pages the shell serves', () => {
    const names = pages.map((file) => path.relative(appsDir, file).split(path.sep).join('/'))
    expect(names).toEqual(
      expect.arrayContaining([
        'shell/src/renderer/index.html',
        'shell/src/renderer/update.html',
        'shell/src/renderer/doc-picker.html',
        'shell/src/renderer/quick-look.html',
        'docs/src/renderer/index.html',
        'sheets/src/renderer/index.html',
        'slides/src/renderer/index.html',
        'pdf/src/renderer/index.html',
        'markdown/src/renderer/index.html',
        'html/src/renderer/index.html',
        'mindmap/src/renderer/index.html',
        'hwp/src/renderer/index.html',
        'epub/src/renderer/index.html',
        'diagram/src/renderer/index.html',
      ]),
    )
  })

  describe.each(pages.map((file) => [path.relative(appsDir, file), file]))('%s', (_name, file) => {
    const policy = policyOf(file)

    it('has a meta CSP', () => {
      expect(policy).not.toBeNull()
    })

    it.each(IPC_URLS)('connect-src allows %s', (url) => {
      expect(allows(policy, 'connect-src', url)).toBe(true)
    })

    it('names every custom scheme it uses in its Windows form too', () => {
      for (const [directive, sources] of policy) {
        for (const scheme of TAURI_SCHEMES) {
          if (sources.includes(`${scheme}:`)) {
            expect(sources, `${directive}: ${scheme}`).toContain(`http://${scheme}.localhost`)
          }
        }
      }
    })
  })

  it('the diagram page frames draw.io in both platform shapes', () => {
    const policy = policyOf(path.join(appsDir, 'diagram/src/renderer/index.html'))
    expect(allows(policy, 'frame-src', 'drawio-app://editor/index.html')).toBe(true)
    expect(allows(policy, 'frame-src', 'http://drawio-app.localhost/editor/index.html')).toBe(true)
  })
})
