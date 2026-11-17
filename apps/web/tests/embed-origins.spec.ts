import { createServer, type Server } from 'node:http'
import { readFile } from 'node:fs/promises'
import type { AddressInfo } from 'node:net'
import { extname, join, normalize, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { expect, test, type Frame, type Page } from '@playwright/test'
import type * as Embed from '@seuoffice/embed'
import { build, type Rollup } from 'vite'

/**
 * The released bundle sits on one origin and the host page that embeds it on
 * another, as in production. The bundle's server (here a small static server
 * over apps/web/dist) lists the origins it trusts in `/embed-origins.json`;
 * the page reads that file at start and nothing else. A host the page refuses
 * hears `origin_not_allowed`; the embed's own sandbox and deadlines are the
 * other half of the contract.
 */
const repo = fileURLToPath(new URL('../../..', import.meta.url))
const DIST = resolve(repo, 'apps/web/dist')
const PDF = join(repo, 'apps/pdf/tests/fixtures/plain-sample.pdf')
const HWPX = join(repo, 'apps/hwp/tests/fixtures/memo.hwpx')
const KEPT_TEXT = 'This line must survive the round trip.'

// different sites, not just different ports
const BUNDLE_HOST = 'localhost'
const HOST_NAME = '127.0.0.1'

const DEFAULT_SANDBOX = 'allow-scripts allow-same-origin allow-downloads'

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
  /** the `code` of the SeuofficeEmbedError `opened` rejected with */
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
  /** the iframe's `sandbox` as the host passes it to `openDocument` */
  sandbox?: string
  /** the iframe's `allow` as the host passes it to `openDocument` */
  allow?: string
  /** set on the iframe behind the embed's back, to see what a page does in a sandbox it refuses */
  forceSandbox?: string
  readyTimeoutMs?: number
  openTimeoutMs?: number
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
    ({ bytes, fileName, bundleUrl, mode, forceSandbox, open }) => {
      const host = window as unknown as HostWindow
      const state: HostState = { opened: false, error: undefined, saves: [] }
      host.hostState = state
      const viewer = document.getElementById('viewer')!
      if (forceSandbox !== undefined) {
        const append = viewer.appendChild.bind(viewer)
        viewer.appendChild = <T extends Node>(node: T): T => {
          if (node instanceof HTMLIFrameElement) node.setAttribute('sandbox', forceSandbox)
          return append(node)
        }
      }
      const base = { baseUrl: bundleUrl, bytes: new Uint8Array(bytes), fileName, ...open }
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
        (error: Error & { code?: string }) => (state.error = error.code ?? error.message),
      )
    },
    {
      bytes,
      fileName,
      bundleUrl,
      mode: options.mode ?? 'view',
      forceSandbox: options.forceSandbox,
      open: {
        ...(options.sandbox !== undefined ? { sandbox: options.sandbox } : {}),
        ...(options.allow !== undefined ? { allow: options.allow } : {}),
        ...(options.readyTimeoutMs !== undefined ? { readyTimeoutMs: options.readyTimeoutMs } : {}),
        ...(options.openTimeoutMs !== undefined ? { openTimeoutMs: options.openTimeoutMs } : {}),
      },
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
 * The page refused this host: `opened` rejected with `origin_not_allowed`
 * (well inside `openTimeoutMs`, so the page answered; the deadline did not),
 * and the page drew nothing.
 */
async function expectRefused(page: Page): Promise<void> {
  await expect
    .poll(async () => (await hostState(page)).error, { timeout: 30_000 })
    .toBe('origin_not_allowed')
  expect((await hostState(page)).opened).toBe(false)
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
  // a refusal must come from the page, long before this deadline
  const DEADLINE = 40_000

  test('a listed origin opens a document, an unlisted one is refused with origin_not_allowed', async ({
    page,
    browser,
  }) => {
    bundle = await serveBundle(configFor(listedUrl()))
    await embed(page, listedUrl(), bundleUrl(), { file: PDF, openTimeoutMs: DEADLINE })
    await expectShown(page)
    expect(bundle.configRequests).toBeGreaterThan(0)

    const other = await browser.newPage()
    try {
      await embed(other, unlistedUrl(), bundleUrl(), { file: PDF, openTimeoutMs: DEADLINE })
      await expectRefused(other)
    } finally {
      await other.close()
    }
  })

  test('the bundle server decides: the same host page is refused once its origin is unlisted', async ({
    page,
  }) => {
    bundle = await serveBundle(configFor(unlistedUrl()))
    await embed(page, listedUrl(), bundleUrl(), { file: PDF, openTimeoutMs: DEADLINE })
    await expectRefused(page)
  })

  test('an entry with another port is not the host origin', async ({ page }) => {
    bundle = await serveBundle(configFor(`http://${HOST_NAME}:${listed!.port + 1}`))
    await embed(page, listedUrl(), bundleUrl(), { file: PDF, openTimeoutMs: DEADLINE })
    await expectRefused(page)
  })

  test('a missing config file leaves only the own origin and refuses the host', async ({
    page,
  }) => {
    bundle = await serveBundle(null)
    await embed(page, listedUrl(), bundleUrl(), { file: PDF, openTimeoutMs: DEADLINE })
    await expectRefused(page)
  })

  const BROKEN: Record<string, ConfigFile> = {
    'broken JSON': { status: 200, body: '{"origins": [' },
    'a page instead of JSON': { status: 200, body: '<!doctype html><title>SeuOffice</title>' },
    'a bare list': { status: 200, body: '["http://127.0.0.1"]' },
    'a wildcard': { status: 200, body: '{"origins":["*"]}' },
    'a server error': { status: 500, body: '{"origins":["http://127.0.0.1"]}' },
  }
  for (const [name, config] of Object.entries(BROKEN)) {
    test(`${name} refuses the host with origin_not_allowed`, async ({ page }) => {
      const body =
        config.status === 200 && config.body.includes('127.0.0.1')
          ? config.body.replace('http://127.0.0.1', listedUrl())
          : config.body
      bundle = await serveBundle({ ...config, body })
      await embed(page, listedUrl(), bundleUrl(), { file: PDF, openTimeoutMs: DEADLINE })
      await expectRefused(page)
    })
  }

  test('one bad entry beside a good one refuses the whole file', async ({ page }) => {
    bundle = await serveBundle(configFor(listedUrl(), 'null'))
    await embed(page, listedUrl(), bundleUrl(), { file: PDF, openTimeoutMs: DEADLINE })
    await expectRefused(page)
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
    await embed(page, listedUrl(), `${bundleUrl()}?${extra}#${listedUrl()}`, {
      file: PDF,
      openTimeoutMs: DEADLINE,
    })
    await expectRefused(page)
  })
})

