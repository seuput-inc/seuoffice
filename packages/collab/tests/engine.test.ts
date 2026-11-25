import { describe, expect, it } from 'vitest'
import { CollabError, type CollabLink, type CollabLinkEvent } from '../src/client/api'
import { startEngine } from '../src/client/join'
import { VirtualClock } from '../src/conformance/network'
import { toyLines } from '../src/conformance/toys'
import type { ServerFrame } from '../src/wire'

/** The page's side of a connection to a broken server that says whatever the test makes it say */
class StubLink implements CollabLink {
  opens = 0
  readonly sent: (string | Uint8Array)[] = []
  readonly #handlers = new Set<(event: CollabLinkEvent) => void>()
  #state: 'idle' | 'connecting' | 'open' = 'idle'

  open(): void {
    if (this.#state !== 'idle') return
    this.#state = 'connecting'
    this.opens += 1
  }

  send(frame: string | Uint8Array): void {
    if (this.#state !== 'open') throw new Error('sent outside an open connection')
    this.sent.push(frame)
  }

  close(): void {
    this.#state = 'idle'
  }

  subscribe(handler: (event: CollabLinkEvent) => void): () => void {
    this.#handlers.add(handler)
    return () => this.#handlers.delete(handler)
  }

  accept(): void {
    this.#state = 'open'
    this.#emit({ type: 'open' })
  }

  deliver(frame: object | string): void {
    this.#emit({ type: 'frame', frame: typeof frame === 'string' ? frame : JSON.stringify(frame) })
  }

  /** the host or the network ended the connection */
  hangUp(): void {
    this.#state = 'idle'
    this.#emit({ type: 'close' })
  }

  lastSent(): unknown {
    const text = this.sent.filter((frame) => typeof frame === 'string').at(-1)
    return text === undefined ? undefined : JSON.parse(text)
  }

  presences(): { t: 'presence'; v: number; at: unknown }[] {
    return this.sent
      .filter((frame) => typeof frame === 'string')
      .map((text) => JSON.parse(text))
      .filter((frame) => frame.t === 'presence')
  }

  #emit(event: CollabLinkEvent): void {
    for (const handler of [...this.#handlers]) handler(event)
  }
}

const welcome = (start: number, head = start) => ({
  t: 'welcome',
  epoch: 'e1',
  client: 'c1',
  name: 'Me',
  start,
  head,
  saved: 0,
  canEdit: true,
  peers: [],
})

const change = (v: number) => ({
  t: 'change',
  v,
  client: 'other',
  seq: v,
  author: 'other',
  ops: [{ id: `o${v}`, t: 'ins', at: 0, text: `o${v}` }],
})

/** a change past the next version: a gap no correct server leaves */
const gap = change(99)

async function started() {
  const link = new StubLink()
  const clock = new VirtualClock()
  const engine = startEngine(link, { format: toyLines.format, bytes: toyLines.file() }, clock)
  await engine.settled()
  return { link, clock, engine }
}

/** fire timers until the engine opens its next connection; false when it never does */
function reconnected(link: StubLink, clock: VirtualClock): boolean {
  const opens = link.opens
  while (link.opens === opens) if (!clock.fire()) return false
  return true
}

async function joined(overrides: Partial<Extract<ServerFrame, { t: 'welcome' }>> = {}) {
  const { link, clock, engine } = await started()
  link.accept()
  link.deliver({ ...welcome(0), ...overrides })
  const editor = toyLines.open(toyLines.file())
  const session = (await engine.join).attach(editor)
  expect(session.status().phase).toBe('live')
  return { link, clock, session, editor }
}

describe('a server that keeps breaking the protocol', () => {
  it('before any welcome: the third broken connection rejects the join with reset', async () => {
    const { link, clock, engine } = await started()
    const outcome = engine.join.then(
      () => null,
      (error: unknown) => error,
    )
    for (let i = 0; i < 3; i++) {
      if (i > 0) expect(reconnected(link, clock)).toBe(true)
      link.accept()
      expect(link.lastSent()).toMatchObject({ t: 'hello' })
      link.deliver('not a frame')
    }
    const error = await outcome
    expect(error).toBeInstanceOf(CollabError)
    expect((error as CollabError).code).toBe('reset')
    expect(reconnected(link, clock)).toBe(false)
  })

  it('after joining: the third broken connection in a row fails the session with reset', async () => {
    const { link, clock, session } = await joined()
    for (let i = 0; i < 3; i++) {
      if (i > 0) {
        expect(reconnected(link, clock)).toBe(true)
        link.accept()
        link.deliver(welcome(0))
      }
      link.deliver(gap)
    }
    expect(session.status()).toMatchObject({ phase: 'failed', failure: 'reset' })
    expect(reconnected(link, clock)).toBe(false)
  })

  it('a change that gets through starts the count over', async () => {
    const { link, clock, session } = await joined()
    link.deliver(gap)
    expect(reconnected(link, clock)).toBe(true)
    link.accept()
    link.deliver(welcome(0))
    link.deliver(gap)

    expect(reconnected(link, clock)).toBe(true)
    link.accept()
    link.deliver(welcome(0, 1))
    link.deliver(change(1))
    link.deliver(gap)
    for (let i = 0; i < 2; i++) {
      expect(reconnected(link, clock)).toBe(true)
      link.accept()
      link.deliver(welcome(1))
      link.deliver(gap)
    }
    expect(session.status()).toMatchObject({ phase: 'offline', version: 1 })

    expect(reconnected(link, clock)).toBe(true)
    link.accept()
    link.deliver(welcome(1))
    link.deliver(gap)
    expect(session.status()).toMatchObject({ phase: 'failed', failure: 'reset' })
  })
})

describe('a frame from a socket the engine gave up', () => {
  it('is nothing, a superseded included, and the next hello names a newer attempt', async () => {
    const { link, clock, session } = await joined()
    link.hangUp()
    link.deliver({ t: 'error', code: 'superseded' })
    link.deliver('not a frame')
    expect(session.status()).toMatchObject({ phase: 'offline' })

    expect(reconnected(link, clock)).toBe(true)
    link.accept()
    expect(link.lastSent()).toMatchObject({
      t: 'hello',
      resume: { epoch: 'e1', client: 'c1', v: 0, attempt: 2 },
    })
    link.deliver(welcome(0))
    expect(session.status().phase).toBe('live')
  })
})

describe('an error the server explains', () => {
  const reason = 'HWP source or edit cannot be replayed: the renderer refused it (unsupported)'

  it('rejects the join with the sentence', async () => {
    const { link, engine } = await started()
    const outcome = engine.join.then(
      () => null,
      (error: unknown) => error,
    )
    link.accept()
    link.deliver({ t: 'error', code: 'schema_mismatch', reason })
    const error = (await outcome) as CollabError
    expect(error.code).toBe('schema_mismatch')
    expect(error.message).toContain(reason)
  })

  it('ends a live session with the sentence in its status', async () => {
    const { link, session } = await joined()
    link.deliver({ t: 'error', code: 'too_large', reason })
    expect(session.status()).toMatchObject({ phase: 'failed', failure: 'adapter', detail: reason })
  })
})

describe('live selections', () => {
  const peer = { client: '__proto__', name: 'Peer' }
  const capabilities = { presence: true as const, peers: [peer] }
  const remote = (v: number, at: unknown) => ({ t: 'presence', client: peer.client, v, at })

  it('offers presence but does not send it to a server that welcomes without it', async () => {
    const { link, session, editor } = await joined()
    expect(JSON.parse(String(link.sent[0]))).toMatchObject({ t: 'hello', p: 1, presence: true })
    session.setPresence(3)
    expect(link.presences()).toEqual([])
    editor.perform({ id: 'one', t: 'del', at: 0 })
    expect(link.lastSent()).toMatchObject({ t: 'submit', seq: 1, base: 0 })
    link.deliver({ t: 'ack', seq: 1, v: 1 })
    expect(session.status()).toMatchObject({ phase: 'live', version: 1, presences: {} })
  })

  it('lets a viewer send once per 100 ms and snapshots only the latest valid selection', async () => {
    const { link, clock, session } = await joined({ presence: true, canEdit: false })
    session.setPresence(1)
    const at = { point: 0 }
    for (let n = 1; n <= 200; n++) {
      at.point = n
      session.setPresence(at)
    }
    at.point = -1
    expect(link.presences()).toEqual([{ t: 'presence', v: 0, at: 1 }])
    expect(clock.fire()).toBe(true)
    expect(clock.now()).toBe(100)
    expect(link.presences().map((f) => f.at)).toEqual([1, { point: 200 }])
    session.setPresence('é'.repeat(2048))
    expect(clock.fire()).toBe(true)
    expect(link.presences().at(-1)).toEqual({ t: 'presence', v: 0, at: null })
    expect(session.status()).toMatchObject({ phase: 'live', unconfirmed: 0, version: 0, saved: 0 })
  })

  it('keeps only negotiated remote peers at the displayed version, and removes selections on clear, leave and close', async () => {
    const { link, session } = await joined(capabilities)
    const max = 'x'.repeat(4096 - JSON.stringify({ t: 'presence', v: 0, at: '' }).length)
    link.deliver(remote(0, max))
    expect(session.status().presences[peer.client]?.at).toBe(max)
    link.deliver(remote(0, { x: 1 }))
    expect(session.status().presences[peer.client]).toEqual({ v: 0, at: { x: 1 } })
    link.deliver(remote(1, 'future'))
    link.deliver({ ...remote(0, 'unknown'), client: 'toString' })
    link.deliver({ ...remote(0, 'self'), client: 'c1' })
    link.deliver({ ...remote(0, 'extra key'), extra: ' '.repeat(4096) })
    link.deliver(' '.repeat(4096) + JSON.stringify(remote(0, 'over budget')))
    link.deliver(
      `{"t":"presence","client":"__proto__","v":0,"at":${'['.repeat(1500)}0${']'.repeat(1500)}}`,
    )
    expect(Object.entries(session.status().presences)).toEqual([
      [peer.client, { v: 0, at: { x: 1 } }],
    ])
    link.deliver(remote(0, null))
    expect(session.status().presences[peer.client]).toBeUndefined()
    link.deliver(remote(0, 2))
    link.deliver(change(1))
    link.deliver(remote(0, 'stale'))
    expect(session.status().presences[peer.client]).toBeUndefined()
    link.deliver(remote(1, 'live'))
    link.deliver({ t: 'peers', peers: [] })
    expect(Object.keys(session.status().presences)).toEqual([])
    link.deliver({ t: 'peers', peers: [peer] })
    link.deliver(remote(1, 'back'))
    session.close()
    expect(Object.keys(session.status().presences)).toEqual([])
  })

  it('never publishes a queued selection under a newer version or over a pending local edit', async () => {
    const { link, clock, session, editor } = await joined(capabilities)
    session.setPresence('first')
    session.setPresence('queued at zero')
    link.deliver(remote(0, 'theirs'))
    editor.perform({ id: 'one', t: 'del', at: 0 })
    expect(session.status().unconfirmed).toBe(1)
    expect(Object.keys(session.status().presences)).toEqual([])
    session.setPresence('in the optimistic view')
    expect(clock.fire()).toBe(true)
    link.deliver({ t: 'ack', seq: 1, v: 1 })
    expect(session.status()).toMatchObject({ unconfirmed: 0, version: 1 })
    expect(Object.keys(session.status().presences)).toEqual([])
    expect(link.presences()).toEqual([{ t: 'presence', v: 0, at: 'first' }])
    session.setPresence('from confirmed view')
    expect(link.presences().at(-1)).toEqual({ t: 'presence', v: 1, at: 'from confirmed view' })
  })

  it('replays the latest same-version selection after welcome, never while offline or after catching up to another version', async () => {
    const { link, clock, session } = await joined(capabilities)
    session.setPresence('original')
    link.deliver(remote(0, 'theirs'))
    link.hangUp()
    expect(Object.keys(session.status().presences)).toEqual([])
    session.setPresence('offline')
    expect(reconnected(link, clock)).toBe(true)
    link.accept()
    expect(link.presences()).toHaveLength(1)
    link.deliver({ ...welcome(0), ...capabilities })
    expect(link.presences().map((f) => f.at)).toEqual(['original', 'offline'])
    link.hangUp()
    expect(reconnected(link, clock)).toBe(true)
    link.accept()
    link.deliver({ ...welcome(0, 1), ...capabilities })
    link.deliver(change(1))
    expect(link.presences()).toHaveLength(2)
    session.setPresence('fresh at one')
    expect(link.presences().at(-1)).toEqual({ t: 'presence', v: 1, at: 'fresh at one' })
    link.hangUp()
    expect(reconnected(link, clock)).toBe(true)
    link.accept()
    link.deliver(welcome(1))
    session.setPresence('legacy server')
    expect(link.presences()).toHaveLength(3)
    link.hangUp()
    expect(reconnected(link, clock)).toBe(true)
    link.accept()
    link.deliver({ ...welcome(1), ...capabilities })
    expect(link.presences().at(-1)).toEqual({ t: 'presence', v: 1, at: 'legacy server' })
  })
})
