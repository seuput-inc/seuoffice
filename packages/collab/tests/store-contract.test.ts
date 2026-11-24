import { describe, expect, it } from 'vitest'
import { storeContract, type StoreContractCase } from '../src/conformance/index'
import type { HwpCheckpoint } from '../src/server/history'
import type { Change } from '../src/model'
import {
  memoryHistoryStore,
  memoryStore,
  type CollabStore,
  type NewEpoch,
} from '../src/server/store'

describe('memoryStore() keeps the store contract', () => {
  for (const each of storeContract(() => memoryStore())) {
    it.skipIf(each.skip !== undefined)(each.name, each.run)
  }
})

describe('memoryHistoryStore() keeps the store contract with HWP checkpoints', () => {
  for (const each of storeContract(() => memoryHistoryStore(() => true), { checkpoints: true })) {
    it.skipIf(each.skip !== undefined)(each.name, each.run)
  }
})

interface Faults {
  /** appends at any base */
  noBaseCheck?: boolean
  /** appends a (client, seq) it already holds */
  noDuplicateCheck?: boolean
  /** checks, awaits, then writes: two appends can both pass the check */
  awaitInAppend?: boolean
  /** the same race in create */
  awaitInCreate?: boolean
}

interface Epoch extends NewEpoch {
  log: Change[]
  saved: number
}

/**
 * A store over a map that several instances can share, as two server processes share a
 * database; correct unless a fault is switched on.
 */
function referenceStore(faults: Faults = {}, backing = new Map<string, Epoch>()): CollabStore {
  const current = (documentId: string, epochId: string) => {
    const epoch = backing.get(documentId)
    return epoch?.id === epochId ? epoch : undefined
  }
  const existing = (documentId: string, epochId: string) => {
    const epoch = current(documentId, epochId)
    if (!epoch) throw new Error('gone')
    return epoch
  }
  return {
    async load(documentId) {
      const epoch = backing.get(documentId)
      if (!epoch) return null
      const { id, format, baseHash, saved } = epoch
      return { id, format, baseHash, head: epoch.log.length, saved }
    },
    async create(documentId, epoch) {
      if (backing.has(documentId)) return 'exists'
      if (faults.awaitInCreate) await Promise.resolve()
      backing.set(documentId, { ...epoch, base: new Uint8Array(epoch.base), log: [], saved: 0 })
      return 'created'
    },
    async readBase(documentId, epochId) {
      return existing(documentId, epochId).base
    },
    async append(documentId, epochId, change) {
      const epoch = current(documentId, epochId)
      if (!epoch) return { kind: 'gone' }
      const earlier = epoch.log.find((c) => c.client === change.client && c.seq === change.seq)
      if (earlier && !faults.noDuplicateCheck) return { kind: 'duplicate', v: earlier.v }
      const head = epoch.log.length
      if (change.base !== head && !faults.noBaseCheck) return { kind: 'stale', head }
      if (faults.awaitInAppend) await Promise.resolve()
      const v = epoch.log.length + 1
      const { client, seq, author, ops } = structuredClone(change)
      epoch.log.push({ v, client, seq, author, ops })
      return { kind: 'appended', v }
    },
    async read(documentId, epochId, after, limit) {
      return structuredClone(existing(documentId, epochId).log.slice(after, after + limit))
    },
    async markSaved(documentId, epochId, v) {
      const epoch = existing(documentId, epochId)
      epoch.saved = Math.max(epoch.saved, v)
      return epoch.saved
    },
    async drop(documentId, epochId) {
      if (current(documentId, epochId)) backing.delete(documentId)
    },
  }
}

const shared = new WeakMap<CollabStore, Map<string, Epoch>>()

function factory(faults: Faults) {
  return {
    make() {
      const backing = new Map<string, Epoch>()
      const store = referenceStore(faults, backing)
      shared.set(store, backing)
      return store
    },
    reopen(store: CollabStore) {
      return referenceStore(faults, shared.get(store))
    },
  }
}

async function failing(cases: StoreContractCase[]): Promise<Map<string, string>> {
  const failed = new Map<string, string>()
  for (const each of cases) {
    if (each.skip) continue
    try {
      await each.run()
    } catch (error) {
      failed.set(each.name, error instanceof Error ? error.message : String(error))
    }
  }
  return failed
}

const checkpointCases = [
  'an HWP checkpoint is first-wins and immutable, found at or below a version, and outlives drop',
  'concurrent HWP checkpoint writes for one version keep exactly one of them',
]

describe('the store contract kit', () => {
  it('passes a correct store, the second-instance case included', async () => {
    const { make, reopen } = factory({})
    const cases = storeContract(make, { reopen })
    expect(cases.filter((each) => each.skip).map((each) => each.name)).toEqual(checkpointCases)
    expect([...(await failing(cases)).keys()]).toEqual([])
  })

  it('skips the second-instance case for a store without reopen, and says why', () => {
    const skipped = storeContract(() => memoryStore()).filter((each) => each.skip)
    expect(skipped.map((each) => each.name)).toEqual([
      ...checkpointCases,
      'a second instance over the same data sees the same epoch and shares its guarantees',
    ])
  })

  it('fails a store whose HWP checkpoint write replaces an earlier one', async () => {
    const make = () => {
      const store = memoryHistoryStore(() => true)
      const kept = new Map<string, HwpCheckpoint>()
      store.writeHwpCheckpoint = async (documentId, epochId, checkpoint) => {
        kept.set(JSON.stringify([documentId, epochId, checkpoint.version]), checkpoint)
      }
      store.readHwpCheckpoint = async (documentId, epochId, atMost) => {
        for (let v = atMost; v > 0; v--) {
          const found = kept.get(JSON.stringify([documentId, epochId, v]))
          if (found) return found
        }
        return null
      }
      return store
    }
    expect([...(await failing(storeContract(make, { checkpoints: true }))).keys()]).toEqual([
      checkpointCases[0],
    ])
  })

  it.each([
    [
      { noBaseCheck: true },
      'append appends only at the head, and answers stale with the head otherwise',
    ],
    [{ noBaseCheck: true }, '16 concurrent appends at one base: one appended, the rest stale'],
    [
      { noDuplicateCheck: true },
      'a repeated (client, seq) is a duplicate with its first version, whatever its base',
    ],
    [
      { noDuplicateCheck: true },
      '16 concurrent copies of one submit: one appended, the rest duplicate',
    ],
    [{ awaitInAppend: true }, '16 concurrent appends at one base: one appended, the rest stale'],
    [
      { awaitInAppend: true },
      '16 concurrent copies of one submit: one appended, the rest duplicate',
    ],
    [{ awaitInCreate: true }, 'create is first-wins under 16 concurrent creates'],
  ] satisfies [Faults, string][])('fails a store with %o on "%s"', async (faults, name) => {
    const { make, reopen } = factory(faults)
    const failed = await failing(storeContract(make, { reopen }))
    expect([...failed.keys()]).toContain(name)
  })

  it('fails a racy append only on the cases that race, so the failure names the cause', async () => {
    // A check-then-await-then-write append still answers every sequential case correctly
    const { make, reopen } = factory({ awaitInAppend: true })
    const failed = await failing(storeContract(make, { reopen }))
    expect([...failed.keys()].every((name) => /concurrent|second instance/.test(name))).toBe(true)
  })
})
