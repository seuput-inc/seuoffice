import { createHash } from 'node:crypto'
import { once } from 'node:events'
import { mkdtemp, rm, writeFile } from 'node:fs/promises'
import {
  createServer,
  request,
  type ClientRequest,
  type IncomingMessage,
  type RequestOptions,
} from 'node:http'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { HttpExecutor } from 'builder-util-runtime'
import type { AppAdapter } from 'electron-updater/out/AppAdapter'
import { AppUpdater } from 'electron-updater/out/AppUpdater'
import { expect, it } from 'vitest'

const channels = ['latest', 'stable'] as const
const releases = '/seuput-inc/seuoffice/releases'
const tag = 'v0.12.52'
const stagingId = 'fe712a2e-d8bc-4aa1-94ca-bb30b60c645c'

function manifest(version: string, url: string) {
  const sha512 = createHash('sha512').update(`fixture: ${url}`).digest('base64')
  return JSON.stringify({ version, files: [{ url, sha512 }] })
}

// Preserve both provider URL construction and HttpExecutor's header assembly. Only the socket
// transport is replaced: no DNS, TLS, proxy or connection to the requested GitHub hostname.
class LoopbackExecutor extends HttpExecutor<ClientRequest> {
  constructor(private readonly port: number) {
    super()
  }

  createRequest(options: RequestOptions, receive: (response: IncomingMessage) => void) {
    const { hostname, protocol, path, headers } = options
    const legacy = hostname === 'github.com' && protocol === 'https:' && !options.port
    const candidate =
      hostname === '127.0.0.1' && protocol === 'http:' && Number(options.port) === this.port
    if (
      !path ||
      !(legacy ? path.startsWith(releases) : candidate && path.startsWith('/candidate/'))
    ) {
      throw new Error(`Unexpected updater destination: ${protocol}//${hostname}${path}`)
    }
    return request(
      {
        hostname: '127.0.0.1',
        port: this.port,
        agent: false,
        method: options.method,
        path: legacy ? `/legacy${path}` : path,
        headers: { ...headers, host: legacy ? 'github.com' : `127.0.0.1:${this.port}` },
      },
      receive,
    )
  }
}

function notAllowed(): never {
  throw new Error('Metadata checks must not download, install or start the app')
}

class MetadataUpdater extends AppUpdater {
  constructor(app: AppAdapter, executor: LoopbackExecutor, channel: (typeof channels)[number]) {
    super(null, app)
    // 6.8.9 leaves the executor null for its AppAdapter test seam and strips these fields from its .d.ts.
    Object.assign(this, { httpExecutor: executor, _testOnlyOptions: { platform: 'darwin' } })
    this.logger = null
    this.autoDownload = false
    if (channel === 'stable') this.channel = channel
    this.allowDowngrade = false
    this.allowPrerelease = channel === 'latest'
  }

  protected doDownloadUpdate(): never {
    return notAllowed()
  }
  quitAndInstall(): never {
    return notAllowed()
  }
}

it.each(channels)(
  'routes %s macOS checks through the bridge even for a late legacy client',
  async (channel) => {
    const atom = `/legacy${releases}.atom`
    const latest = `/legacy${releases}/latest`
    const tagged = `/legacy${releases}/download/${tag}`
    const routes = new Map([
      [atom, `<feed><entry><link href="https://github.com${releases}/tag/${tag}"/></entry></feed>`],
      [latest, JSON.stringify({ tag_name: tag })],
    ])
    for (const name of channels) {
      routes.set(`${tagged}/${name}-mac.yml`, manifest('0.12.52', `Electron-${name}.zip`))
    }
    const seen: { url: string; headers: IncomingMessage['headers']; method?: string }[] = []
    const server = createServer((req, res) => {
      const url = req.url ?? ''
      seen.push({ url, headers: req.headers, method: req.method })
      const body = routes.get(url.split('?')[0])
      res.writeHead(body === undefined ? 404 : 200).end(body)
    })
    const data = await mkdtemp(join(tmpdir(), 'migration-route-'))
    try {
      server.listen(0, '127.0.0.1')
      await once(server, 'listening')
      const address = server.address()
      if (!address || typeof address === 'string') throw new Error('Expected a loopback TCP port')
      const executor = new LoopbackExecutor(address.port)
      const origin = `http://127.0.0.1:${address.port}`
      await writeFile(join(data, '.updaterId'), stagingId)
      async function check(version: string, config: object, offered: string, file: string) {
        const appUpdateConfigPath = join(data, `${version}-app-update.yml`)
        await writeFile(appUpdateConfigPath, JSON.stringify(config))
        const updater = new MetadataUpdater(
          {
            version,
            name: 'fixture',
            isPackaged: true,
            appUpdateConfigPath,
            userDataPath: data,
            baseCachePath: data,
            whenReady: () => Promise.resolve(),
            relaunch: notAllowed,
            quit: notAllowed,
            onQuit: notAllowed,
          },
          executor,
          channel,
        )
        expect(await updater.checkForUpdates()).toMatchObject({
          isUpdateAvailable: true,
          downloadPromise: null,
          updateInfo: {
            version: offered,
            files: [
              {
                url: file,
                sha512: createHash('sha512').update(`fixture: ${file}`).digest('base64'),
              },
            ],
          },
        })
      }

      const old = { provider: 'github', owner: 'seuput-inc', repo: 'seuoffice', channel }
      await check('0.12.51', old, '0.12.52', `Electron-${channel}.zip`)
      for (const name of channels) {
        routes.set(`/candidate/${name}-mac.yml`, manifest('0.13.0', `Tauri-${name}.zip`))
      }
      await check(
        '0.12.52',
        { provider: 'generic', url: `${origin}/candidate/`, channel },
        '0.13.0',
        `Tauri-${channel}.zip`,
      )
      await check('0.12.51', old, '0.12.52', `Electron-${channel}.zip`)

      const legacyPaths = [
        atom,
        ...(channel === 'stable' ? [latest] : []),
        `${tagged}/${channel}-mac.yml`,
      ]
      expect(seen.map(({ url }) => url.split('?')[0])).toEqual([
        ...legacyPaths,
        `/candidate/${channel}-mac.yml`,
        ...legacyPaths,
      ])
      for (const { url, method, headers } of seen) {
        expect(method).toBe('GET')
        expect(headers['user-agent']).toBe('electron-builder')
        expect(headers['x-user-staging-id']).toBe(stagingId)
        // The tagged path identifies the offered release, not the running app.
        const sourceVersion = url.startsWith('/legacy/') ? '0.12.51' : '0.12.52'
        expect(JSON.stringify({ url, headers })).not.toContain(sourceVersion)
      }
    } finally {
      if (server.listening) {
        const closed = once(server, 'close')
        server.close()
        await closed
      }
      await rm(data, { recursive: true, force: true })
    }
  },
)
