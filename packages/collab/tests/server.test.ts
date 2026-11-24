import { describe, expect, it, vi } from 'vitest'
import { VirtualClock } from '../src/conformance/network'
import { sha256 } from '../src/sha256'
import { createServerCore, type Principal, type ServerSocket } from '../src/server/server'
import { memoryHistoryStore, memoryStore } from '../src/server/store'

const FORMAT = { name: 'toy', schema: 1 }
const BASE = new TextEncoder().encode('base file')

class FakeSocket implements ServerSocket {
  readonly sent: (string | Uint8Array)[] = []
  closed = false
  #message: (frame: string | Uint8Array) => void = () => {}
  #close: () => void = () => {}

  send(frame: string | Uint8Array): void {
    this.sent.push(frame)
  }

  close(): void {
    this.closed = true
  }

  onMessage(handler: (frame: string | Uint8Array) => void): void {
    this.#message = handler
  }

  onClose(handler: () => void): void {
    this.#close = handler
  }

  receive(frame: string | Uint8Array | object): void {
    this.#message(
      typeof frame === 'string' || frame instanceof Uint8Array ? frame : JSON.stringify(frame),
    )
  }

  hangUp(): void {
    this.#close()
  }

  frames(): Record<string, unknown>[] {
    return this.sent.filter((frame) => typeof frame === 'string').map((frame) => JSON.parse(frame))
  }

  presences(): Record<string, unknown>[] {
    return this.frames().filter((frame) => frame.t === 'presence')
  }

  /** the client and epoch this socket was welcomed with */
  welcomed(): { client: string; epoch: string } {
    const welcome = this.frames().find((frame) => frame.t === 'welcome')
    if (typeof welcome?.client !== 'string' || typeof welcome.epoch !== 'string')
      throw new Error('not welcomed')
    return { client: welcome.client, epoch: welcome.epoch }
  }
}

function principal(userId: string, canEdit = true): Principal {
  return { documentId: 'doc', userId, name: userId, canEdit }
}

async function setup(limits = {}, store = memoryStore()) {
  const clock = new VirtualClock()
  const server = createServerCore({ store, limits, clock })
  const fileHash = await sha256(BASE)
  const hello = { t: 'hello', p: 1, format: FORMAT, fileHash }

  async function open(
    user = 'a',
    canEdit = true,
    documentId = 'doc',
    presence = true,
  ): Promise<FakeSocket> {
    const socket = new FakeSocket()
    server.accept(socket, { ...principal(user, canEdit), documentId })
    socket.receive(presence ? { ...hello, presence: true } : hello)
    await server.idle()
    if (socket.frames().at(-1)?.t === 'need_base') {
      socket.receive(BASE)
      await server.idle()
    }
    return socket
  }

  return { server, store, clock, hello, open }
}

