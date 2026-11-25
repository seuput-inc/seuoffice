import { DatabaseSync } from 'node:sqlite'
import { describe, expect, it, vi } from 'vitest'
import { checkPrefix, HistoryError, type HistoryRequest, type HistoryResult } from '../src/history'
import type { Json } from '../src/model'
import { sha256 } from '../src/sha256'
import {
  createHistoryServer,
  type HistoryServer,
  type RetainedHistoryStore,
} from '../src/server/history'
import { createCollabServer, type Principal, type ServerSocket } from '../src/server/server'
import { memoryHistoryStore } from '../src/server/store'
import type {
  HistoryByteSource,
  HistoryStage,
  StreamHistoryStore,
} from '../src/server/stream-history'
// The example store is plain JavaScript without declarations, so it stays out of the typecheck.
const { createStreamStore } = await import(
  new URL('../../history-renderer/examples/stream-store.mjs', import.meta.url).href
)

const format = { name: 'hwp', schema: 1 }
const base = new TextEncoder().encode('base:')
const p: Principal = { documentId: 'doc', userId: 'alice', name: 'Alice', canEdit: true }
const text = (bytes: Uint8Array) => new TextDecoder().decode(bytes)
const encode = (value: string) => new TextEncoder().encode(value)

interface Rendered {
  base: string
  request: HistoryRequest
}

/**
 * A stand-in renderer with the real one's request limits: the file is text, every op appends
 * its `t`, an undo appends '~'. `reopen` false makes it change a checkpoint it is asked to reopen.
 */
function renderer(reopen: boolean) {
  const calls: Rendered[] = []
  async function render(bytes: Uint8Array, request: HistoryRequest): Promise<HistoryResult> {
    checkPrefix(request, format)
    if ((await sha256(bytes)) !== request.baseHash) throw new Error('base hash')
    if (text(bytes).startsWith('opaque:'))
      throw new HistoryError('unsupported', 'a record the renderer cannot replay')
    if (request.changes.length > 128) throw new Error('renderer limit')
    if (request.undo && request.changes.length + 1 > 128)
      throw new HistoryError('unsupported', 'compensated prefix exceeds replay limits')
    calls.push({ base: text(bytes), request: structuredClone(request) })
    let out = text(bytes)
    for (const change of request.changes)
      for (const op of change.ops)
        if (op && typeof op === 'object' && !Array.isArray(op)) out += String(op.t)
    if (!reopen && !request.changes.length && out !== 'base:') out += '!'
    return request.undo
      ? { fileName: 'shared.hwp', bytes: encode(out + '~'), compensation: [{ t: '~' }] }
      : { fileName: 'shared.hwp', bytes: encode(out) }
  }
  async function renderStream(
    source: HistoryByteSource,
    header: Uint8Array,
    changes: AsyncIterable<Uint8Array>,
    stage: HistoryStage,
  ): Promise<{ fileName: string; compensation?: Json[] }> {
    let bytes = new Uint8Array()
    for await (const part of source.open()) bytes = new Uint8Array([...bytes, ...part])
    const request = { ...JSON.parse(text(header)), changes: [] as HistoryRequest['changes'] }
    for await (const frame of changes) request.changes.push(JSON.parse(text(frame)))
    const file = await render(bytes, request)
    await stage.write(file.bytes)
    return file.compensation ? { fileName: file.fileName, compensation: file.compensation } : file
  }
  return { calls, render, renderStream }
}

class Socket implements ServerSocket {
  readonly frames: Record<string, unknown>[] = []
  closed: string | undefined
  #message: (frame: string | Uint8Array) => void = () => {}
  send(frame: string | Uint8Array) {
    if (typeof frame === 'string') this.frames.push(JSON.parse(frame))
  }
  close(_code?: number, reason?: string) {
    this.closed = reason ?? 'closed'
  }
  onMessage(handler: (frame: string | Uint8Array) => void) {
    this.#message = handler
  }
  onClose() {}
  receive(frame: object | Uint8Array) {
    this.#message(frame instanceof Uint8Array ? frame : JSON.stringify(frame))
  }
  async next(t: string, count = 1) {
    await vi.waitFor(
      () => {
        if (this.frames.filter((f) => f.t === t).length < count) {
          throw new Error(`waiting for ${t}: ${JSON.stringify(this.frames.at(-1))}`)
        }
      },
      { interval: 1, timeout: 5000 },
    )
    return this.frames.filter((f) => f.t === t)[count - 1]!
  }
}

