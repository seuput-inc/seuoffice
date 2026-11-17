import {
  afterEach,
  beforeEach,
  describe,
  expect,
  it,
  vi,
  type Mock,
  type MockInstance,
} from 'vitest'
import type * as ChannelModule from '../src/embed-channel'

type Channel = typeof ChannelModule

const OWN = location.origin
const LISTED = 'https://yuyu.example'
const OTHER = 'https://other.example'

describe('normalizeOrigin', () => {
  let channel: Channel
  beforeEach(async () => {
    channel = await import('../src/embed-channel')
  })

  it.each([
    ['https://yuyu.example', 'https://yuyu.example'],
    ['https://yuyu.example/', 'https://yuyu.example'],
    ['HTTPS://YuYu.Example', 'https://yuyu.example'],
    ['https://yuyu.example:443', 'https://yuyu.example'],
    ['http://localhost:5173', 'http://localhost:5173'],
    ['http://127.0.0.1:8080', 'http://127.0.0.1:8080'],
  ])('normalizes %s', (input, origin) => {
    expect(channel.normalizeOrigin(input)).toBe(origin)
  })

  it.each([
    '*',
    'https://*.example.com',
    'https://%2A.example.com',
    'null',
    '',
    'yuyu.example',
    '//yuyu.example',
    'https://yuyu.example/app',
    'https://yuyu.example/?x=1',
    'https://yuyu.example?',
    'https://yuyu.example/#top',
    'https://user:pw@yuyu.example',
    ' https://yuyu.example',
    'https://yuyu.example ',
    'ftp://yuyu.example',
    'file:///tmp/host.html',
    'data:text/html,hi',
    'javascript:alert(1)',
    'chrome-extension://abcdef',
    42,
    null,
    undefined,
    { origin: LISTED },
  ])('rejects %j', (input) => {
    expect(channel.normalizeOrigin(input)).toBeNull()
  })
})

describe('parseOriginsConfig', () => {
  let channel: Channel
  beforeEach(async () => {
    channel = await import('../src/embed-channel')
  })

  it('reads a list of origins, normalized', () => {
    expect(channel.parseOriginsConfig({ origins: [LISTED, 'HTTP://A.example:80/'] })).toEqual([
      LISTED,
      'http://a.example',
    ])
    expect(channel.parseOriginsConfig({ origins: [] })).toEqual([])
  })

  it.each([
    null,
    'https://yuyu.example',
    [LISTED],
    {},
    { origins: LISTED },
    { origins: '*' },
    { origins: { 0: LISTED } },
    { origins: [LISTED, '*'] },
    { origins: [LISTED, 'null'] },
    { origins: [LISTED, 7] },
    { origins: [LISTED, 'https://yuyu.example/path'] },
  ])('refuses the whole file for %j', (json) => {
    expect(channel.parseOriginsConfig(json)).toBeNull()
  })
})