describe('the server', () => {
  it('answers a malformed frame and a frame before hello with bad_frame and closes', async () => {
    const { server } = await setup()
    for (const frame of [
      'not json',
      { t: 'submit', seq: 1, base: 0, ops: [] },
      { t: 'hello', p: 2 },
    ]) {
      const socket = new FakeSocket()
      server.accept(socket, principal('a'))
      socket.receive(frame)
      await server.idle()
      expect(socket.frames()).toEqual([{ t: 'error', code: 'bad_frame' }])
      expect(socket.closed).toBe(true)
    }
  })

  it('refuses a base whose hash is not the announced one', async () => {
    const { server, hello } = await setup()
    const socket = new FakeSocket()
    server.accept(socket, principal('a'))
    socket.receive(hello)
    await server.idle()
    socket.receive(new TextEncoder().encode('other bytes'))
    await server.idle()
    expect(socket.frames()).toEqual([{ t: 'need_base' }, { t: 'error', code: 'bad_frame' }])
  })

  it('answers too_large for a base past its limit', async () => {
    const { server, hello } = await setup({ maxBaseBytes: 4 })
    const seeder = new FakeSocket()
    server.accept(seeder, principal('a'))
    seeder.receive(hello)
    await server.idle()
    seeder.receive(BASE)
    await server.idle()
    expect(seeder.frames()).toEqual([{ t: 'need_base' }, { t: 'error', code: 'too_large' }])
    expect(seeder.closed).toBe(true)
  })

  it('answers too_large for a text frame past its limit', async () => {
    const { server, open } = await setup({ maxChangeBytes: 100 })
    const socket = await open()
    socket.receive({ t: 'submit', seq: 1, base: 0, ops: ['x'.repeat(100)] })
    await server.idle()
    expect(socket.frames().at(-1)).toEqual({ t: 'error', code: 'too_large' })
    expect(socket.closed).toBe(true)
  })

  it('forbids a submit from a principal that cannot edit and leaves the log alone', async () => {
    const { server, open } = await setup()
    const editor = await open('editor')
    const viewer = await open('viewer', false)
    expect(viewer.frames().find((frame) => frame.t === 'welcome')).toMatchObject({ canEdit: false })

    viewer.receive({ t: 'submit', seq: 1, base: 0, ops: [1] })
    await server.idle()
    expect(viewer.frames().at(-1)).toEqual({ t: 'error', code: 'forbidden' })
    expect(editor.frames().some((frame) => frame.t === 'change')).toBe(false)
  })

  it('sends a joiner every change after its start, in order, across catch-up pages', async () => {
    const { server, open } = await setup({ catchUpPage: 2 })
    const writer = await open('writer')
    for (let seq = 1; seq <= 5; seq++) {
      writer.receive({ t: 'submit', seq, base: seq - 1, ops: [seq] })
      await server.idle()
    }
    expect(
      writer
        .frames()
        .filter((frame) => frame.t === 'ack')
        .map((frame) => frame.v),
    ).toEqual([1, 2, 3, 4, 5])

    const reader = await open('reader')
    const welcome = reader.frames().find((frame) => frame.t === 'welcome')
    expect(welcome).toMatchObject({ start: 0, head: 5 })
    const changes = reader.frames().filter((frame) => frame.t === 'change')
    expect(changes.map((frame) => frame.v)).toEqual([1, 2, 3, 4, 5])
    expect(changes.map((frame) => frame.ops)).toEqual([[1], [2], [3], [4], [5]])
  })

  it('acks a submit already in the log without appending it again, and answers a stale base with stale', async () => {
    const { server, open } = await setup()
    const a = await open('a')
    const b = await open('b')
    a.receive({ t: 'submit', seq: 1, base: 0, ops: ['first'] })
    await server.idle()
    a.receive({ t: 'submit', seq: 1, base: 0, ops: ['first'] })
    b.receive({ t: 'submit', seq: 1, base: 0, ops: ['late'] })
    await server.idle()
    expect(a.frames().filter((frame) => frame.t === 'ack')).toEqual([
      { t: 'ack', v: 1, seq: 1 },
      { t: 'ack', v: 1, seq: 1 },
    ])
    expect(b.frames().slice(-2)).toMatchObject([
      { t: 'change', v: 1, ops: ['first'] },
      { t: 'stale', seq: 1, base: 0, head: 1 },
    ])
  })

  it('keeps the socket with the highest connection attempt of a client, whatever the accept and hello order', async () => {
    const { server, hello, open } = await setup()
    const first = await open('a')
    const { client, epoch } = first.welcomed()
    const resume = (attempt: number) => ({ ...hello, resume: { epoch, client, v: 0, attempt } })

    // Accepted before the attempt it lost to, and its hello read after
    const third = new FakeSocket()
    server.accept(third, principal('a'))
    const second = new FakeSocket()
    server.accept(second, principal('a'))
    third.receive(resume(3))
    await server.idle()
    expect(first.frames().at(-1)).toEqual({ t: 'error', code: 'superseded' })
    second.receive(resume(2))
    await server.idle()
    expect(second.frames()).toEqual([{ t: 'error', code: 'superseded' }])
    expect(third.closed).toBe(false)

    // A newer attempt takes over from the welcomed one
    const fourth = new FakeSocket()
    server.accept(fourth, principal('a'))
    fourth.receive(resume(4))
    await server.idle()
    expect(third.frames().at(-1)).toEqual({ t: 'error', code: 'superseded' })
    expect(fourth.welcomed()).toEqual({ client, epoch })
  })

  it('keeps an epoch its last socket left, so a resume continues it, until it idled out saved', async () => {
    const { server, store, clock, hello, open } = await setup()
    const socket = await open('a')
    const { client, epoch } = socket.welcomed()
    const resume = { ...hello, resume: { epoch, client, v: 0, attempt: 2 } }
    socket.hangUp()
    await server.idle()

    const back = new FakeSocket()
    server.accept(back, principal('a'))
    back.receive(resume)
    await server.idle()
    expect(back.welcomed()).toEqual({ client, epoch })
    back.hangUp()
    await server.idle()

    expect(clock.fire()).toBe(true)
    await server.idle()
    expect(await store.load('doc')).toBeNull()
    const late = new FakeSocket()
    server.accept(late, principal('a'))
    late.receive(resume)
    await server.idle()
    expect(late.frames()).toEqual([{ t: 'error', code: 'reset' }])
  })

  it('keeps an idle epoch with unsaved versions however long it waits', async () => {
    const { server, store, clock, open } = await setup({ idleEpochMs: 1000 })
    const socket = await open('a')
    socket.receive({ t: 'submit', seq: 1, base: 0, ops: ['x'] })
    await server.idle()
    socket.hangUp()
    await server.idle()
    while (clock.fire()) await server.idle()
    expect(await store.load('doc')).toMatchObject({ head: 1, saved: 0 })
  })

  it("treats a resume of another user's client id as a fresh hello", async () => {
    const { server, hello, open } = await setup()
    const owner = await open('owner')
    const { client, epoch } = owner.welcomed()

    const other = new FakeSocket()
    server.accept(other, principal('other'))
    other.receive({ ...hello, resume: { epoch, client, v: 0, attempt: 2 } })
    await server.idle()
    expect(other.welcomed().client).not.toBe(client)
    expect(owner.closed).toBe(false)
    expect(owner.frames().some((frame) => frame.t === 'error')).toBe(false)
  })

  it('ends a document on reset: its sockets get reset and its epoch is dropped', async () => {
    const { server, store, open } = await setup()
    const a = await open('a')
    a.receive({ t: 'submit', seq: 1, base: 0, ops: ['x'] })
    await server.idle()
    await server.reset('doc')
    expect(a.frames().at(-1)).toEqual({ t: 'error', code: 'reset' })
    expect(a.closed).toBe(true)
    expect(await store.load('doc')).toBeNull()
  })

  it('closes every socket on close(), keeps the epochs for a later start, and turns later sockets away', async () => {
    const { server, store, open } = await setup()
    const a = await open('a')
    const b = await open('b')
    await server.close()
    expect([a.closed, b.closed]).toEqual([true, true])
    expect(await store.load('doc')).not.toBeNull()

    const late = new FakeSocket()
    server.accept(late, principal('c'))
    expect(late.closed).toBe(true)
  })
})

