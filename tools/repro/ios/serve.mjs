/**
 * The two origins of the repro and its collector, in one process.
 *
 * - the bundle (apps/web/dist) on `localhost`, as cross-origin-embed.ts serves it, with probe.js
 *   added to every index.html and the host listed in /embed-origins.json
 * - the host page on `127.0.0.1`, a different site, embedding one document through @seuoffice/embed
 *
 * Both answer /__report (a page posts what it measured) and /__phase (what the pages of the
 * current case should do next).
 */
import { createServer } from 'node:http'
import { readFile } from 'node:fs/promises'
import { extname, join, normalize, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const here = fileURLToPath(new URL('.', import.meta.url))

const MIME = {
  '.html': 'text/html',
  '.js': 'text/javascript',
  '.mjs': 'text/javascript',
  '.css': 'text/css',
  '.json': 'application/json',
  '.wasm': 'application/wasm',
  '.woff2': 'font/woff2',
  '.woff': 'font/woff',
  '.ttf': 'font/ttf',
  '.otf': 'font/otf',
  '.png': 'image/png',
  '.svg': 'image/svg+xml',
  '.txt': 'text/plain',
  '.map': 'application/json',
}

const hostHtml = (viewport) =>
  '<!doctype html><html><head><meta charset="utf-8">' +
  (viewport ? '<meta name="viewport" content="width=device-width, initial-scale=1">' : '') +
  '<title>host</title><style>html,body{margin:0;height:100%}#viewer{height:100vh}</style></head>' +
  '<body><div id="viewer"></div><script src="/embed.js"></script><script src="/host.js"></script></body></html>'

/** `site`: a folder with dist/, embed.js and files/ (build-site.mjs writes it) */
export async function start({ site, hostPort = 5310, bundlePort = 5311, settleMs = 5000 }) {
  const dist = resolve(site, 'dist')
  const state = { case: '', phase: 'loading' }
  const reports = []
  const waiters = new Set()
  const wake = () => waiters.forEach((check) => check())

  async function collector(request, response, path) {
    if (path === '/__phase') {
      response.writeHead(200, { 'content-type': 'application/json', 'cache-control': 'no-store' })
      response.end(JSON.stringify(state))
      return true
    }
    if (path === '/__report') {
      let body = ''
      for await (const chunk of request) body += chunk
      const report = { at: Date.now(), ...JSON.parse(body) }
      reports.push(report)
      console.log(`[report] ${report.case} ${report.frame} ${report.phase ?? report.event}`)
      // the document is on screen: let fonts and the first layout settle, then everyone measures
      if (report.frame === 'host' && report.case === state.case)
        setTimeout(() => {
          if (report.case === state.case && state.phase === 'loading') state.phase = 'as-shown'
        }, report.event === 'opened' ? settleMs : 0)
      response.writeHead(204).end()
      wake()
      return true
    }
    return false
  }

  const bundle = createServer((request, response) => {
    void (async () => {
      const path = decodeURIComponent(new URL(request.url ?? '/', 'http://x').pathname)
      if (await collector(request, response, path)) return
      if (path === '/__probe.js') {
        response.writeHead(200, { 'content-type': 'text/javascript' })
        response.end(await readFile(join(here, 'probe.js')))
        return
      }
      if (path === '/embed-origins.json') {
        response.writeHead(200, { 'content-type': 'application/json' })
        response.end(JSON.stringify({ origins: [`http://127.0.0.1:${hostPort}`] }))
        return
      }
      const file = normalize(join(dist, path.endsWith('/') ? `${path}index.html` : path))
      if (!file.startsWith(dist)) return void response.writeHead(403).end()
      try {
        let body = await readFile(file)
        if (file.endsWith('index.html'))
          body = body.toString().replace(/<head>/i, '<head><script src="/__probe.js"></script>')
        response.writeHead(200, {
          'content-type': MIME[extname(file)] ?? 'application/octet-stream',
          'cache-control': 'no-store',
        })
        response.end(body)
      } catch {
        response.writeHead(404).end('not found')
      }
    })()
  })

  const host = createServer((request, response) => {
    void (async () => {
      const url = new URL(request.url ?? '/', 'http://x')
      const path = decodeURIComponent(url.pathname)
      if (await collector(request, response, path)) return
      try {
        if (path === '/embed.js' || path === '/host.js') {
          const from = path === '/embed.js' ? join(site, 'embed.js') : join(here, 'host.js')
          response.writeHead(200, { 'content-type': 'text/javascript' }).end(await readFile(from))
        } else if (path.startsWith('/files/')) {
          const file = normalize(join(site, 'files', path.slice('/files/'.length)))
          if (!file.startsWith(join(site, 'files'))) return void response.writeHead(403).end()
          response.writeHead(200, { 'content-type': 'application/octet-stream' }).end(await readFile(file))
        } else if (path === '/host.html') {
          response
            .writeHead(200, { 'content-type': 'text/html', 'cache-control': 'no-store' })
            .end(hostHtml(url.searchParams.get('viewport') !== 'none'))
        } else {
          response.writeHead(200, { 'content-type': 'text/html' }).end('<!doctype html><title>warm</title><p>warm')
        }
      } catch {
        response.writeHead(404).end('not found')
      }
    })()
  })

  await new Promise((done) => bundle.listen(bundlePort, done))
  await new Promise((done) => host.listen(hostPort, done))

  return {
    reports,
    hostUrl: `http://127.0.0.1:${hostPort}`,
    bundleUrl: `http://localhost:${bundlePort}/`,
    /** the page address of a case; open it after `begin` */
    pageUrl(test) {
      const query = new URLSearchParams({
        case: test.name,
        file: test.file,
        mode: test.mode,
        bundle: `http://localhost:${bundlePort}/`,
        ...(test.viewport === false ? { viewport: 'none' } : {}),
      })
      return `http://127.0.0.1:${hostPort}/host.html?${query}`
    },
    begin(name) {
      state.case = name
      state.phase = 'loading'
    },
    setPhase(phase) {
      state.phase = phase
    },
    /** resolves once frames of `name` reported `phase` and none has for `quietMs`; false on timeout */
    waitFor(name, phase, timeoutMs, quietMs = 2500) {
      return new Promise((done) => {
        let quiet
        const finish = (ok) => {
          clearTimeout(timer)
          clearTimeout(quiet)
          waiters.delete(check)
          done(ok)
        }
        const timer = setTimeout(() => finish(false), timeoutMs)
        const check = () => {
          const got = reports.some((r) => r.case === name && r.phase === phase)
          if (!got) return
          clearTimeout(quiet)
          quiet = setTimeout(() => finish(true), quietMs)
        }
        waiters.add(check)
        check()
      })
    },
    async close() {
      bundle.closeAllConnections()
      host.closeAllConnections()
      await Promise.all([new Promise((d) => bundle.close(d)), new Promise((d) => host.close(d))])
    },
  }
}

/** what probe.js tries in place after the page as shown, by format */
const TRIED = { docx: ['adjust-100', 'adjust-none', 'adjust-zoom', 'adjust-zoom-editable'] }

/** the documents of the repro: every format, for viewing and for editing */
export const CASES = [
  ...[
    ['docx', 'simple.docx'],
    // paragraphs of body text that run over several lines
    ['docx-report', '13-english-report.docx'],
    ['xlsx', 'compatibility-edit.xlsx'],
    ['pptx', '01_standard_business.pptx'],
    ['pdf', 'plain-sample.pdf'],
    ['hwp', 'memo.hwp'],
  ].flatMap(([format, file]) =>
    ['view', 'edit'].map((mode) => ({
      name: `${format}-${mode}`,
      file,
      mode,
      variants: TRIED[format.split('-')[0]] ?? ['adjust-100'],
    })),
  ),
  // the host page cross-origin-embed.ts serves has no viewport meta: Safari lays it out 980 wide
  {
    name: 'docx-report-view-no-viewport-meta',
    file: '13-english-report.docx',
    mode: 'view',
    viewport: false,
    variants: TRIED.docx,
  },
]
