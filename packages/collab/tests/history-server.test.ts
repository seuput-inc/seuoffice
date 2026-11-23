import { setImmediate as nextTurn } from 'node:timers/promises'
import { describe, expect, it } from 'vitest'
import { deferred } from '../src/deferred'
import type { HistoryRequest, HistoryResult } from '../src/history'
import {
  createHistoryServer,
  memoryHistoryStore,
  type Principal,
  type ServerSocket,
} from '../src/server/index'
import { sha256 } from '../src/sha256'
import { parseServerFrame, type BytesFrame, type ServerFrame } from '../src/wire'

const p: Principal = { documentId: 'doc', name: 'Alice', userId: 'alice', canEdit: true }
const bytes = Uint8Array.of(10)
const format = { name: 'counter', schema: 1 }
const expected = { epoch: 'A', version: 1 }
const restore = { expected, source: expected, requestId: 'restore-1' }
const undo = { expected, target: { epoch: 'A', v: 1, index: 0 }, requestId: 'undo-1' }

async function render(base: Uint8Array, request: HistoryRequest): Promise<HistoryResult> {
  let value = base[0]!
  for (const change of request.changes) for (const delta of change.ops) value += Number(delta)
  const file = { fileName: 'revision.counter', bytes: Uint8Array.of(value) }
  if (!request.undo) return file
  const { v, index } = request.undo
  const delta = -Number(request.changes[v - 1]!.ops[index])
  return { ...file, bytes: Uint8Array.of(value + delta), compensation: [delta] }
}

async function setup(renderer = render, idleEpochMs = Infinity) {
  const grants = new Map([
    ['alice', 'edit'],
    ['bob', 'edit'],
    ['reader', 'view'],
  ])
  const store = memoryHistoryStore(
    (user, action) =>
      user.documentId === 'doc' &&
      (action === 'export' ? grants.has(user.userId) : grants.get(user.userId) === 'edit'),
  )
  const server = createHistoryServer({ store, render: renderer, limits: { idleEpochMs } })
  const baseHash = await sha256(bytes)
  const seed = (id: string) => store.create('doc', { id, base: bytes, format, baseHash })
  await seed('A')
  await store.append('doc', 'A', { base: 0, client: 'writer', seq: 1, author: 'alice', ops: [5] })

  async function connect(userId: string) {
    let receive!: (frame: string | Uint8Array) => void
    let hangUp!: () => void
    const welcome = deferred<void>()
    const ack = deferred<void>()
    const frames: (ServerFrame | BytesFrame)[] = []
    const socket: ServerSocket = {
      onMessage(handler) {
        receive = handler
      },
      onClose(handler) {
        hangUp = handler
      },
      close() {},
      send(frame) {
        const msg = parseServerFrame(frame)
        frames.push(msg)
        if (msg.t === 'welcome') welcome.resolve()
        if (msg.t === 'ack') ack.resolve()
      },
    }
    server.accept(socket, { ...p, userId })
    receive(JSON.stringify({ t: 'hello', p: 1, format, fileHash: baseHash }))
    await welcome.promise
    return {
      frames,
      hangUp,
      submit(base: number, delta: number) {
        receive(JSON.stringify({ t: 'submit', seq: 1, base, ops: [delta] }))
        return ack.promise
      },
    }
  }
  return { server, store, grants, seed, connect }
}

