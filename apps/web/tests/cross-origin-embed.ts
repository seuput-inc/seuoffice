import { createServer, type Server } from 'node:http'
import { readFile } from 'node:fs/promises'
import type { AddressInfo } from 'node:net'
import { extname, join, normalize, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { expect, type Page } from '@playwright/test'
import type * as Embed from '@seuoffice/embed'
import { build, type Rollup } from 'vite'

/**
 * The released bundle sits on one origin and the host page that embeds it on
 * another, as in production. The bundle's server (here a small static server
 * over apps/web/dist) lists the origins it trusts in `/embed-origins.json`;
 * the host page loads @seuoffice/embed from its source and opens a document
 * through it, so the iframe carries the embed's own sandbox.
 */
export const repo = fileURLToPath(new URL('../../..', import.meta.url))
const DIST = resolve(repo, 'apps/web/dist')

// different sites, not just different ports
export const BUNDLE_HOST = 'localhost'
export const HOST_NAME = '127.0.0.1'

export const DEFAULT_SANDBOX = 'allow-scripts allow-same-origin allow-downloads'

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

/** The host's copy of @seuoffice/embed, from its source, as one classic script; call in beforeAll */
export async function buildEmbedScript(): Promise<void> {
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
}

/** What the bundle's server answers for `/embed-origins.json`; `null` leaves the file out */
export interface ConfigFile {
  status: number
  body: string
}

export interface Site {
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
export async function serveBundle(config: ConfigFile | null): Promise<Site> {
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
export async function serveHost(): Promise<Site> {
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

/** A host and a bundle that lists it, as `test.beforeEach` sets them up */
export async function serveHostAndBundle(): Promise<{ host: Site; bundle: Site }> {
  const host = await serveHost()
  const config = { status: 200, body: JSON.stringify({ origins: [origin(host, HOST_NAME)] }) }
  return { host, bundle: await serveBundle(config) }
}

export interface HostState {
  opened: boolean
  /** the `code` of the SeuofficeEmbedError `opened` rejected with */
  error: string | undefined
  saves: number[][]
}

export type HostWindow = Window & {
  SeuofficeEmbed: typeof Embed
  hostState: HostState
  embedded: Embed.EmbeddedDocument
}

export interface EmbedOptions {
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
export async function embed(
  page: Page,
  hostUrl: string,
  bundleUrl: string,
  options: EmbedOptions,
): Promise<void> {
  await page.goto(`${hostUrl}/host.html`)
  const bytes = [...(await readFile(options.file))]
  const fileName = options.file.slice(options.file.replaceAll('\\', '/').lastIndexOf('/') + 1)
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

export const hostState = (page: Page): Promise<HostState> =>
  page.evaluate(() => (window as unknown as HostWindow).hostState)

/** The host heard `opened` */
export async function expectOpened(page: Page, timeout = 60_000): Promise<void> {
  await expect.poll(async () => (await hostState(page)).opened, { timeout }).toBe(true)
}

export const origin = (site: Site, name: string): string => `http://${name}:${site.port}`
