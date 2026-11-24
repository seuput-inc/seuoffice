import { describe, expect, it, vi } from 'vitest'
import { checkPrefix, type HistoryRequest, type HistoryResult } from '../src/history'
import { sha256 } from '../src/sha256'
import { createHistoryServer } from '../src/server/history'
import type { Principal, ServerSocket } from '../src/server/server'
import { memoryHistoryStore } from '../src/server/store'

const format = { name: 'hwp', schema: 1 }
const base = new TextEncoder().encode('base:')
const p: Principal = { documentId: 'doc', userId: 'alice', name: 'Alice', canEdit: true }
const text = (bytes: Uint8Array) => new TextDecoder().decode(bytes)

/** A stand-in renderer: the file is text, every op appends its `t`, an undo appends '~'. */
function renderer(reopen: (request: HistoryRequest) => boolean = () => true) {
  const requests: HistoryRequest[] = []
  const render = vi.fn(async (bytes: Uint8Array, request: HistoryRequest) => {
    checkPrefix(request, format)
    if ((await sha256(bytes)) !== request.baseHash) throw new Error('base hash')
    requests.push(structuredClone(request))
    let out = text(bytes)
    for (const change of request.changes)
      for (const op of change.ops)
        if (op && typeof op === 'object' && !Array.isArray(op)) out += String(op.t)
    if ((request.start ?? 0) > 0 && !request.changes.length && !reopen(request)) out += '!'
    const result: HistoryResult = { fileName: 'shared.hwp', bytes: new TextEncoder().encode(out) }
    return request.undo
      ? { ...result, bytes: new TextEncoder().encode(out + '~'), compensation: [{ t: '~' }] }
      : result
  })
  return { render, requests }
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
      { interval: 1, timeout: 3000 },
    )
    return this.frames.filter((f) => f.t === t)[count - 1]!
  }
}

async function setup(
  options: { reopen?: (r: HistoryRequest) => boolean; failWrite?: boolean } = {},
) {
  const store = memoryHistoryStore(() => true)
  const write = vi.fn(store.writeHwpCheckpoint!)
  store.writeHwpCheckpoint = options.failWrite
    ? vi.fn(async () => {
        throw new Error('disk full')
      })
    : write
  const { render, requests } = renderer(options.reopen)
  const server = createHistoryServer({ store, render })
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
  return { store, write, render, requests, server, open, alice, epoch, type, next: () => ++seq }
}

describe('HWP retained history checkpoints', () => {
  it('keeps one editor typing 300 changes in a single session and replays every version', async () => {
    const { store, requests, server, alice, epoch, type } = await setup()
    await type(300)
    expect(alice.closed).toBeUndefined()
    expect((await store.load(p.documentId))!.head).toBe(300)
    expect(Math.max(...requests.map((r) => r.changes.length))).toBeLessThanOrEqual(128)
    for (const version of [5, 128, 200, 300]) {
      const file = await server.exportHistory(p, { epoch, version })
      expect(text(file.bytes)).toBe('base:' + 'x'.repeat(version))
    }
    expect((await store.readHwpCheckpoint!(p.documentId, epoch, 300))?.version).toBeLessThan(300)
  })

  it('writes the checkpoint once when two editors submit at the limit together', async () => {
    const { store, write, requests, open, alice, epoch, type, next } = await setup()
    await type(128)
    const bob = await open('bob')
    const seq = next()
    alice.receive({ t: 'submit', seq, base: 128, ops: [{ t: 'a' }] })
    bob.receive({ t: 'submit', seq: 1, base: 128, ops: [{ t: 'b' }] })
    await alice.next('ack', seq)
    expect((await bob.next('stale')).head).toBe(129)
    bob.receive({ t: 'submit', seq: 1, base: 129, ops: [{ t: 'b' }] })
    await bob.next('ack')
    expect(write).toHaveBeenCalledOnce()
    expect(write.mock.calls[0]![2].version).toBe(128)
    expect(requests.at(-1)).toMatchObject({ start: 128, version: 130 })
    expect(requests.at(-1)!.changes.map((c) => c.v)).toEqual([129, 130])
    const checkpoint = await store.readHwpCheckpoint!(p.documentId, epoch, 130)
    expect(text(checkpoint!.base)).toBe('base:' + 'x'.repeat(128))
  })

  it.each([
    ['the renderer cannot reopen its own file', { reopen: () => false }],
    ['the store cannot keep the checkpoint', { failWrite: true }],
  ])('ends the session with a reason when %s', async (_case, options) => {
    const { store, alice, epoch, type, next } = await setup(options)
    await type(128)
    alice.receive({ t: 'submit', seq: next(), base: 128, ops: [{ t: 'y' }] })
    expect(await alice.next('error')).toEqual({ t: 'error', code: 'schema_mismatch' })
    expect(alice.closed).toMatch(/HWP/)
    expect((await store.load(p.documentId))!.head).toBe(128)
    expect(await store.readHwpCheckpoint!(p.documentId, epoch, 128)).toBeNull()
  })

  it('refuses to undo an edit older than the checkpoint and undoes a newer one', async () => {
    const { server, epoch, type } = await setup()
    await type(130)
    const expected = { epoch, version: 130 }
    await expect(
      server.undoHistory(p, { requestId: 'old', expected, target: { epoch, v: 3, index: 0 } }),
    ).rejects.toThrow(/older than the saved history point/)
    await expect(
      server.undoHistory(p, { requestId: 'new', expected, target: { epoch, v: 130, index: 0 } }),
    ).resolves.toEqual({ kind: 'undone', version: 131 })
  })
})
