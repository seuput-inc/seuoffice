import { createServer, type Server } from 'node:http'
import { readFile } from 'node:fs/promises'
import type { AddressInfo } from 'node:net'
import { extname, join, normalize, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { expect, test, type Page } from '@playwright/test'
import type * as Embed from '@seuoffice/embed'
import { build, type Rollup } from 'vite'

/**
 * The released bundle sits on one origin and the host page that embeds it on
 * another, as in production. The bundle's server (here a small static server
 * over apps/web/dist) lists the origins it trusts in `/embed-origins.json`;
 * the page reads that file at start and nothing else.
 */
const repo = fileURLToPath(new URL('../../..', import.meta.url))
const DIST = resolve(repo, 'apps/web/dist')
const PDF = join(repo, 'apps/pdf/tests/fixtures/plain-sample.pdf')
const HWPX = join(repo, 'apps/hwp/tests/fixtures/memo.hwpx')
const KEPT_TEXT = 'This line must survive the round trip.'

// different sites, not just different ports
const BUNDLE_HOST = 'localhost'
const HOST_NAME = '127.0.0.1'

const HOST_HTML =
  '<!doctype html><html><body style="margin:0"><div id="viewer" style="height:900px"></div>' +
  '<script src="/embed.js"></script></body></html>'

const MIME: Record<string, string> = {
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

let embedScript = ''

test.beforeAll(async () => {
  // the host's copy of @seuoffice/embed, from its source, as one classic script
  const result = await build({
    configFile: false,
    logLevel: 'silent',
    resolve: { conditions: ['seuoffice-source'] },
    build: {
      write: false,
      minify: false,
      lib: {
        entry: `${repo}packages/embed/src/index.ts`,
        formats: ['iife'],
        name: 'SeuofficeEmbed',
      },
    },
  })
  const output = (Array.isArray(result) ? result[0]! : (result as Rollup.RollupOutput)).output
  embedScript = output[0].code
})

/** What the bundle's server answers for `/embed-origins.json`; `null` leaves the file out */
interface ConfigFile {
  status: number
  body: string
}

interface Site {
  port: number
  /** the requests this site saw for the config file */
  configRequests: number
  close: () => Promise<void>
}

function listen(server: Server): Promise<number> {
  return new Promise((done) => server.listen(0, () => done((server.address() as AddressInfo).port)))
}

function closing(server: Server): Promise<void> {
  server.closeAllConnections()
  return new Promise((done) => server.close(() => done()))
}

/** apps/web/dist over HTTP, with `config` as its embed-origins.json and a host page of its own */
async function serveBundle(config: ConfigFile | null): Promise<Site> {
  const site: Site = { port: 0, configRequests: 0, close: async () => {} }
  const server = createServer((request, response) => {
    void (async () => {
      const path = decodeURIComponent(new URL(request.url ?? '/', 'http://x').pathname)
      if (path === '/embed.js') {
        response.writeHead(200, { 'content-type': 'text/javascript' }).end(embedScript)
        return
      }
      if (path === '/host.html') {
        response.writeHead(200, { 'content-type': 'text/html' }).end(HOST_HTML)
        return
      }
      if (path === '/embed-origins.json') {
        site.configRequests++
        if (config === null) response.writeHead(404).end('not found')
        else
          response.writeHead(config.status, { 'content-type': 'application/json' }).end(config.body)
        return
      }
      const file = normalize(join(DIST, path.endsWith('/') ? `${path}index.html` : path))
      if (!file.startsWith(DIST)) {
        response.writeHead(403).end()
        return
      }
      try {
        const body = await readFile(file)
        response
          .writeHead(200, { 'content-type': MIME[extname(file)] ?? 'application/octet-stream' })
          .end(body)
      } catch {
        response.writeHead(404).end('not found')
      }
    })()
  })
  site.port = await listen(server)
  site.close = () => closing(server)
  return site
}

/** A page that only hosts: the embed script and an empty viewer */
async function serveHost(): Promise<Site> {
  const site: Site = { port: 0, configRequests: 0, close: async () => {} }
  const server = createServer((request, response) => {
    if (request.url === '/embed.js') {
      response.writeHead(200, { 'content-type': 'text/javascript' }).end(embedScript)
    } else {
      response.writeHead(200, { 'content-type': 'text/html' }).end(HOST_HTML)
    }
  })
  site.port = await listen(server)
  site.close = () => closing(server)
  return site
}

interface HostState {
  opened: boolean
  error: string | undefined
  saves: number[][]
}

type HostWindow = Window & {
  SeuofficeEmbed: typeof Embed
  hostState: HostState
  embedded: Embed.EmbeddedDocument
}

interface EmbedOptions {
  file: string
  mode?: 'view' | 'edit'
  /** the iframe's `sandbox` attribute; none when left out */
  sandbox?: string
  readyTimeoutMs?: number
}

/** Load the host page at `hostUrl` and embed `file` from the bundle at `bundleUrl` */
async function embed(
  page: Page,
  hostUrl: string,
  bundleUrl: string,
  options: EmbedOptions,
): Promise<void> {
  await page.goto(`${hostUrl}/host.html`)
  const bytes = [...(await readFile(options.file))]
  const fileName = options.file.slice(options.file.lastIndexOf('/') + 1)
  await page.evaluate(
    ({ bytes, fileName, bundleUrl, mode, sandbox, readyTimeoutMs }) => {
      const host = window as unknown as HostWindow
      const state: HostState = { opened: false, error: undefined, saves: [] }
      host.hostState = state
      const viewer = document.getElementById('viewer')!
      if (sandbox !== undefined) {
        // set before the iframe is inserted, so its first navigation is sandboxed
        const append = viewer.appendChild.bind(viewer)
        viewer.appendChild = <T extends Node>(node: T): T => {
          if (node instanceof HTMLIFrameElement) node.setAttribute('sandbox', sandbox)
          return append(node)
        }
      }
      const base = { baseUrl: bundleUrl, bytes: new Uint8Array(bytes), fileName, readyTimeoutMs }
      const doc =
        mode === 'edit'
          ? host.SeuofficeEmbed.openDocument(viewer, {
              ...base,
              mode,
              onSave: async ({ bytes }) => {
                state.saves.push([...bytes])
              },
            })
          : host.SeuofficeEmbed.openDocument(viewer, base)
      host.embedded = doc
      doc.opened.then(
        () => (state.opened = true),
        (error: Error) => (state.error = error.message),
      )
    },
    {
      bytes,
      fileName,
      bundleUrl,
      mode: options.mode ?? 'view',
      sandbox: options.sandbox,
      readyTimeoutMs: options.readyTimeoutMs,
    },
  )
}

const hostState = (page: Page): Promise<HostState> =>
  page.evaluate(() => (window as unknown as HostWindow).hostState)

/** The document is on screen: the host heard `opened` and the app drew the document */
async function expectShown(page: Page): Promise<void> {
  await expect.poll(async () => (await hostState(page)).opened, { timeout: 45_000 }).toBe(true)
  await expect(page.frameLocator('#viewer iframe').getByText(KEPT_TEXT).first()).toBeVisible({
    timeout: 30_000,
  })
}

/**
 * The page answered nothing to this host. Waits until the bundle's server was
 * asked for the file (the page is up and listening), then gives an `open` that
 * might wrongly be accepted time to show.
 */
async function expectIgnored(page: Page, bundle: Site): Promise<void> {
  await expect.poll(() => bundle.configRequests, { timeout: 45_000 }).toBeGreaterThan(0)
  // nothing to await for a message that is dropped: a fixed pause is the only way to see no answer
  await page.waitForTimeout(2000)
  expect(await hostState(page)).toMatchObject({ opened: false, error: undefined })
  await expect(page.frameLocator('#viewer iframe').getByText(KEPT_TEXT)).toHaveCount(0)
}

const origin = (site: Site, name: string): string => `http://${name}:${site.port}`

test.describe('allowed origins come from the bundle server', () => {
  let bundle: Site | undefined
  let listed: Site | undefined
  let unlisted: Site | undefined

  test.beforeEach(async () => {
    listed = await serveHost()
    unlisted = await serveHost()
  })

  test.afterEach(async () => {
    await Promise.all([bundle?.close(), listed?.close(), unlisted?.close()])
    bundle = listed = unlisted = undefined
  })

  const bundleUrl = (): string => `${origin(bundle!, BUNDLE_HOST)}/`
  const listedUrl = (): string => origin(listed!, HOST_NAME)
  const unlistedUrl = (): string => origin(unlisted!, HOST_NAME)
  const configFor = (...origins: string[]): ConfigFile => ({
    status: 200,
    body: JSON.stringify({ origins }),
  })

  test('a listed origin opens a document, an unlisted one is ignored', async ({
    page,
    browser,
  }) => {
    bundle = await serveBundle(configFor(listedUrl()))
    await embed(page, listedUrl(), bundleUrl(), { file: PDF })
    await expectShown(page)
    expect(bundle.configRequests).toBeGreaterThan(0)

    const other = await browser.newPage()
    try {
      const before = bundle.configRequests
      await embed(other, unlistedUrl(), bundleUrl(), { file: PDF })
      await expect.poll(() => bundle!.configRequests).toBeGreaterThan(before)
      await other.waitForTimeout(2000)
      expect(await hostState(other)).toMatchObject({ opened: false, error: undefined })
      await expect(other.frameLocator('#viewer iframe').getByText(KEPT_TEXT)).toHaveCount(0)
    } finally {
      await other.close()
    }
  })

  test('the bundle server decides: the same host page is ignored once its origin is unlisted', async ({
    page,
  }) => {
    bundle = await serveBundle(configFor(unlistedUrl()))
    await embed(page, listedUrl(), bundleUrl(), { file: PDF })
    await expectIgnored(page, bundle)
  })

  test('an entry with another port is not the host origin', async ({ page }) => {
    // the host's real origin is http://127.0.0.1:<port>; a list entry for another port never matches
    bundle = await serveBundle(configFor(`http://${HOST_NAME}:${listed!.port + 1}`))
    await embed(page, listedUrl(), bundleUrl(), { file: PDF })
    await expectIgnored(page, bundle)
  })

  test('a missing config file leaves only the own origin', async ({ page }) => {
    bundle = await serveBundle(null)
    await embed(page, listedUrl(), bundleUrl(), { file: PDF })
    await expectIgnored(page, bundle)
  })

  const BROKEN: Record<string, ConfigFile> = {
    'broken JSON': { status: 200, body: '{"origins": [' },
    'a page instead of JSON': { status: 200, body: '<!doctype html><title>SeuOffice</title>' },
    'a bare list': { status: 200, body: '["http://127.0.0.1"]' },
    'a wildcard': { status: 200, body: '{"origins":["*"]}' },
    'a server error': { status: 500, body: '{"origins":["http://127.0.0.1"]}' },
  }
  for (const [name, config] of Object.entries(BROKEN)) {
    test(`${name} leaves only the own origin`, async ({ page }) => {
      const body =
        config.status === 200 && config.body.includes('127.0.0.1')
          ? config.body.replace('http://127.0.0.1', listedUrl())
          : config.body
      bundle = await serveBundle({ ...config, body })
      await embed(page, listedUrl(), bundleUrl(), { file: PDF })
      await expectIgnored(page, bundle)
    })
  }

  test('one bad entry beside a good one refuses the whole file', async ({ page }) => {
    bundle = await serveBundle(configFor(listedUrl(), 'null'))
    await embed(page, listedUrl(), bundleUrl(), { file: PDF })
    await expectIgnored(page, bundle)
  })

  test('the own origin opens with a missing or a broken file', async ({ page }) => {
    for (const config of [null, BROKEN['broken JSON']!]) {
      bundle = await serveBundle(config)
      await embed(page, origin(bundle, BUNDLE_HOST), bundleUrl(), { file: PDF })
      await expectShown(page)
      await bundle.close()
    }
  })

  test('the page address cannot add an origin', async ({ page }) => {
    bundle = await serveBundle(null)
    const extra = `origins=${encodeURIComponent(listedUrl())}&embed-origins=${encodeURIComponent(listedUrl())}`
    await embed(page, listedUrl(), `${bundleUrl()}?${extra}#${listedUrl()}`, { file: PDF })
    await expectIgnored(page, bundle)
  })
})

/**
 * Whether a host can put `sandbox` on the iframe of a cross-origin bundle. The
 * page needs its own origin (allow-same-origin): without it the page's origin
 * is `null`, the host drops its `ready`, and the document never opens.
 */
test.describe('a sandboxed iframe on a cross-origin bundle', () => {
  const READY_TIMEOUT = 5000
  let bundle: Site
  let host: Site

  test.beforeEach(async () => {
    host = await serveHost()
    const config = { status: 200, body: JSON.stringify({ origins: [origin(host, HOST_NAME)] }) }
    bundle = await serveBundle(config)
  })

  test.afterEach(async () => {
    await Promise.all([bundle.close(), host.close()])
  })

  /** Every request that failed, and every response of 400 or more, from any frame */
  function watchFailures(page: Page): string[] {
    const failures: string[] = []
    page.on('requestfailed', (request) =>
      failures.push(`${request.failure()?.errorText} ${request.url()}`),
    )
    page.on('response', (response) => {
      if (response.status() >= 400) failures.push(`${response.status()} ${response.url()}`)
    })
    page.on('pageerror', (error) => failures.push(error.message))
    return failures
  }

  const hostUrl = (): string => origin(host, HOST_NAME)
  const bundleUrl = (): string => `${origin(bundle, BUNDLE_HOST)}/`

  test('allow-scripts alone never opens: the page has an opaque origin', async ({ page }) => {
    await embed(page, hostUrl(), bundleUrl(), {
      file: PDF,
      sandbox: 'allow-scripts',
      readyTimeoutMs: READY_TIMEOUT,
    })
    await expect
      .poll(async () => (await hostState(page)).error, { timeout: 15_000 })
      .toMatch(/did not answer/)
    expect((await hostState(page)).opened).toBe(false)
  })

  const SAME_ORIGIN = 'allow-scripts allow-same-origin'
  const WORKING = [
    SAME_ORIGIN,
    `${SAME_ORIGIN} allow-downloads`,
    `${SAME_ORIGIN} allow-downloads allow-forms allow-popups allow-modals`,
  ]
  for (const sandbox of WORKING) {
    test(`"${sandbox}" opens, edits and saves a pdf (pdf.js worker, wasm, fonts)`, async ({
      page,
    }) => {
      const failures = watchFailures(page)
      await embed(page, hostUrl(), bundleUrl(), { file: PDF, mode: 'edit', sandbox })
      await expectShown(page)
      const frame = page.frameLocator('#viewer iframe')
      await frame.locator('.ribbon-tab', { hasText: '페이지' }).click()
      await frame.getByRole('button', { name: '오른쪽으로 회전' }).click()
      await page.evaluate(() => (window as unknown as HostWindow).embedded.save())
      const { saves } = await hostState(page)
      expect(saves).toHaveLength(1)
      expect(Buffer.from(saves[0]!).subarray(0, 5).toString()).toBe('%PDF-')
      expect(failures).toEqual([])
    })

    test(`"${sandbox}" opens an hwpx (rhwp wasm and its fonts)`, async ({ page }) => {
      const failures = watchFailures(page)
      await embed(page, hostUrl(), bundleUrl(), { file: HWPX, sandbox })
      await expect.poll(async () => (await hostState(page)).opened, { timeout: 60_000 }).toBe(true)
      expect(failures).toEqual([])
    })

    test(`"${sandbox}" ${sandbox.includes('allow-downloads') ? 'allows' : 'blocks'} a download`, async ({
      page,
    }) => {
      await embed(page, hostUrl(), bundleUrl(), { file: PDF, sandbox })
      await expectShown(page)
      const frame = page.frames().find((f) => f.url().startsWith(bundleUrl()))!
      const download = page.waitForEvent('download', { timeout: 4000 })
      await frame.evaluate(() => {
        const link = document.createElement('a')
        link.href = URL.createObjectURL(new Blob(['x']))
        link.download = 'from-the-page.txt'
        document.body.appendChild(link)
        link.click()
      })
      if (sandbox.includes('allow-downloads')) {
        expect((await download).suggestedFilename()).toBe('from-the-page.txt')
      } else {
        await expect(download).rejects.toThrow()
      }
    })

    test(`"${sandbox}" ${sandbox.includes('allow-modals') ? 'allows' : 'blocks'} dialogs and popups`, async ({
      page,
    }) => {
      const dialogs: string[] = []
      page.on('dialog', (dialog) => {
        dialogs.push(dialog.type())
        void dialog.dismiss()
      })
      await embed(page, hostUrl(), bundleUrl(), { file: PDF, sandbox })
      await expectShown(page)
      const frame = page.frames().find((f) => f.url().startsWith(bundleUrl()))!
      await frame.evaluate(() => window.confirm('sandbox probe'))
      const popup = page.waitForEvent('popup', { timeout: 3000 })
      await frame.evaluate(() => window.open('about:blank', '_blank'))
      expect(dialogs).toEqual(sandbox.includes('allow-modals') ? ['confirm'] : [])
      if (sandbox.includes('allow-popups')) await popup
      else await expect(popup).rejects.toThrow()
    })
  }
})