describe('live selections', () => {
  it('relays a viewer to other negotiated sockets in its document, including late joiners, without touching the log', async () => {
    const history = memoryHistoryStore(() => true)
    const { server, store, clock, open } = await setup({}, history)
    const editor = await open('editor')
    const legacy = await open('old editor', true, 'doc', false)
    const elsewhere = await open('elsewhere', true, 'another')
    const viewer = await open('viewer', false)
    const { client, epoch } = viewer.welcomed()
    const before = await store.load('doc')
    const point = { epoch, version: 0 }
    const prefix = await history.readHistory(principal('editor'), point, 'export')
    const at = { from: 2, to: 4 }
    viewer.receive({ t: 'presence', client: 'spoof', v: 0, at })
    await server.idle()
    const mark = { t: 'presence', client, v: 0, at }
    expect(editor.presences()).toEqual([mark])
    const late = await open('late', false)
    expect(late.frames()[0]).toMatchObject({ t: 'welcome', presence: true })
    expect(late.presences()).toEqual([mark])
    expect((await open('late legacy', false, 'doc', false)).presences()).toEqual([])
    expect(legacy.frames()[0]).not.toHaveProperty('presence')
    legacy.receive({ t: 'presence', v: 0, at: 'not negotiated' })
    viewer.receive({ t: 'presence', v: 0, at: null })
    await server.idle()
    expect(clock.fire()).toBe(true)
    await server.idle()
    expect(editor.presences()).toEqual([mark, { ...mark, at: null }])
    expect((await open('after clear')).presences()).toEqual([])
    for (const socket of [viewer, legacy, elsewhere]) expect(socket.presences()).toEqual([])
    expect(await store.load('doc')).toEqual(before)
    expect(await store.read('doc', epoch, 0, 10)).toEqual([])
    expect(await store.readBase('doc', epoch)).toEqual(BASE)
    expect(await history.readHistory(principal('editor'), point, 'export')).toEqual(prefix)
    legacy.receive({ t: 'submit', seq: 1, base: 0, ops: ['still editable'] })
    await server.idle()
    expect(legacy.frames().at(-1)).toEqual({ t: 'ack', seq: 1, v: 1 })
  })

  it('keeps the latest motion per socket for each 100 ms and does not use the edit inbox budget', async () => {
    const { server, clock, open } = await setup({ maxQueuedFrames: 2 })
    const a = await open('a')
    const b = await open('b', false)
    const witness = await open('witness')
    const { client } = a.welcomed()
    const first = { t: 'presence', v: 0, at: 0 }
    a.receive(first)
    await server.idle()
    for (let at = 1; at <= 300; at++) a.receive({ ...first, at })
    b.receive({ ...first, at: 'independent' })
    await server.idle()
    const leading = [
      { ...first, client },
      { ...first, client: b.welcomed().client, at: 'independent' },
    ]
    expect(witness.presences()).toEqual(leading)
    expect((await open('mid-burst')).presences()).toContainEqual({ ...first, client, at: 300 })
    expect(clock.fire()).toBe(true)
    await server.idle()
    expect(clock.now()).toBe(100)
    expect(witness.presences()).toEqual([...leading, { ...first, client, at: 300 }])
    expect(a.closed).toBe(false)
  })

  it('drops deep, future and stale positions without disconnecting or relabeling them', async () => {
    const { server, clock, open } = await setup()
    const a = await open('a')
    const witness = await open('witness')
    const room = 4096 - JSON.stringify({ t: 'presence', v: 0, at: '' }).length
    const max = 'é'.repeat(Math.floor(room / 2)) + 'x'.repeat(room % 2)
    a.receive({ t: 'presence', v: 0, at: max })
    await server.idle()
    a.receive(`{"t":"presence","v":0,"at":${'['.repeat(1500)}0${']'.repeat(1500)}}`)
    a.receive({ t: 'presence', v: 1, at: 'future' })
    expect(clock.fire()).toBe(false)
    a.receive({ t: 'presence', v: 0, at: 'queued at zero' })
    a.receive({ t: 'submit', seq: 1, base: 0, ops: [1] })
    await server.idle()
    a.receive({ t: 'presence', v: 0, at: 'stale' })
    expect(clock.fire()).toBe(true)
    await server.idle()
    expect(witness.presences().map((f) => f.at)).toEqual([max])
    expect((await open('late')).presences()).toEqual([])
    a.receive({ t: 'presence', v: 1, at: 7 })
    await server.idle()
    expect(witness.presences().at(-1)).toEqual({
      t: 'presence',
      v: 1,
      client: a.welcomed().client,
      at: 7,
    })
    expect(a.closed).toBe(false)
  })

  it('refuses a wire position one byte over the UTF-8 limit, without forwarding it', async () => {
    const { server, open } = await setup()
    const editor = await open()
    const viewer = await open('viewer', false)
    const frame = JSON.stringify({ t: 'presence', v: 0, at: 'é' })
    viewer.receive(frame.padEnd(4096))
    await server.idle()
    expect(viewer.frames().at(-1)).toEqual({ t: 'error', code: 'bad_frame' })
    expect(viewer.closed).toBe(true)
    expect(editor.presences().every((position) => position.at === null)).toBe(true)
  })

  it('clears each departing socket once and its delayed messages cannot erase its replacement', async () => {
    const { server, hello, clock, open } = await setup()
    const old = await open('a')
    const witness = await open('witness')
    const { client, epoch } = old.welcomed()
    const frame = { t: 'presence', v: 0, at: 'old' }
    old.receive(frame)
    await server.idle()
    old.receive({ ...frame, at: 'queued' })
    const replacement = new FakeSocket()
    server.accept(replacement, principal('a'))
    replacement.receive({ ...hello, presence: true, resume: { client, epoch, v: 0, attempt: 2 } })
    await server.idle()
    replacement.receive({ ...frame, at: 'new' })
    old.hangUp()
    old.receive({ ...frame, at: 'lost socket' })
    await server.idle()
    expect(witness.presences().map((f) => f.at)).toEqual(['old', null, 'new'])
    expect((await open('late')).presences()).toEqual([{ ...frame, client, at: 'new' }])
    replacement.receive({ ...frame, at: 'queued' })
    replacement.hangUp()
    replacement.hangUp()
    await server.idle()
    expect(clock.fire()).toBe(false)
    expect(witness.presences().map((f) => f.at)).toEqual(['old', null, 'new', null])
    expect((await open('after leave')).presences()).toEqual([])
  })

  it('delivers one last clear to the remaining socket when the server closes', async () => {
    const { server, open } = await setup()
    const first = await open()
    const last = await open('last')
    first.receive({ t: 'presence', v: 0, at: 1 })
    await server.idle()
    await server.close()
    first.hangUp()
    await server.idle()
    expect(last.presences().map((f) => f.at)).toEqual([1, null])
  })
})

