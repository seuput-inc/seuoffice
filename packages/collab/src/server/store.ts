import type { HistoryOpId } from '../history.js'
import type { Change, Format, Json } from '../model.js'
import type { Principal } from './server.js'
import type {
  HistoryAction,
  HistoryArtifact,
  HistoryCommand,
  HistoryReceipt,
  RetainedHistoryStore,
  UndoTargetPolicy,
} from './history.js'

/** What the server needs to know about a document's epoch without reading its base or log */
export interface EpochHead {
  id: string
  format: Format
  baseHash: string
  head: number
  saved: number
}

export interface NewEpoch {
  id: string
  format: Format
  baseHash: string
  base: Uint8Array
}

/** A submit to append at `base + 1` */
export interface AppendRequest {
  base: number
  client: string
  seq: number
  author: string
  ops: Json[]
}

export type AppendResult =
  | { kind: 'appended'; v: number }
  | { kind: 'duplicate'; v: number }
  | { kind: 'stale'; head: number }
  | { kind: 'gone' }

/**
 * Where epochs live; a durable implementation is the host's. Every method is
 * atomic per document. `append` is the one compare-and-append that keeps the
 * log's invariants: it answers `duplicate` with the earlier version when the
 * epoch already holds `(client, seq)` (checked before the base), appends only
 * when `base` equals the head, and never reuses a version. `gone` means the
 * epoch no longer exists. `read` returns the changes after version `after`,
 * at most `limit` of them, in order. `markSaved` keeps the highest version
 * reported and returns it; the server never reports one past the head.
 * `readBase` and `read` reject for an epoch that is gone. `create` is
 * first-wins and leaves an existing epoch untouched. `drop` removes only the
 * epoch it names; after it, a `create` starts again at version 0 and the
 * dropped epoch's `(client, seq)` pairs are free. `ops` come back as equal
 * JSON; object key order may change. `storeContract` in
 * `@seuoffice/collab/conformance` checks all of this against an implementation.
 */
export interface CollabStore {
  load(documentId: string): Promise<EpochHead | null>
  create(documentId: string, epoch: NewEpoch): Promise<'created' | 'exists'>
  readBase(documentId: string, epochId: string): Promise<Uint8Array>
  append(documentId: string, epochId: string, change: AppendRequest): Promise<AppendResult>
  read(documentId: string, epochId: string, after: number, limit: number): Promise<Change[]>
  markSaved(documentId: string, epochId: string, v: number): Promise<number>
  drop(documentId: string, epochId: string): Promise<void>
}

interface StoredEpoch {
  id: string
  format: Format
  baseHash: string
  base: Uint8Array
  log: Change[]
  saved: number
  /** `${client}\n${seq}` to the version it was appended at */
  submits: Map<string, number>
}

/** Epochs in this process's memory; everything is lost when it exits */
export function memoryStore(): CollabStore {
  return liveStore(new Map())
}

function liveStore(
  epochs: Map<string, StoredEpoch>,
  retain?: (documentId: string, epoch: StoredEpoch) => void,
): CollabStore {
  function current(documentId: string, epochId: string): StoredEpoch | undefined {
    const epoch = epochs.get(documentId)
    return epoch?.id === epochId ? epoch : undefined
  }

  function existing(documentId: string, epochId: string): StoredEpoch {
    const epoch = current(documentId, epochId)
    if (!epoch) throw new Error(`epoch ${epochId} of ${documentId} is gone`)
    return epoch
  }

  return {
    async load(documentId) {
      const epoch = epochs.get(documentId)
      if (!epoch) return null
      const { id, format, baseHash, saved } = epoch
      return {
        id,
        format: retain ? { ...format } : format,
        baseHash,
        head: epoch.log.length,
        saved,
      }
    },
    async create(documentId, epoch) {
      if (epochs.has(documentId)) return 'exists'
      const entry: StoredEpoch = {
        ...epoch,
        format: retain ? { ...epoch.format } : epoch.format,
        base: retain ? new Uint8Array(epoch.base) : epoch.base,
        log: [],
        saved: 0,
        submits: new Map(),
      }
      retain?.(documentId, entry)
      epochs.set(documentId, entry)
      return 'created'
    },
    async readBase(documentId, epochId) {
      const { base } = existing(documentId, epochId)
      return retain ? new Uint8Array(base) : base
    },
    async append(documentId, epochId, change) {
      const epoch = current(documentId, epochId)
      if (!epoch) return { kind: 'gone' }
      const key = `${change.client}\n${change.seq}`
      const earlier = epoch.submits.get(key)
      if (earlier !== undefined) return { kind: 'duplicate', v: earlier }
      if (change.base !== epoch.log.length) return { kind: 'stale', head: epoch.log.length }
      const v = epoch.log.length + 1
      const { client, seq, author, ops } = change
      epoch.log.push({ v, client, seq, author, ops: retain ? structuredClone(ops) : ops })
      epoch.submits.set(key, v)
      return { kind: 'appended', v }
    },
    async read(documentId, epochId, after, limit) {
      const page = existing(documentId, epochId).log.slice(after, after + limit)
      return retain ? structuredClone(page) : page
    },
    async markSaved(documentId, epochId, v) {
      const epoch = existing(documentId, epochId)
      epoch.saved = Math.max(epoch.saved, Math.min(v, epoch.log.length))
      return epoch.saved
    },
    async drop(documentId, epochId) {
      if (current(documentId, epochId)) epochs.delete(documentId)
    },
  }
}