describe('retained history through the host API', () => {
  it('forks an immutable historical prefix after the source epoch ended, without changing the active document', async () => {
    const { server, store, seed } = await setup()
    await store.drop('doc', 'A')
    await seed('B')
    const input = { ...restore, expected: { epoch: 'B', version: 0 } }
    const [first, retry, fork] = await Promise.all([
      server.restoreHistory(p, input),
      server.restoreHistory(p, input),
      server.restoreHistory(p, { ...input, requestId: 'second-fork' }),
    ])
    expect(retry).toEqual(first)
    expect(fork.revisionId).not.toBe(first.revisionId)
    expect(await store.load('doc')).toMatchObject({ id: 'B', head: 0, saved: 0 })
    await expect(store.readBase('doc', 'A')).rejects.toThrow()
    const saved = await store.readRevision(p, first.revisionId)
    expect(saved.bytes).toEqual(Uint8Array.of(15))
    saved.bytes.fill(0)
    expect((await store.readRevision(p, first.revisionId)).bytes).toEqual(Uint8Array.of(15))
    expect((await server.exportHistory(p, { epoch: 'A', version: 0 })).bytes).toEqual(bytes)
    await expect(server.exportHistory(p, { epoch: 'A', version: 2 })).rejects.toThrow()
    await expect(
      server.restoreHistory(p, { ...input, source: { epoch: 'A', version: 0 } }),
    ).rejects.toThrow(/conflicting/)
    await expect(server.restoreHistory(p, restore)).rejects.toThrow()
    await server.close()
  })

  it('delivers a compensation once, including to a live peer and a late joiner, without altering the saved version', async () => {
    const { server, store, connect } = await setup()
    const bob = await connect('bob')
    await bob.submit(1, 7)
    const input = { ...undo, expected: { epoch: 'A', version: 2 } }
    const [one, two] = await Promise.all([
      server.undoHistory(p, input),
      server.undoHistory(p, input),
    ])
    expect(one).toEqual({ kind: 'undone', version: 3 })
    expect(two).toEqual(one)
    expect(await server.undoHistory(p, input)).toEqual(one)
    const late = await connect('alice')
    // A submit ack follows catch-up, so all previous changes have reached the late joiner.
    await late.submit(3, 1)
    for (const peer of [bob, late]) {
      expect(peer.frames.filter((frame) => frame.t === 'change' && frame.v === 3)).toEqual([
        expect.objectContaining({ author: 'alice', ops: [-5] }),
      ])
    }
    expect(await server.undoHistory(p, input)).toEqual(one)
    expect(await store.load('doc')).toMatchObject({ id: 'A', head: 4, saved: 0 })
    expect((await server.exportHistory(p, { epoch: 'A', version: 4 })).bytes).toEqual(
      Uint8Array.of(18),
    )
    await expect(
      server.undoHistory(p, { ...input, expected: { epoch: 'A', version: 4 } }),
    ).rejects.toThrow(/conflicting/)
    await expect(
      server.undoHistory(p, {
        ...input,
        requestId: 'second-undo',
        expected: { epoch: 'A', version: 4 },
      }),
    ).rejects.toThrow(/already undone/)
    await server.close()
  })

  it.each(['restore', 'undo'] as const)(
    'rejects a %s rendered before a competing socket submit',
    async (action) => {
      const started = deferred<void>()
      const resume = deferred<void>()
      const { server, store, connect } = await setup(async (base, req) => {
        started.resolve()
        await resume.promise
        return render(base, req)
      })
      const bob = await connect('bob')
      const saving =
        action === 'restore' ? server.restoreHistory(p, restore) : server.undoHistory(p, undo)
      await started.promise
      await bob.submit(1, 7)
      resume.resolve()
      await expect(saving).rejects.toThrow(/stale/)
      expect((await store.read('doc', 'A', 0, 10)).map((c) => c.ops)).toEqual([[5], [7]])
      await server.close()
    },
  )

  it('cannot slip an unsaved compensation between the idle saved-head check and drop', async () => {
    const rendered = deferred<void>()
    const { server, store, connect } = await setup(async (base, req) => {
      const file = await render(base, req)
      rendered.resolve()
      return file
    }, 0)
    const peer = await connect('bob')
    await store.markSaved('doc', 'A', 1)
    const checked = deferred<void>()
    const drop = deferred<void>()
    const load = store.load
    store.load = async (doc) => {
      const snapshot = await load(doc)
      checked.resolve()
      await drop.promise
      return snapshot
    }
    peer.hangUp()
    await checked.promise
    const saving = server.undoHistory(p, undo)
    await rendered.promise
    await nextTurn()
    drop.resolve()
    await expect(saving).rejects.toThrow(/stale/)
    expect(await load('doc')).toBeNull()
    await expect(server.exportHistory(p, { epoch: 'A', version: 2 })).rejects.toThrow()
    await server.close()
  })

  it('waits for a history write that has entered the transaction before closing', async () => {
    const { server, store } = await setup()
    const commit = store.commitHistory
    const entered = deferred<void>()
    const resume = deferred<void>()
    store.commitHistory = async (...args) => {
      entered.resolve()
      await resume.promise
      return commit(...args)
    }
    const saving = server.undoHistory(p, undo)
    await entered.promise
    const closing = server.close().then(() => store.load('doc'))
    await nextTurn()
    resume.resolve()
    expect(await closing).toMatchObject({ head: 2 })
    expect(await saving).toEqual({ kind: 'undone', version: 2 })
  })

  it('checks the ACL again at commit and rejects view-only writes and another author’s target', async () => {
    const started = deferred<void>()
    const resume = deferred<void>()
    const { server, grants, store } = await setup(async (base, req) => {
      started.resolve()
      await resume.promise
      return render(base, req)
    })
    await expect(server.undoHistory({ ...p, userId: 'bob' }, undo)).rejects.toThrow(/target/)
    await expect(server.restoreHistory({ ...p, userId: 'reader' }, restore)).rejects.toThrow(
      /forbidden/,
    )
    await expect(server.restoreHistory({ ...p, canEdit: false }, restore)).rejects.toThrow(
      /forbidden/,
    )
    await expect(server.exportHistory({ ...p, documentId: 'different' }, expected)).rejects.toThrow(
      /forbidden/,
    )
    const saving = server.undoHistory(p, undo)
    await started.promise
    grants.delete('alice')
    resume.resolve()
    await expect(saving).rejects.toThrow(/forbidden/)
    expect(await store.load('doc')).toMatchObject({ head: 1, saved: 0 })
    await server.close()
  })

  it('rejects a renderer result without compensation before writing', async () => {
    const { server, store } = await setup(async () => ({ fileName: 'bad.counter', bytes }))
    await expect(server.undoHistory(p, undo)).rejects.toThrow()
    expect(await store.load('doc')).toMatchObject({ head: 1, saved: 0 })
    await server.close()
  })
})