describe('fingerprints', () => {
  it('come only from editors, and a client that reports twice at one version is not compared with itself', async () => {
    const { server, store, open } = await setup()
    const editor = await open('editor')
    const viewer = await open('viewer', false)
    editor.receive({ t: 'fingerprint', v: 0, fp: 'first' })
    editor.receive({ t: 'fingerprint', v: 0, fp: 'second' })
    viewer.receive({ t: 'fingerprint', v: 0, fp: 'viewer' })
    await server.idle()
    for (const socket of [editor, viewer]) {
      expect(socket.closed).toBe(false)
      expect(socket.frames().some((frame) => frame.t === 'diverged')).toBe(false)
    }
    expect(await store.load('doc')).not.toBeNull()

    const other = await open('other')
    other.receive({ t: 'fingerprint', v: 0, fp: 'third' })
    await server.idle()
    expect(editor.frames().at(-1)).toEqual({ t: 'diverged', v: 0 })
    expect(await store.load('doc')).toBeNull()
  })
})

describe('seeding', () => {
  it('asks only an editor for the base; a viewer waits until the epoch exists', async () => {
    const { server, hello, open } = await setup()
    const viewer = new FakeSocket()
    server.accept(viewer, principal('viewer', false))
    viewer.receive(hello)
    await server.idle()
    expect(viewer.frames()).toEqual([])

    const editor = await open('editor')
    expect(editor.frames()[0]).toEqual({ t: 'need_base' })
    expect(viewer.welcomed().epoch).toBe(editor.welcomed().epoch)
  })

  it('refuses a seeder whose file is not the one the host names as current', async () => {
    const { server, hello, open } = await setup()
    const stranger = new FakeSocket()
    server.accept(stranger, { ...principal('a'), fileHash: 'f'.repeat(64) })
    stranger.receive(hello)
    await server.idle()
    expect(stranger.frames()).toEqual([{ t: 'error', code: 'forbidden' }])

    const current = new FakeSocket()
    server.accept(current, { ...principal('b'), fileHash: hello.fileHash })
    current.receive(hello)
    await server.idle()
    expect(current.frames()).toEqual([{ t: 'need_base' }])
    current.receive(BASE)
    await server.idle()

    // Once the epoch exists the hint plays no part: a joiner just opens the epoch's base
    const joiner = new FakeSocket()
    server.accept(joiner, { ...principal('c'), fileHash: 'f'.repeat(64) })
    joiner.receive(hello)
    await server.idle()
    expect(joiner.welcomed().epoch).toBe(current.welcomed().epoch)
    void open
  })

  it('hands seeding to the next waiting editor when the seeder sends no base within 30 s', async () => {
    const { server, clock, hello } = await setup()
    const silent = new FakeSocket()
    server.accept(silent, principal('a'))
    silent.receive(hello)
    const next = new FakeSocket()
    server.accept(next, principal('b'))
    next.receive(hello)
    await server.idle()
    expect(next.frames()).toEqual([])

    const start = clock.now()
    expect(clock.fire()).toBe(true)
    await server.idle()
    expect(clock.now() - start).toBe(30_000)
    expect(silent.closed).toBe(true)
    expect(next.frames()).toEqual([{ t: 'need_base' }])
  })
})