/** The embed's iframe on a cross-origin bundle, as a host sets it up */
test.describe('the iframe sandbox on a cross-origin bundle', () => {
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
  const appFrame = (page: Page): Frame =>
    page.frames().find((f) => f.url().startsWith(bundleUrl()))!

  test('the default is closed: scripts, own origin, downloads', async ({ page }) => {
    await embed(page, hostUrl(), bundleUrl(), { file: PDF })
    await expectShown(page)
    const attribute = await page.evaluate(() =>
      document.querySelector('#viewer iframe')!.getAttribute('sandbox'),
    )
    expect(attribute).toBe(DEFAULT_SANDBOX)
  })

  test('a sandbox without scripts or the own origin is refused at once, before any frame', async ({
    page,
  }) => {
    for (const sandbox of ['allow-scripts', 'allow-same-origin allow-downloads', '']) {
      await embed(page, hostUrl(), bundleUrl(), { file: PDF, sandbox })
      await expect
        .poll(async () => (await hostState(page)).error, { timeout: 3000 })
        .toBe('invalid_sandbox')
      expect(await page.locator('#viewer iframe').count()).toBe(0)
    }
  })

  test('a frame forced into an opaque origin never opens: the host hears timeout', async ({
    page,
  }) => {
    await embed(page, hostUrl(), bundleUrl(), {
      file: PDF,
      forceSandbox: 'allow-scripts',
      readyTimeoutMs: READY_TIMEOUT,
    })
    await expect
      .poll(async () => (await hostState(page)).error, { timeout: 15_000 })
      .toBe('timeout')
  })

  test('the default opens, edits and saves a pdf (pdf.js worker, wasm, fonts)', async ({
    page,
  }) => {
    const failures = watchFailures(page)
    await embed(page, hostUrl(), bundleUrl(), { file: PDF, mode: 'edit' })
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

  test('the default opens an hwpx (rhwp wasm and its fonts)', async ({ page }) => {
    const failures = watchFailures(page)
    await embed(page, hostUrl(), bundleUrl(), { file: HWPX })
    await expect.poll(async () => (await hostState(page)).opened, { timeout: 60_000 }).toBe(true)
    expect(failures).toEqual([])
  })

  test('the default lets the page download a file', async ({ page }) => {
    await embed(page, hostUrl(), bundleUrl(), { file: PDF })
    await expectShown(page)
    const download = page.waitForEvent('download', { timeout: 5000 })
    await appFrame(page).evaluate(() => {
      const link = document.createElement('a')
      link.href = URL.createObjectURL(new Blob(['x']))
      link.download = 'from-the-page.txt'
      document.body.appendChild(link)
      link.click()
    })
    expect((await download).suggestedFilename()).toBe('from-the-page.txt')
  })

  test('without allow-downloads the page cannot download', async ({ page }) => {
    await embed(page, hostUrl(), bundleUrl(), {
      file: PDF,
      sandbox: 'allow-scripts allow-same-origin',
    })
    await expectShown(page)
    const download = page.waitForEvent('download', { timeout: 3000 })
    await appFrame(page).evaluate(() => {
      const link = document.createElement('a')
      link.href = URL.createObjectURL(new Blob(['x']))
      link.download = 'from-the-page.txt'
      document.body.appendChild(link)
      link.click()
    })
    await expect(download).rejects.toThrow()
  })

  /** What a page's alert-family calls and window.open do under `sandbox` */
  async function probeModals(
    page: Page,
    sandbox: string | undefined,
  ): Promise<{ dialogs: string[]; popup: boolean }> {
    const dialogs: string[] = []
    page.on('dialog', (dialog) => {
      dialogs.push(dialog.type())
      void dialog.dismiss()
    })
    await embed(page, hostUrl(), bundleUrl(), { file: PDF, ...(sandbox ? { sandbox } : {}) })
    await expectShown(page)
    const frame = appFrame(page)
    await frame.evaluate(() => {
      window.alert('probe')
      window.confirm('probe')
      window.prompt('probe')
    })
    const popup = page.waitForEvent('popup', { timeout: 3000 }).then(
      () => true,
      () => false,
    )
    await frame.evaluate(() => window.open('about:blank', '_blank'))
    return { dialogs, popup: await popup }
  }

  test('the default blocks alert, confirm, prompt and window.open', async ({ page }) => {
    expect(await probeModals(page, undefined)).toEqual({ dialogs: [], popup: false })
  })

  test('allow-modals and allow-popups open them again', async ({ page }) => {
    const sandbox = `${DEFAULT_SANDBOX} allow-modals allow-popups`
    expect(await probeModals(page, sandbox)).toEqual({
      dialogs: ['alert', 'confirm', 'prompt'],
      popup: true,
    })
  })

  /** window.print from the page: whether the print events fire */
  async function probePrint(page: Page, sandbox: string | undefined): Promise<string[]> {
    await embed(page, hostUrl(), bundleUrl(), { file: PDF, ...(sandbox ? { sandbox } : {}) })
    await expectShown(page)
    return appFrame(page).evaluate(async () => {
      const events: string[] = []
      window.addEventListener('beforeprint', () => events.push('beforeprint'))
      window.addEventListener('afterprint', () => events.push('afterprint'))
      window.print()
      await new Promise((done) => setTimeout(done, 1500))
      return events
    })
  }

  test('the default blocks window.print', async ({ page }) => {
    expect(await probePrint(page, undefined)).toEqual([])
  })

  test('allow-modals lets window.print run', async ({ page }) => {
    const events = await probePrint(page, `${DEFAULT_SANDBOX} allow-modals`)
    expect(events).toContain('beforeprint')
  })
})

/**
 * The clipboard under the default sandbox. Native copy and paste (Ctrl+C and
 * Ctrl+V on the page) are the browser's own; `navigator.clipboard`, which the
 * apps' context menus, AI copy buttons and paste actions call, needs the
 * iframe's `allow` for a cross-origin frame.
 */
test.describe('the clipboard under the default sandbox', () => {
  let bundle: Site
  let host: Site

  test.beforeEach(async ({ context }) => {
    host = await serveHost()
    const hostOrigin = origin(host, HOST_NAME)
    bundle = await serveBundle({ status: 200, body: JSON.stringify({ origins: [hostOrigin] }) })
    await context.grantPermissions(['clipboard-read', 'clipboard-write'], { origin: hostOrigin })
    await context.grantPermissions(['clipboard-read', 'clipboard-write'], {
      origin: origin(bundle, BUNDLE_HOST),
    })
  })

  test.afterEach(async () => {
    await Promise.all([bundle.close(), host.close()])
  })

  const hostUrl = (): string => origin(host, HOST_NAME)
  const bundleUrl = (): string => `${origin(bundle, BUNDLE_HOST)}/`
  const appFrame = (page: Page): Frame =>
    page.frames().find((f) => f.url().startsWith(bundleUrl()))!

  /** Outcome of `navigator.clipboard` calls made by the page inside the iframe */
  interface ApiResult {
    write: string
    read: string
  }

  async function probeApi(page: Page, allow: string | undefined): Promise<ApiResult> {
    await embed(page, hostUrl(), bundleUrl(), { file: PDF, ...(allow ? { allow } : {}) })
    await expectShown(page)
    await page.evaluate(() => navigator.clipboard.writeText('from-the-host'))
    // the page's own document must have focus, as it has when a person clicked in it
    await page.frameLocator('#viewer iframe').locator('.pdf-page-content').first().click()
    return appFrame(page).evaluate(async () => {
      const outcome = async (run: () => Promise<unknown>): Promise<string> =>
        run().then(
          (value) => `ok:${String(value ?? '')}`,
          (error: Error) => error.name,
        )
      const read = await outcome(() => navigator.clipboard.readText())
      const write = await outcome(() => navigator.clipboard.writeText('from-the-page'))
      return { write, read }
    })
  }

  test('without allow the page cannot use navigator.clipboard', async ({ page }) => {
    expect(await probeApi(page, undefined)).toEqual({
      write: 'NotAllowedError',
      read: 'NotAllowedError',
    })
  })

  test('allow="clipboard-write" lets the page copy, not paste', async ({ page }) => {
    expect(await probeApi(page, 'clipboard-write')).toEqual({
      write: 'ok:',
      read: 'NotAllowedError',
    })
    expect(await page.evaluate(() => navigator.clipboard.readText())).toBe('from-the-page')
  })

  test('allow="clipboard-read; clipboard-write" lets the page copy and paste', async ({ page }) => {
    expect(await probeApi(page, 'clipboard-read; clipboard-write')).toEqual({
      write: 'ok:',
      read: 'ok:from-the-host',
    })
  })

  test('view mode: Ctrl+C on selected text copies without any allow', async ({ page }) => {
    await embed(page, hostUrl(), bundleUrl(), { file: PDF })
    await expectShown(page)
    await page.evaluate(() => navigator.clipboard.writeText('before'))
    const line = page.frameLocator('#viewer iframe').getByText(KEPT_TEXT).first()
    await line.click({ clickCount: 3 })
    await page.keyboard.press('ControlOrMeta+c')
    await expect
      .poll(() => page.evaluate(() => navigator.clipboard.readText()), { timeout: 5000 })
      .toContain('must survive')
  })

  test('edit mode: Ctrl+V pastes into the page without any allow', async ({ page }) => {
    await embed(page, hostUrl(), bundleUrl(), { file: PDF, mode: 'edit' })
    await expectShown(page)
    await page.evaluate(() => navigator.clipboard.writeText('pasted-from-host'))
    const frame = page.frameLocator('#viewer iframe')
    await frame.locator('.pdf-page-content').first().click()
    await page.keyboard.press('ControlOrMeta+f')
    const input = frame.locator('.pdf-search-input')
    await input.click()
    await page.keyboard.press('ControlOrMeta+v')
    await expect(input).toHaveValue(/pasted-from-host/)
  })
})
