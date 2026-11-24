import { readFileSync } from 'node:fs'
import { expect, it } from 'vitest'
import {
  memoryHistoryStore,
  type Change,
  type Principal,
  type UndoRequest,
} from '../src/server/index'
import { sha256 } from '../src/sha256'

it('stores the exact certified compensation, and leaves no receipt on an author mismatch', async () => {
  const p: Principal = { documentId: 'doc', userId: 'account', name: 'Writer', canEdit: true }
  const base = readFileSync(
    new URL('../../../apps/hwp/src/renderer/assets/blank.hwp', import.meta.url),
  )
  const store = memoryHistoryStore(() => true)
  const id = 'epoch'
  await store.create(p.documentId, {
    id,
    format: { name: 'hwp', schema: 1 },
    base,
    baseHash: await sha256(base),
  })
  const from = { section: 0, paragraph: 0, path: [], offset: 0 }
  const op = { kind: 'splice', from, to: from, text: 'A' }
  await store.append(p.documentId, id, {
    client: 'writer',
    seq: 1,
    author: p.userId,
    base: 0,
    ops: [op],
  })
  const command: UndoRequest & { kind: 'undo' } = {
    kind: 'undo',
    requestId: 'undo',
    expected: { epoch: id, version: 1 },
    target: { epoch: id, v: 1, index: 0 },
  }
  const change: Change = {
    v: 2,
    client: `history:${crypto.randomUUID()}`,
    seq: 1,
    author: p.userId,
    ops: [{ ...op, to: { ...from, offset: 1 }, text: '' }],
  }
  const write = { command, compensation: change.ops, change }
  await expect(store.commitHistory(p, { command, compensation: change.ops })).rejects.toThrow()
  await expect(
    store.commitHistory(p, { ...write, change: { ...change, author: 'other' } }),
  ).rejects.toThrow()
  expect(await store.historyReceipt(p, command)).toBeNull()
  expect(await store.read(p.documentId, id, 1, 1)).toEqual([])
  expect(await store.commitHistory(p, write)).toEqual({ kind: 'undone', version: 2 })
  expect(await store.commitHistory(p, write)).toEqual({ kind: 'undone', version: 2 })
  expect(await store.read(p.documentId, id, 1, 2)).toEqual([change])
})