describe('a failing store', () => {
  it('costs only the socket whose work failed', async () => {
    const inner = memoryStore()
    let loads = 0
    const store = {
      ...inner,
      async load(documentId: string) {
        loads += 1
        if (loads === 5) throw new Error('store unavailable')
        return inner.load(documentId)
      },
    }
    const server = createServerCore({ store, clock: new VirtualClock() })
    const hello = { t: 'hello', p: 1, format: FORMAT, fileHash: await sha256(BASE) }
    const greeted = (user: string) => {
      const socket = new FakeSocket()
      server.accept(socket, principal(user))
      socket.receive(hello)
      return socket
    }
    const seeder = greeted('a')
    const unlucky = greeted('b')
    const lucky = greeted('c')
    await server.idle()
    seeder.receive(BASE)
    await server.idle()

    expect(seeder.closed).toBe(false)
    expect(seeder.welcomed()).toBeTruthy()
    expect(unlucky.closed).toBe(true)
    expect(lucky.welcomed().epoch).toBe(seeder.welcomed().epoch)
  })
})

describe('inbound frames', () => {
  it('keeps waiting edits encoded until their queue slot runs', async () => {
    const { server, open } = await setup()
    const socket = await open()
    const raw = JSON.stringify({ t: 'submit', seq: 1, base: 0, ops: ['a short edit'] }).padEnd(4100)
    const parse = vi.spyOn(JSON, 'parse')
    try {
      socket.receive(raw)
      expect(parse.mock.calls.filter(([input]) => input === raw)).toHaveLength(0)
      await server.idle()
      expect(socket.frames().at(-1)).toEqual({ t: 'ack', seq: 1, v: 1 })
    } finally {
      parse.mockRestore()
    }
  })

  it('closes a socket with more than 256 frames waiting', async () => {
    const { server, open } = await setup()
    const socket = await open('a')
    for (let i = 0; i < 256; i++) socket.receive({ t: 'fingerprint', v: 0, fp: 'same' })
    await server.idle()
    expect(socket.closed).toBe(false)

    for (let i = 0; i < 257; i++) socket.receive({ t: 'fingerprint', v: 0, fp: 'same' })
    await server.idle()
    expect(socket.closed).toBe(true)
  })

  it('closes a socket whose waiting frames hold more than maxQueuedBytes', async () => {
    const { server, open } = await setup({ maxQueuedBytes: 1000 })
    const submit = (seq: number) => ({ t: 'submit', seq, base: seq - 1, ops: ['x'.repeat(250)] })
    const socket = await open('a')
    for (let seq = 1; seq <= 3; seq++) socket.receive(submit(seq))
    await server.idle()
    expect(socket.closed).toBe(false)
    expect(socket.frames().filter((frame) => frame.t === 'ack')).toHaveLength(3)

    for (let seq = 4; seq <= 7; seq++) socket.receive(submit(seq))
    await server.idle()
    expect(socket.closed).toBe(true)
  })

  it('does not count the base upload, which only maxBaseBytes bounds', async () => {
    const store = memoryStore()
    const server = createServerCore({
      store,
      limits: { maxQueuedBytes: 1000 },
      clock: new VirtualClock(),
    })
    const base = new Uint8Array(4000).fill(7)
    const hello = { t: 'hello', p: 1, format: FORMAT, fileHash: await sha256(base) }
    const seeder = new FakeSocket()
    server.accept(seeder, principal('a'))
    seeder.receive(hello)
    seeder.receive(base)
    await server.idle()
    expect(seeder.closed).toBe(false)
    expect(seeder.frames()[0]).toEqual({ t: 'need_base' })
    expect(seeder.welcomed()).toBeTruthy()

    // A second binary frame is no base upload and counts like any other frame
    const other = new FakeSocket()
    server.accept(other, principal('b'))
    other.receive(hello)
    other.receive(base)
    other.receive(base)
    await server.idle()
    expect(other.closed).toBe(true)
    expect(other.frames()).toEqual([])
  })
})
