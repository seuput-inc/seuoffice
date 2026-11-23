import { randomUUID } from 'node:crypto'

const CHUNK = 64 * 1024
const decoder = new TextDecoder()

/**
 * In-process example, not a public ACL service. The caller opens a node:sqlite DatabaseSync
 * in its own private directory and supplies authenticated principals and synchronous policies.
 * A process crash can leave private stages and disposable exports behind. Only an exclusive
 * owner, with all servers and readers stopped, may delete those rows during offline recovery.
 * @param {import('node:sqlite').DatabaseSync} database
 */
export function createStreamStore(
  database,
  authorize,
  authorizeTarget = (p, t) => p.userId === t.author,
) {
  database.exec(`
    PRAGMA foreign_keys = ON;
    CREATE TABLE IF NOT EXISTS objects (
      id TEXT PRIMARY KEY, doc TEXT NOT NULL,
      kind TEXT NOT NULL CHECK (kind IN ('base','private','export','revision')),
      byte_length INTEGER NOT NULL DEFAULT 0, file_name TEXT
    );
    CREATE TABLE IF NOT EXISTS chunks (
      object_id TEXT NOT NULL REFERENCES objects(id) ON DELETE CASCADE,
      n INTEGER NOT NULL, bytes BLOB NOT NULL, PRIMARY KEY(object_id,n)
    ) WITHOUT ROWID;
    CREATE TABLE IF NOT EXISTS epochs (
      doc TEXT NOT NULL, epoch TEXT NOT NULL, name TEXT NOT NULL, schema INTEGER NOT NULL,
      base_hash TEXT NOT NULL, base_id TEXT NOT NULL REFERENCES objects(id),
      head INTEGER NOT NULL DEFAULT 0, saved INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(doc,epoch)
    ) WITHOUT ROWID;
    CREATE TABLE IF NOT EXISTS live (
      doc TEXT PRIMARY KEY, epoch TEXT NOT NULL, FOREIGN KEY(doc,epoch) REFERENCES epochs(doc,epoch)
    );
    CREATE TABLE IF NOT EXISTS frames (
      doc TEXT NOT NULL, epoch TEXT NOT NULL, v INTEGER NOT NULL, client TEXT NOT NULL,
      seq INTEGER NOT NULL, author TEXT NOT NULL, op_count INTEGER NOT NULL, bytes BLOB NOT NULL,
      PRIMARY KEY(doc,epoch,v), UNIQUE(doc,epoch,client,seq), FOREIGN KEY(doc,epoch) REFERENCES epochs(doc,epoch)
    ) WITHOUT ROWID;
    CREATE TABLE IF NOT EXISTS receipts (
      doc TEXT NOT NULL, user TEXT NOT NULL, request_id TEXT NOT NULL, intent TEXT NOT NULL,
      result NOT NULL, PRIMARY KEY(doc,user,request_id)
    ) WITHOUT ROWID;
    CREATE TABLE IF NOT EXISTS undone (
      doc TEXT NOT NULL, epoch TEXT NOT NULL, v INTEGER NOT NULL, i INTEGER NOT NULL,
      PRIMARY KEY(doc,epoch,v,i), FOREIGN KEY(doc,epoch,v) REFERENCES frames(doc,epoch,v)
    ) WITHOUT ROWID;
  `)
  const statements = new Map()
  function sql(query) {
    let statement = statements.get(query)
    if (!statement) statements.set(query, (statement = database.prepare(query)))
    return statement
  }
  function transaction(work, signal) {
    database.exec('BEGIN IMMEDIATE')
    try {
      signal?.throwIfAborted()
      const result = work()
      signal?.throwIfAborted()
      database.exec('COMMIT')
      return result
    } catch (error) {
      // SQLite can also roll back the entire transaction itself (I/O failure or RAISE(ROLLBACK)).
      if (database.isTransaction !== false) {
        try {
          database.exec('ROLLBACK')
        } catch (rollback) {
          throw new AggregateError([error, rollback], 'history transaction failed')
        }
      }
      throw error
    }
  }
  const live = (doc) =>
    sql('SELECT e.* FROM live JOIN epochs e USING(doc,epoch) WHERE doc=?').get(doc)
  function current(doc, epoch) {
    const row = live(doc)
    if (row?.epoch !== epoch) throw new Error('epoch is gone')
    return row
  }
  function allowed(p, action) {
    if (
      !p?.documentId ||
      !p.userId ||
      (action !== 'export' && p.canEdit !== true) ||
      authorize(p, action) !== true
    )
      throw new Error('history forbidden')
  }
  function allowedUndo(p, target) {
    allowed(p, 'undo')
    const { epoch, v, index } = target
    if (live(p.documentId)?.epoch !== epoch) throw new Error('stale history head')
    const row = sql('SELECT author,op_count FROM frames WHERE doc=? AND epoch=? AND v=?').get(
      p.documentId,
      epoch,
      v,
    )
    if (
      !row?.author ||
      !Number.isSafeInteger(index) ||
      index < 0 ||
      index >= row.op_count ||
      authorizeTarget(p, { epoch, v, index, author: row.author }) !== true
    )
      throw new Error('undo target unavailable')
  }
  function lookup(p, command) {
    const { kind, expected, source, target, requestId } = command
    if (kind === 'undo') {
      if (expected.epoch !== target.epoch) throw new Error('invalid undo target')
      allowedUndo(p, target)
    } else allowed(p, kind)
    const key = [p.documentId, p.userId, requestId]
    const intent = JSON.stringify([
      kind,
      expected.epoch,
      expected.version,
      ...(kind === 'undo'
        ? [target.epoch, target.v, target.index]
        : [source.epoch, source.version]),
    ])
    const row = sql(
      'SELECT intent,result FROM receipts WHERE doc=? AND user=? AND request_id=?',
    ).get(...key)
    if (row && row.intent !== intent) throw new Error('conflicting history request id')
    const prior = !row
      ? null
      : kind === 'undo'
        ? { kind: 'undone', version: row.result }
        : { kind: 'restored', revisionId: row.result }
    return { key, intent, prior }
  }
  function object(doc, kind) {
    const id = randomUUID()
    sql('INSERT INTO objects(id,doc,kind) VALUES(?,?,?)').run(id, doc, kind)
    return id
  }
  function putBytes(id, n, bytes) {
    if (!(bytes instanceof Uint8Array)) throw new TypeError('expected bytes')
    const insert = sql('INSERT INTO chunks(object_id,n,bytes) VALUES(?,?,?)')
    for (let i = 0; i < bytes.byteLength; i += CHUNK)
      insert.run(id, n++, bytes.subarray(i, i + CHUNK))
    return n
  }
  const chunk = (id, n) =>
    sql('SELECT bytes FROM chunks WHERE object_id=? AND n=?').get(id, n)?.bytes
  function source(id, byteLength, check) {
    return {
      byteLength,
      async *open() {
        let read = 0
        for (let n = 0; ; n++) {
          check()
          if (read === byteLength) return
          const bytes = chunk(id, n)
          if (!bytes?.length || read + bytes.length > byteLength)
            throw new Error('object unavailable')
          read += bytes.length
          yield bytes
        }
      },
    }
  }
  const putFrame = (doc, epoch, v, client, seq, author, opCount, bytes) =>
    sql(
      'INSERT INTO frames(doc,epoch,v,client,seq,author,op_count,bytes) VALUES(?,?,?,?,?,?,?,?)',
    ).run(doc, epoch, v, client, seq, author, opCount, bytes)
  const head = (doc, epoch, v) =>
    sql('UPDATE epochs SET head=? WHERE doc=? AND epoch=?').run(v, doc, epoch)
  function appendFrame(doc, epoch, change) {
    const { v, client, seq, author, ops } = change
    putFrame(doc, epoch, v, client, seq, author, ops.length, Buffer.from(JSON.stringify(change)))
    head(doc, epoch, v)
  }
  // Trusted fixture admission. Both iterables are consumed once, in a single synchronous transaction.
  function seed(doc, { id, format, baseHash, baseChunks, changes = [] }) {
    return transaction(() => {
      if (live(doc)) return 'exists'
      const base = object(doc, 'base')
      let n = 0,
        total = 0,
        version = 0
      for (const bytes of baseChunks) {
        n = putBytes(base, n, bytes)
        total += bytes.byteLength
      }
      sql('UPDATE objects SET byte_length=? WHERE id=?').run(total, base)
      sql('INSERT INTO epochs(doc,epoch,name,schema,base_hash,base_id) VALUES(?,?,?,?,?,?)').run(
        doc,
        id,
        format.name,
        format.schema,
        baseHash,
        base,
      )
      for (const { v, client, seq, author, opCount, bytes } of changes) {
        if (v !== ++version) throw new Error('noncontiguous fixture frame')
        putFrame(doc, id, v, client, seq, author, opCount, bytes)
      }
      head(doc, id, version)
      sql('INSERT INTO live(doc,epoch) VALUES(?,?)').run(doc, id)
      return 'created'
    })
  }
  const stages = new WeakMap()
  function transfer(p, stage, action, kind, fileName) {
    const state = stages.get(stage)
    if (!state || state.doc !== p.documentId || state.user !== p.userId || state.action !== action)
      throw new Error('stage unavailable')
    if (action === 'export') {
      const now = live(state.doc)
      if (now?.epoch !== state.epoch || now?.head !== state.head)
        throw new Error('stale history head')
    }
    const { changes } = sql(
      "UPDATE objects SET kind=?,file_name=?,byte_length=? WHERE id=? AND kind='private'",
    ).run(kind, fileName, state.byteLength, state.id)
    if (changes !== 1) throw new Error('stage unavailable')
    return state
  }
  const store = {
    async load(doc) {
      const row = live(doc)
      return row
        ? {
            id: row.epoch,
            format: { name: row.name, schema: row.schema },
            baseHash: row.base_hash,
            head: row.head,
            saved: row.saved,
          }
        : null
    },
    async create(doc, { id, base, format, baseHash }) {
      return seed(doc, { id, format, baseHash, baseChunks: [base] })
    },
    async append(doc, epoch, { base, client, seq, author, ops }) {
      return transaction(() => {
        const row = live(doc)
        if (row?.epoch !== epoch) return { kind: 'gone' }
        const prior = sql(
          'SELECT v FROM frames WHERE doc=? AND epoch=? AND client=? AND seq=?',
        ).get(doc, epoch, client, seq)
        if (prior) return { kind: 'duplicate', v: prior.v }
        if (row.head !== base) return { kind: 'stale', head: row.head }
        const v = base + 1
        appendFrame(doc, epoch, { v, client, seq, author, ops })
        return { kind: 'appended', v }
      })
    },
    // Only the existing live socket protocol materializes a full base or parses its page of changes.
    async readBase(doc, epoch) {
      const { base_id } = current(doc, epoch)
      const { byte_length } = sql('SELECT byte_length FROM objects WHERE id=?').get(base_id)
      const bytes = new Uint8Array(byte_length)
      for (let n = 0, offset = 0; offset < byte_length; n++) {
        const part = chunk(base_id, n)
        if (!part?.length) throw new Error('base unavailable')
        bytes.set(part, offset)
        offset += part.length
      }
      return bytes
    },
    async read(doc, epoch, after, limit) {
      current(doc, epoch)
      return sql('SELECT bytes FROM frames WHERE doc=? AND epoch=? AND v>? ORDER BY v LIMIT ?')
        .all(doc, epoch, after, limit)
        .map(({ bytes }) => JSON.parse(decoder.decode(bytes)))
    },
    async markSaved(doc, epoch, v) {
      return transaction(() => {
        const row = current(doc, epoch)
        const saved = Math.max(row.saved, Math.min(row.head, v))
        sql('UPDATE epochs SET saved=? WHERE doc=? AND epoch=?').run(saved, doc, epoch)
        return saved
      })
    },
    async drop(doc, epoch) {
      sql('DELETE FROM live WHERE doc=? AND epoch=?').run(doc, epoch)
    },
    async authorizeHistory(p, action) {
      allowed(p, action)
    },
    async authorizeUndoTarget(p, target) {
      allowedUndo(p, target)
    },
    async historyReceipt(p, command) {
      return lookup(p, command).prior
    },
    async readHistoryStream(p, { epoch, version }, action) {
      const who = { ...p }
      const check = () => allowed(who, action)
      check()
      const doc = who.documentId
      const row = sql(
        'SELECT e.*,o.byte_length FROM epochs e JOIN objects o ON o.id=e.base_id WHERE e.doc=? AND epoch=?',
      ).get(doc, epoch)
      if (!row || !Number.isSafeInteger(version) || version < 0 || version > row.head)
        throw new Error('history unavailable')
      return {
        epoch: row.epoch,
        version,
        format: { name: row.name, schema: row.schema },
        baseHash: row.base_hash,
        source: source(row.base_id, row.byte_length, check),
        changes: (async function* () {
          for (let v = 1; v <= version; v++) {
            check()
            const frame = sql('SELECT bytes FROM frames WHERE doc=? AND epoch=? AND v=?').get(
              doc,
              epoch,
              v,
            )
            if (!frame) throw new Error('history unavailable')
            yield frame.bytes
          }
        })(),
      }
    },
    async createHistoryStage(p, action) {
      return transaction(() => {
        allowed(p, action)
        const { documentId: doc, userId: user } = p
        const fence = action === 'export' ? live(doc) : null
        const id = object(doc, 'private')
        const state = {
          id,
          doc,
          user,
          action,
          epoch: fence?.epoch,
          head: fence?.head,
          n: 0,
          byteLength: 0,
        }
        const stage = {
          async write(bytes) {
            if (!stages.has(stage)) throw new Error('stage unavailable')
            state.n = transaction(() => putBytes(id, state.n, bytes))
            state.byteLength += bytes.byteLength
          },
          async abort() {
            if (!stages.has(stage)) return
            sql("DELETE FROM objects WHERE id=? AND kind='private'").run(id)
            stages.delete(stage)
          },
        }
        stages.set(stage, state)
        return stage
      })
    },
    async publishHistoryExport(p, stage, fileName, signal) {
      const who = { ...p }
      const { id, byteLength } = transaction(() => {
        allowed(who, 'export')
        return transfer(who, stage, 'export', 'export', fileName)
      }, signal)
      stages.delete(stage)
      let disposed = false
      return {
        fileName,
        ...source(id, byteLength, () => {
          if (disposed) throw new Error('export disposed')
          allowed(who, 'export')
        }),
        async dispose() {
          if (disposed) return
          sql("DELETE FROM objects WHERE id=? AND kind='export'").run(id)
          disposed = true
        },
      }
    },
    async commitHistoryStream(p, write, signal) {
      const { command } = write
      const receipt = transaction(() => {
        const { key, intent, prior } = lookup(p, command)
        if (prior) return prior
        const { epoch, version } = command.expected
        const doc = p.documentId
        const now = live(doc)
        if (now?.epoch !== epoch || now.head !== version) throw new Error('stale history head')
        let result, receipt
        if (command.kind === 'restore') {
          result = transfer(p, write.stage, 'restore', 'revision', write.fileName).id
          receipt = { kind: 'restored', revisionId: result }
        } else {
          const { v, index } = command.target
          const marked = sql(
            'INSERT INTO undone(doc,epoch,v,i) VALUES(?,?,?,?) ON CONFLICT DO NOTHING',
          ).run(doc, epoch, v, index)
          if (!marked.changes) throw new Error('target already undone')
          const { change } = write
          if (
            change.v !== version + 1 ||
            change.author !== p.userId ||
            change.seq !== 1 ||
            !change.client.startsWith('history:')
          )
            throw new Error('invalid compensation change')
          appendFrame(doc, epoch, change)
          result = change.v
          receipt = { kind: 'undone', version: result }
        }
        sql('INSERT INTO receipts(doc,user,request_id,intent,result) VALUES(?,?,?,?,?)').run(
          ...key,
          intent,
          result,
        )
        return receipt
      }, signal)
      if (command.kind === 'restore' && stages.get(write.stage)?.id === receipt.revisionId)
        stages.delete(write.stage)
      return receipt
    },
  }
  function readRevision(p, revisionId) {
    const who = { ...p }
    const check = () => allowed(who, 'export')
    check()
    const row = sql(
      "SELECT file_name,byte_length FROM objects WHERE id=? AND doc=? AND kind='revision'",
    ).get(revisionId, who.documentId)
    if (!row) throw new Error('revision unavailable')
    return { fileName: row.file_name, ...source(revisionId, row.byte_length, check) }
  }
  return { store, seed, readRevision }
}
