import { describe, expect, it } from 'vitest'
import { replayHistory, type CollabAdapter, type HistoryRequest } from '../src/client/index'
import { toyMap, type MapOp } from '../src/conformance/toys'
import { sha256 } from '../src/sha256'

const base = toyMap.file()
const op = (key: string, value: number) => ({ id: `${key}${value}`, key, value })
const first = { v: 1, client: 'a', seq: 1, author: 'alice', ops: [op('title', 1), op('title', 2)] }
const second = { v: 2, client: 'b', seq: 1, author: 'bob', ops: [op('body', 3)] }
const target = { epoch: 'old', v: 1, index: 1 }
const prefix = async (): Promise<HistoryRequest> => ({
  epoch: target.epoch,
  baseHash: await sha256(base),
  format: toyMap.format,
  version: 2,
  changes: [first, second],
})
const text = (bytes: Uint8Array) => new TextDecoder().decode(bytes)

function editor(): CollabAdapter<MapOp> {
  const opened = toyMap.open(base)
  const state = new Map<string, number | null>()
  return {
    ...opened,
    integrate(remote, local) {
      for (const edit of remote) state.set(edit.key, edit.value)
      return opened.integrate(remote, local)
    },
    captureUndo(edit) {
      const previous = state.get(edit.key) ?? null
      return (later) =>
        later.some((other) => other.key === edit.key)
          ? null
          : [{ id: 'undo', key: edit.key, value: previous }]
    },
  }
}

describe('retained history', () => {
  it('renders version zero and an exact prefix without consulting the live room', async () => {
    const request = await prefix()
    const baseFile = await replayHistory(editor(), request.format, base, {
      ...request,
      version: 0,
      changes: [],
    })
    expect(text(baseFile.bytes)).toBe('[]')
    const file = await replayHistory(editor(), request.format, base, {
      ...request,
      version: 1,
      changes: [first],
    })
    expect(text(file.bytes)).toBe('[["title",2]]')
    expect(file.compensation).toBeUndefined()
  })

  it('undoes one op within a batch against the head and keeps a later independent edit', async () => {
    const request = await prefix()
    const file = await replayHistory(editor(), request.format, base, { ...request, undo: target })
    expect(text(file.bytes)).toBe('[["body",3],["title",1]]')
    const appended = { ...first, v: 3, client: 'server', ops: file.compensation! }
    const peer = await replayHistory(editor(), request.format, base, {
      ...request,
      changes: [...request.changes, appended],
      version: 3,
    })
    expect(peer.bytes).toEqual(file.bytes)
  })

  it('refuses a later write to the same field, including one in the same change', async () => {
    const request = await prefix()
    await expect(
      replayHistory(editor(), request.format, base, { ...request, undo: { ...target, index: 0 } }),
    ).rejects.toMatchObject({ code: 'conflict' })
  })

  it('refuses missing changes, unknown ops, wrong bases and identities from another epoch', async () => {
    const request = await prefix()
    for (const update of [
      { version: 3 },
      { changes: [first, first] },
      { changes: [second, first] },
      { changes: [first, { ...second, client: 'a' }] },
      { changes: [first, { ...second, ops: [null] }] },
      { baseHash: '0'.repeat(64) },
      { undo: { ...target, epoch: 'new' } },
      { undo: { ...target, index: 4 } },
    ]) {
      await expect(
        replayHistory(editor(), request.format, base, { ...request, ...update }),
      ).rejects.toMatchObject({ code: 'invalid_history' })
    }
    await expect(
      replayHistory(toyMap.open(base), request.format, base, { ...request, undo: target }),
    ).rejects.toMatchObject({ code: 'unsupported' })
    await expect(
      replayHistory(editor(), { ...request.format, schema: 4 }, base, request),
    ).rejects.toMatchObject({ code: 'unsupported' })
  })
})