describe('the origins a page accepts', () => {
  let host: Window
  let channel: Channel
  let fetchMock: Mock<(url: URL, init: RequestInit) => Promise<Response>>

  const config = (body: BodyInit | null, status = 200): Response => new Response(body, { status })

  function fromHost(origin: string, fileName = 'memo.hwpx'): void {
    window.dispatchEvent(
      new MessageEvent('message', {
        source: host,
        origin,
        data: {
          source: 'seuoffice',
          version: 1,
          type: 'open',
          fileName,
          bytes: new ArrayBuffer(1),
        },
      }),
    )
  }

  // a page that ignores an `open` emits nothing to wait for, so a short real delay lets queued work run
  /** Whether `receiveOpen` has settled after everything queued has run */
  async function settled(opening: Promise<unknown>): Promise<boolean> {
    let done = false
    void opening.then(() => (done = true))
    await new Promise((resolve) => setTimeout(resolve, 20))
    return done
  }

  const REFUSAL = {
    source: 'seuoffice',
    version: 1,
    type: 'error',
    code: 'origin_not_allowed',
  }
  let posts: MockInstance<Window['postMessage']>
  /** What the page posted to the host besides `ready`: [message, options] */
  const refusals = (): unknown[][] =>
    posts.mock.calls.filter(([message]) => (message as { type?: string }).type === 'error')

  async function load(respond: () => Promise<Response>): Promise<{ opening: Promise<unknown> }> {
    fetchMock = vi.fn(respond)
    vi.stubGlobal('fetch', fetchMock)
    // the channel keeps one host per page load, so each test loads a fresh copy
    vi.resetModules()
    channel = await import('../src/embed-channel')
    return { opening: channel.receiveOpen() }
  }

  // every loaded copy of the channel listens on the same window: drop them after each test
  let listeners: EventListener[]

  beforeEach(() => {
    const frame = document.createElement('iframe')
    document.body.appendChild(frame)
    host = frame.contentWindow!
    posts = vi.spyOn(host, 'postMessage').mockImplementation(() => {})
    Object.defineProperty(window, 'parent', { value: host, configurable: true })
    listeners = []
    const listen = window.addEventListener.bind(window)
    vi.spyOn(window, 'addEventListener').mockImplementation((type, listener, options) => {
      if (type === 'message') listeners.push(listener as EventListener)
      listen(type, listener, options)
    })
  })

  afterEach(() => {
    vi.mocked(window.addEventListener).mockRestore()
    for (const listener of listeners) window.removeEventListener('message', listener)
    vi.unstubAllGlobals()
    history.replaceState(null, '', '/')
    Object.defineProperty(window, 'parent', { value: window, configurable: true })
    document.body.replaceChildren()
  })

  it('reads the same-origin file next to the app folders, never cached', async () => {
    const { opening } = await load(async () => config(JSON.stringify({ origins: [LISTED] })))
    fromHost(OWN)
    await opening
    const [url, init] = fetchMock.mock.calls[0] as [URL, RequestInit]
    expect(url.href).toBe(new URL('../embed-origins.json', location.href).href)
    expect(url.origin).toBe(OWN)
    expect(init).toMatchObject({ cache: 'no-store', redirect: 'error' })
  })

  it('holds an open from a listed origin until the file arrives, then takes it', async () => {
    let answer!: (response: Response) => void
    const { opening } = await load(() => new Promise<Response>((resolve) => (answer = resolve)))
    fromHost(LISTED)
    expect(await settled(opening)).toBe(false)
    answer(config(JSON.stringify({ origins: [LISTED] })))
    await expect(opening).resolves.toMatchObject({ fileName: 'memo.hwpx' })
  })

  it('holds an open from the own origin the same way', async () => {
    let answer!: (response: Response) => void
    const { opening } = await load(() => new Promise<Response>((resolve) => (answer = resolve)))
    fromHost(OWN)
    expect(await settled(opening)).toBe(false)
    answer(config(JSON.stringify({ origins: [] })))
    await expect(opening).resolves.toMatchObject({ fileName: 'memo.hwpx' })
  })

  it('keeps the arrival order: the first allowed open wins', async () => {
    let answer!: (response: Response) => void
    const { opening } = await load(() => new Promise<Response>((resolve) => (answer = resolve)))
    fromHost(OTHER, 'evil.hwpx')
    fromHost(LISTED, 'first.hwpx')
    fromHost(OWN, 'second.hwpx')
    answer(config(JSON.stringify({ origins: [LISTED] })))
    await expect(opening).resolves.toMatchObject({ fileName: 'first.hwpx' })
  })

  it('ignores an unlisted origin, also after the file arrived', async () => {
    const { opening } = await load(async () => config(JSON.stringify({ origins: [LISTED] })))
    fromHost(OTHER)
    expect(await settled(opening)).toBe(false)
    expect(refusals()).toEqual([[REFUSAL, { targetOrigin: OTHER }]])
    fromHost(OTHER)
    expect(await settled(opening)).toBe(false)
    fromHost(LISTED)
    await expect(opening).resolves.toMatchObject({ fileName: 'memo.hwpx' })
  })

  it('needs the exact origin: another port, scheme or subdomain is not listed', async () => {
    const { opening } = await load(async () => config(JSON.stringify({ origins: [LISTED] })))
    for (const origin of [
      'https://yuyu.example:8443',
      'http://yuyu.example',
      'https://www.yuyu.example',
      'null',
    ]) {
      fromHost(origin)
    }
    expect(await settled(opening)).toBe(false)
  })

  it('answers a refused open with the code alone, to the origin that sent it', async () => {
    const { opening } = await load(async () => config(JSON.stringify({ origins: [LISTED] })))
    fromHost(OTHER, 'secret-plan.hwpx')
    await settled(opening)
    const [[message, options]] = refusals()
    expect(message).toStrictEqual(REFUSAL)
    expect(JSON.stringify(message)).not.toContain('secret-plan')
    expect(JSON.stringify(message)).not.toContain(LISTED)
    expect(options).toStrictEqual({ targetOrigin: OTHER })
  })

  it('does not answer an opaque origin: there is no target for it', async () => {
    const { opening } = await load(async () => config(JSON.stringify({ origins: [LISTED] })))
    fromHost('null')
    await settled(opening)
    expect(refusals()).toEqual([])
  })

  it('does not answer a message that is not an open', async () => {
    const { opening } = await load(async () => config(JSON.stringify({ origins: [LISTED] })))
    window.dispatchEvent(
      new MessageEvent('message', {
        source: host,
        origin: OTHER,
        data: { source: 'seuoffice', version: 1, type: 'saved', saveId: 1 },
      }),
    )
    window.dispatchEvent(
      new MessageEvent('message', { source: host, origin: OTHER, data: { type: 'open' } }),
    )
    await settled(opening)
    expect(refusals()).toEqual([])
  })

  it.each([
    ['a missing file', () => Promise.resolve(config('not found', 404))],
    ['a redirect-less 204', () => Promise.resolve(config(null, 204))],
    ['a server error', () => Promise.resolve(config('{"origins":["https://yuyu.example"]}', 500))],
    ['a failed fetch', () => Promise.reject(new TypeError('Failed to fetch'))],
    ['a page instead of JSON', () => Promise.resolve(config('<!doctype html><title>app</title>'))],
    ['broken JSON', () => Promise.resolve(config('{"origins": ['))],
    ['the wrong shape', () => Promise.resolve(config(JSON.stringify([LISTED])))],
    ['a wildcard', () => Promise.resolve(config(JSON.stringify({ origins: ['*'] })))],
    [
      'one bad entry next to a good one',
      () => Promise.resolve(config(JSON.stringify({ origins: [LISTED, 'null'] }))),
    ],
  ])('%s leaves only the own origin', async (_name, respond) => {
    vi.spyOn(console, 'warn').mockImplementation(() => {})
    const { opening } = await load(respond)
    fromHost(LISTED)
    expect(await settled(opening)).toBe(false)
    expect(refusals()).toEqual([[REFUSAL, { targetOrigin: LISTED }]])
    fromHost(OWN)
    await expect(opening).resolves.toMatchObject({ fileName: 'memo.hwpx' })
  })

  it('does not read origins from the page address', async () => {
    history.replaceState(
      null,
      '',
      `/?origins=${encodeURIComponent(LISTED)}&embed-origins=${LISTED}#${LISTED}`,
    )
    window.name = JSON.stringify({ origins: [LISTED] })
    const { opening } = await load(async () => config('not found', 404))
    fromHost(LISTED)
    expect(await settled(opening)).toBe(false)
    window.name = ''
  })

  it('does not take origins from a message', async () => {
    const { opening } = await load(async () => config('not found', 404))
    window.dispatchEvent(
      new MessageEvent('message', {
        source: host,
        origin: LISTED,
        data: {
          source: 'seuoffice',
          version: 1,
          type: 'open',
          fileName: 'memo.hwpx',
          bytes: new ArrayBuffer(1),
          origins: [LISTED],
          allowedOrigins: [LISTED],
        },
      }),
    )
    expect(await settled(opening)).toBe(false)
    expect(refusals()).toEqual([[REFUSAL, { targetOrigin: LISTED }]])
  })
})