type Kind = 'bytes' | 'stream'

async function setup(
  kind: Kind,
  options: { reopen?: boolean; failWrite?: boolean; failCommit?: boolean } = {},
) {
  const { calls, render, renderStream } = renderer(options.reopen ?? true)
  const db = kind === 'stream' ? new DatabaseSync(':memory:') : null
  const stream: { store: StreamHistoryStore } | null = db
    ? createStreamStore(
        db,
        () => true,
        () => true,
      )
    : null
  const memory = stream ? null : memoryHistoryStore(() => true)
  const store: RetainedHistoryStore | StreamHistoryStore = stream?.store ?? memory!
  const write = vi.fn(store.writeHwpCheckpoint!.bind(store))
  store.writeHwpCheckpoint = options.failWrite
    ? vi.fn(async () => {
        throw new Error('disk full')
      })
    : write
  const refuse = vi.fn(async () => {
    throw new Error('stale history head')
  })
  if (options.failCommit && stream) stream.store.commitHistoryStream = refuse
  if (options.failCommit && memory) memory.commitHistory = refuse
  const server = stream
    ? createHistoryServer({ store: stream.store, renderStream })
    : createHistoryServer({ store: memory!, render })
  const fileHash = await sha256(base)
  async function open(userId = 'alice') {
    const socket = new Socket()
    server.accept(socket, { ...p, userId, name: userId })
    socket.receive({ t: 'hello', p: 1, format, fileHash })
    // The first editor seeds the epoch; later ones join it.
    if (userId === 'alice') {
      await socket.next('need_base')
      socket.receive(base)
    }
    await socket.next('welcome')
    return socket
  }
  const alice = await open()
  const epoch = String((await alice.next('welcome')).epoch)
  let seq = 0
  async function type(count: number) {
    for (let i = 0; i < count; i++) {
      const head = (await store.load(p.documentId))!.head
      alice.receive({ t: 'submit', seq: ++seq, base: head, ops: [{ t: 'x' }] })
      await alice.next('ack', seq)
    }
  }
  async function exported(version: number) {
    const file = await server.exportHistory(p, { epoch, version })
    if ('bytes' in file) return text(file.bytes)
    let bytes = new Uint8Array()
    for await (const part of file.open()) bytes = new Uint8Array([...bytes, ...part])
    await file.dispose()
    return text(bytes)
  }
  return {
    store,
    write,
    calls,
    server,
    open,
    alice,
    epoch,
    type,
    exported,
    next: () => ++seq,
    async close() {
      await server.close()
      db?.close()
    },
  }
}