/** Process-local reference. Dropping the live epoch removes only its active pointer. */
export function memoryHistoryStore(
  authorize: (principal: Principal, action: HistoryAction) => boolean,
  authorizeTarget: UndoTargetPolicy = (p, target) => target.author === p.userId,
): RetainedHistoryStore {
  const active = new Map<string, StoredEpoch>()
  const archive = new Map<string, StoredEpoch>()
  const revisions = new Map<string, HistoryArtifact>()
  const receipts = new Map<string, { intent: string; receipt: HistoryReceipt }>()
  const undone = new Set<string>()
  const key = (...parts: string[]) => JSON.stringify(parts)

  function allowed(p: Principal, action: HistoryAction): void {
    if (
      !p?.documentId ||
      !p.userId ||
      (action !== 'export' && p.canEdit !== true) ||
      authorize(p, action) !== true
    )
      throw new Error('history forbidden')
  }

  function allowedUndo(p: Principal, { epoch, v, index }: HistoryOpId): void {
    allowed(p, 'undo')
    const live = active.get(p.documentId)
    if (!live || live.id !== epoch) throw new Error('stale history head')
    const change = live.log[v - 1]
    if (
      !change ||
      change.v !== v ||
      !change.author ||
      !Number.isSafeInteger(index) ||
      index < 0 ||
      index >= change.ops.length ||
      authorizeTarget(p, { epoch, v, index, author: change.author }) !== true
    )
      throw new Error('undo target unavailable')
  }

  function lookup(p: Principal, cmd: HistoryCommand) {
    if (cmd.kind === 'undo') {
      if (cmd.target.epoch !== cmd.expected.epoch) throw new Error('invalid undo target')
      allowedUndo(p, cmd.target)
    } else allowed(p, cmd.kind)
    const id = key(p.documentId, p.userId, cmd.requestId)
    const { expected } = cmd
    const intent = JSON.stringify([
      cmd.kind,
      expected.epoch,
      expected.version,
      ...(cmd.kind === 'restore'
        ? [cmd.source.epoch, cmd.source.version]
        : [cmd.target.epoch, cmd.target.v, cmd.target.index]),
    ])
    const prior = receipts.get(id)
    if (prior && prior.intent !== intent) throw new Error('conflicting history request id')
    return { id, intent, prior: prior?.receipt }
  }

  const store = liveStore(active, (doc, epoch) => {
    const id = key(doc, epoch.id)
    if (archive.has(id)) throw new Error('retained epoch id already used')
    archive.set(id, epoch)
  })
  return {
    ...store,
    async authorizeHistory(p, action) {
      allowed(p, action)
    },
    async authorizeUndoTarget(p, target) {
      allowedUndo(p, target)
    },
    async readHistory(p, { epoch, version }, action) {
      allowed(p, action)
      const entry = archive.get(key(p.documentId, epoch))
      if (!entry || !Number.isSafeInteger(version) || version < 0 || version > entry.log.length)
        throw new Error('history unavailable')
      return structuredClone({
        baseBytes: entry.base,
        request: {
          epoch,
          version,
          format: entry.format,
          baseHash: entry.baseHash,
          changes: entry.log.slice(0, version),
        },
      })
    },
    async historyReceipt(p, command) {
      const { prior } = lookup(p, command)
      return prior ? { ...prior } : null
    },
    async commitHistory(p, write) {
      // No await between the ACL, receipt, head check and writes.
      const { command } = write
      const { id, intent, prior } = lookup(p, command)
      if (prior) return { ...prior }
      const live = active.get(p.documentId)
      if (
        !live ||
        live.id !== command.expected.epoch ||
        live.log.length !== command.expected.version
      )
        throw new Error('stale history head')
      let receipt: HistoryReceipt
      if ('artifact' in write) {
        const revisionId = crypto.randomUUID()
        const { fileName, bytes } = write.artifact
        revisions.set(key(p.documentId, revisionId), { fileName, bytes: new Uint8Array(bytes) })
        receipt = { kind: 'restored', revisionId }
      } else {
        const { epoch, v, index } = write.command.target
        const target = JSON.stringify([p.documentId, epoch, v, index])
        if (undone.has(target)) throw new Error('target already undone')
        const client = `history:${crypto.randomUUID()}`
        const version = live.log.length + 1
        live.log.push({
          v: version,
          client,
          seq: 1,
          author: p.userId,
          ops: structuredClone(write.compensation),
        })
        live.submits.set(`${client}\n1`, version)
        undone.add(target)
        receipt = { kind: 'undone', version }
      }
      receipts.set(id, { intent, receipt })
      return { ...receipt }
    },
    async readRevision(p, revisionId) {
      allowed(p, 'export')
      const revision = revisions.get(key(p.documentId, revisionId))
      if (!revision) throw new Error('revision unavailable')
      return structuredClone(revision)
    },
  }
}