describe.each<Kind>(['bytes', 'stream'])(
  'HWP retained history checkpoints in a %s store',
  (kind) => {
    it('keeps one editor typing 1,000 single keys in one session and replays every version', async () => {
      const t = await setup(kind)
      try {
        await t.type(1000)
        expect(t.alice.closed).toBeUndefined()
        expect((await t.store.load(p.documentId))!.head).toBe(1000)
        expect(Math.max(...t.calls.map((c) => c.request.changes.length))).toBeLessThanOrEqual(128)
        for (const version of [5, 128, 500, 1000])
          expect(await t.exported(version)).toBe('base:' + 'x'.repeat(version))
        expect(
          (await t.store.readHwpCheckpoint!(p.documentId, t.epoch, 1000))?.version,
        ).toBeLessThan(1000)
        const { revisionId } = await t.server.restoreHistory(p, {
          requestId: 'restore-200',
          source: { epoch: t.epoch, version: 200 },
          expected: { epoch: t.epoch, version: 1000 },
        })
        expect(revisionId).toBeTruthy()
      } finally {
        await t.close()
      }
    }, 60_000)

    it('writes the checkpoint once when two editors submit at the limit together', async () => {
      const t = await setup(kind)
      try {
        await t.type(128)
        const bob = await t.open('bob')
        const seq = t.next()
        t.alice.receive({ t: 'submit', seq, base: 128, ops: [{ t: 'a' }] })
        bob.receive({ t: 'submit', seq: 1, base: 128, ops: [{ t: 'b' }] })
        await t.alice.next('ack', seq)
        expect((await bob.next('stale')).head).toBe(129)
        bob.receive({ t: 'submit', seq: 1, base: 129, ops: [{ t: 'b' }] })
        await bob.next('ack')
        expect(t.write).toHaveBeenCalledOnce()
        expect(t.write.mock.calls[0]![2].version).toBe(128)
        // Bob's admission replays from the checkpoint, renumbered from 1.
        expect(t.calls.at(-1)!.base).toBe('base:' + 'x'.repeat(128))
        expect(t.calls.at(-1)!.request.changes.map((c) => c.ops)).toEqual([
          [{ t: 'a' }],
          [{ t: 'b' }],
        ])
      } finally {
        await t.close()
      }
    })

    it('checkpoints before the renderer text budget, not only the change count', async () => {
      const t = await setup(kind)
      try {
        const picture = (n: number) => [{ t: String(n), data: 'p'.repeat(150 * 1024) }]
        t.alice.receive({ t: 'submit', seq: t.next(), base: 0, ops: picture(1) })
        await t.alice.next('ack', 1)
        t.alice.receive({ t: 'submit', seq: t.next(), base: 1, ops: picture(2) })
        await t.alice.next('ack', 2)
        expect(t.alice.closed).toBeUndefined()
        expect((await t.store.readHwpCheckpoint!(p.documentId, t.epoch, 2))?.version).toBe(1)
        expect(await t.exported(2)).toBe('base:12')
      } finally {
        await t.close()
      }
    })

    it.each([
      ['the renderer cannot reopen its own file', { reopen: false }],
      ['the store cannot keep the checkpoint', { failWrite: true }],
    ])('ends the session with a reason when %s', async (_case, options) => {
      const t = await setup(kind, options)
      try {
        await t.type(128)
        t.alice.receive({ t: 'submit', seq: t.next(), base: 128, ops: [{ t: 'y' }] })
        const refusal = await t.alice.next('error')
        expect(refusal).toMatchObject({ t: 'error', code: 'schema_mismatch' })
        expect(refusal.reason).toMatch(
          /^HWP .*: the (renderer did not reopen|store could not keep)/,
        )
        // The host's own error text stays in the host log.
        expect(refusal.reason).not.toContain('disk full')
        expect(t.alice.closed).toMatch(/HWP/)
        expect((await t.store.load(p.documentId))!.head).toBe(128)
        expect(await t.store.readHwpCheckpoint!(p.documentId, t.epoch, 128)).toBeNull()
      } finally {
        await t.close()
      }
    })

    it('refuses to undo an edit older than the checkpoint and undoes one inside a full window', async () => {
      const t = await setup(kind)
      try {
        await t.type(256)
        const expected = { epoch: t.epoch, version: 256 }
        await expect(
          t.server.undoHistory(p, {
            requestId: 'old',
            expected,
            target: { epoch: t.epoch, v: 3, index: 0 },
          }),
        ).rejects.toThrow(/older than the saved history point/)
        // Versions 129-256 fill the window; the compensation would not fit without a new start.
        await expect(
          t.server.undoHistory(p, {
            requestId: 'full',
            expected,
            target: { epoch: t.epoch, v: 130, index: 0 },
          }),
        ).resolves.toEqual({ kind: 'undone', version: 257 })
        expect((await t.store.readHwpCheckpoint!(p.documentId, t.epoch, 257))?.version).toBe(129)
        expect(await t.exported(257)).toBe('base:' + 'x'.repeat(256) + '~')
        // The next submit replays from that checkpoint instead of overflowing from 128.
        await t.type(1)
        expect(t.alice.closed).toBeUndefined()
        expect(await t.exported(258)).toBe('base:' + 'x'.repeat(256) + '~x')
      } finally {
        await t.close()
      }
    })

    it('leaves the history replayable when an undo commit is refused after its checkpoint', async () => {
      const t = await setup(kind, { failCommit: true })
      try {
        await t.type(256)
        await expect(
          t.server.undoHistory(p, {
            requestId: 'full',
            expected: { epoch: t.epoch, version: 256 },
            target: { epoch: t.epoch, v: 130, index: 0 },
          }),
        ).rejects.toThrow(/stale history head/)
        expect((await t.store.load(p.documentId))!.head).toBe(256)
        // The confirmed version 129 may stay as a checkpoint; it is a correct file either way.
        const bob = await t.open('bob')
        bob.receive({ t: 'submit', seq: 1, base: 256, ops: [{ t: 'b' }] })
        expect(await bob.next('ack')).toMatchObject({ v: 257 })
        expect(await t.exported(257)).toBe('base:' + 'x'.repeat(256) + 'b')
      } finally {
        await t.close()
      }
    })

    it('refuses an undo whose checkpoint cannot be stored and leaves everyone editing', async () => {
      const t = await setup(kind)
      try {
        await t.type(256)
        const store = t.store
        const original = store.writeHwpCheckpoint!.bind(store)
        let failures = 2
        store.writeHwpCheckpoint = vi.fn(async (...args: Parameters<typeof original>) => {
          if (failures-- > 0) throw new Error('disk full')
          return original(...args)
        })
        await expect(
          t.server.undoHistory(p, {
            requestId: 'full',
            expected: { epoch: t.epoch, version: 256 },
            target: { epoch: t.epoch, v: 130, index: 0 },
          }),
        ).rejects.toThrow(/could not be stored; nothing was changed/)
        expect((await store.load(p.documentId))!.head).toBe(256)
        expect((await store.readHwpCheckpoint!(p.documentId, t.epoch, 256))?.version).toBe(128)
        const bob = await t.open('bob')
        bob.receive({ t: 'submit', seq: 1, base: 256, ops: [{ t: 'b' }] })
        expect(await bob.next('ack')).toMatchObject({ v: 257 })
        expect(await t.exported(257)).toBe('base:' + 'x'.repeat(256) + 'b')
        const { revisionId } = await t.server.restoreHistory(p, {
          requestId: 'restore-head',
          source: { epoch: t.epoch, version: 257 },
          expected: { epoch: t.epoch, version: 257 },
        })
        expect(revisionId).toBeTruthy()
      } finally {
        await t.close()
      }
    })
  },
)

describe('HWP epochs this history server did not start', () => {
  type Start = 'the host at upload' | 'a server without history' | 'its first editor'

  /** The epoch of `file` as `by` starts it, with `edits` single keys already in it. */
  async function begun(store: RetainedHistoryStore, file: string, edits: number, by: Start) {
    const bytes = encode(file)
    const fileHash = await sha256(bytes)
    if (by === 'the host at upload') {
      await store.create(p.documentId, { id: 'uploaded', format, baseHash: fileHash, base: bytes })
      for (let seq = 1; seq <= edits; seq++) {
        const change = { client: 'host', seq, author: p.userId, base: seq - 1, ops: [{ t: 'x' }] }
        await store.append(p.documentId, 'uploaded', change)
      }
    } else if (by === 'a server without history') {
      const plain = createCollabServer({ store })
      const socket = new Socket()
      plain.accept(socket, p)
      socket.receive({ t: 'hello', p: 1, format, fileHash })
      await socket.next('need_base')
      socket.receive(bytes)
      await socket.next('welcome')
      for (let seq = 1; seq <= edits; seq++) {
        socket.receive({ t: 'submit', seq, base: seq - 1, ops: [{ t: 'x' }] })
        await socket.next('ack', seq)
      }
      await plain.close()
    }
    return bytes
  }

  /** An editor or viewer saying hello with `file`, which it uploads when asked for the base. */
  async function join(server: HistoryServer, file: Uint8Array, userId: string, canEdit = true) {
    const socket = new Socket()
    server.accept(socket, { ...p, userId, name: userId, canEdit })
    socket.receive({ t: 'hello', p: 1, format, fileHash: await sha256(file) })
    await vi.waitFor(() => expect(answer(socket) ?? socket.frames[0]).toBeDefined())
    if (socket.frames[0]?.t === 'need_base') socket.receive(file)
    await vi.waitFor(() => expect(answer(socket)).toBeDefined())
    return socket
  }
  /** What the server answered the hello with. */
  function answer(socket: Socket) {
    return socket.frames.find((f) => f.t === 'welcome' || f.t === 'error')
  }

  it.each<Start>(['the host at upload', 'a server without history', 'its first editor'])(
    'keeps two editors typing on a byte store without exact history writes when %s started the epoch, and refuses only HWP undo',
    async (by) => {
      const { exactHistoryChange, ...legacy } = memoryHistoryStore(() => true)
      expect(exactHistoryChange).toBe(true)
      const file = await begun(legacy, 'base:', 0, by)
      const { render, calls } = renderer(true)
      const server = createHistoryServer({ store: legacy, render })
      try {
        const alice = await join(server, file, 'alice')
        const bob = await join(server, file, 'bob')
        expect([answer(alice), answer(bob)]).toMatchObject([{ t: 'welcome' }, { t: 'welcome' }])
        // The first editor's join replayed the epoch base once, whoever started it.
        expect(calls.map((c) => c.request.version)).toEqual([0])
        alice.receive({ t: 'submit', seq: 1, base: 0, ops: [{ t: 'a' }] })
        expect(await alice.next('ack')).toMatchObject({ v: 1 })
        bob.receive({ t: 'submit', seq: 1, base: 1, ops: [{ t: 'b' }] })
        expect(await bob.next('ack')).toMatchObject({ v: 2 })
        const epoch = (await legacy.load(p.documentId))!.id
        const exported = await server.exportHistory(p, { epoch, version: 2 })
        expect(text(exported.bytes)).toBe('base:ab')
        await expect(
          server.undoHistory(p, {
            requestId: 'undo-a',
            expected: { epoch, version: 2 },
            target: { epoch, v: 1, index: 0 },
          }),
        ).rejects.toThrow(/exact history writes/)
        expect((await legacy.load(p.documentId))!.head).toBe(2)
        expect([alice.closed, bob.closed]).toEqual([undefined, undefined])
      } finally {
        await server.close()
      }
    },
  )

  it('refuses an editor at join, with a reason, when the renderer cannot replay the epoch', async () => {
    const store = memoryHistoryStore(() => true)
    const file = await begun(store, 'opaque:', 2, 'a server without history')
    const { render } = renderer(true)
    const server = createHistoryServer({ store, render })
    try {
      const editor = await join(server, file, 'alice')
      expect(answer(editor)).toEqual({
        t: 'error',
        code: 'schema_mismatch',
        reason:
          'HWP source or edit cannot be replayed within the retained history profile: the existing session cannot be replayed: the renderer refused it (unsupported)',
      })
      expect(editor.frames.some((f) => f.t === 'welcome')).toBe(false)
      expect(editor.closed).toMatch(/HWP/)
      // A viewer sends no edits for the server to replay.
      const viewer = await join(server, file, 'bob', false)
      expect(answer(viewer)).toMatchObject({ t: 'welcome', head: 2, canEdit: false })
      expect((await store.load(p.documentId))!.head).toBe(2)
    } finally {
      await server.close()
    }
  })
})
