import assert from 'node:assert/strict'
import { fork } from 'node:child_process'
import { closeSync, createWriteStream, openSync, readSync, realpathSync } from 'node:fs'
import { stat } from 'node:fs/promises'
import { fileURLToPath } from 'node:url'
import { pipeline } from 'node:stream/promises'
import { DatabaseSync } from 'node:sqlite'
import { createHistoryServer } from '@seuoffice/collab/server'
import { renderStream } from '@seuoffice/history-renderer'
import { original, sha } from './stream-proof-fixture.mjs'

const renderer = import.meta.resolve('@seuoffice/history-renderer')
for (const entry of [renderer, import.meta.resolve('@seuoffice/collab/server')])
  assert(
    realpathSync(fileURLToPath(entry)).includes('/node_modules/'),
    'install both tarballs in the consumer',
  )
const example = (file) => new URL('../examples/' + file, renderer)
const { createStreamStore } = await import(example('stream-store.mjs'))
export const { createStreamHost, downloadHistory } = await import(example('stream-host.mjs'))
export const format = { name: 'pdf', schema: 2 }
export const alice = { documentId: 'survey', userId: 'Alice', name: 'Alice', canEdit: true }
export const bob = { ...alice, userId: 'Bob', name: 'Bob' }
export const point = (epoch, version) => ({ epoch, version })
export const ownTarget = (principal, target) => principal.userId === target.author

export function* chunks(path) {
  const fd = openSync(path, 'r')
  try {
    const buf = Buffer.allocUnsafe(64 * 1024)
    for (let n; (n = readSync(fd, buf)) > 0;) yield buf.subarray(0, n)
  } finally {
    closeSync(fd)
  }
}

function* lines(path) {
  let rest = Buffer.alloc(0)
  for (const part of chunks(path)) {
    const buf = rest.length ? Buffer.concat([rest, part]) : part
    let start = 0
    for (let end; (end = buf.indexOf(10, start)) !== -1; start = end + 1)
      yield buf.subarray(start, end)
    rest = Buffer.from(buf.subarray(start))
  }
  assert.equal(rest.length, 0, 'truncated fixture line')
}

export function seed(host, pdf, log, epoch, principal = alice) {
  function* frames() {
    for (const bytes of lines(log.path)) {
      // Fixture setup represents trusted admission. Preserve the exact original frame bytes.
      const { v, client, seq, author, ops } = JSON.parse(bytes)
      yield { v, client, seq, author, opCount: ops.length, bytes }
    }
  }
  assert.equal(
    host.seed(principal.documentId, {
      id: epoch,
      format,
      baseHash: pdf.hash,
      baseChunks: chunks(pdf.path),
      changes: frames(),
    }),
    'created',
  )
}

export function connectDb(path, authorize, target = ownTarget, render = renderStream) {
  const db = new DatabaseSync(path)
  db.exec('PRAGMA journal_mode=WAL')
  const host = createStreamStore(db, authorize, target)
  const server = createHistoryServer({ store: host.store, renderStream: render })
  return {
    ...host,
    db,
    server,
    async close() {
      await server.close()
      db.close()
    },
  }
}

export function counts(db) {
  const all = (sql) => db.prepare(sql).get().n
  const kinds = Object.fromEntries(
    ['base', 'revision', 'private', 'export'].map((kind) => [
      kind,
      db.prepare('SELECT count(*) AS n FROM objects WHERE kind=?').get(kind).n,
    ]),
  )
  return {
    ...kinds,
    chunks: all('SELECT count(*) AS n FROM chunks'),
    frames: all('SELECT count(*) AS n FROM frames'),
    receipts: all('SELECT count(*) AS n FROM receipts'),
    undone: all('SELECT count(*) AS n FROM undone'),
  }
}

export async function save(artifact, path) {
  try {
    assert.equal(artifact.fileName, 'history.pdf')
    await pipeline(artifact.open(), createWriteStream(path, { flags: 'wx' }))
    assert.equal((await stat(path)).size, artifact.byteLength)
  } finally {
    await artifact.dispose?.()
  }
}

export function reopen(pdf, path, meta, angles) {
  const child = fork(new URL('./stream-proof-reopen.mjs', import.meta.url), {
    stdio: ['ignore', 'inherit', 'inherit', 'ipc'],
  })
  return new Promise((resolve, reject) => {
    let result
    child.once('message', (message) => {
      result = message
    })
    child.once('error', reject)
    child.once('exit', (code, signal) =>
      code === 0 && result
        ? resolve(result)
        : reject(new Error(`PDF reopen failed: ${path} (${signal ?? code})`)),
    )
    child.send({ path, meta: { ...original, ...meta }, angles, images: pdf.images })
  })
}

export function retainedDigest(db, doc, epoch) {
  const digest = sha()
  for (const { bytes } of db
    .prepare('SELECT bytes FROM frames WHERE doc=? AND epoch=? ORDER BY v')
    .iterate(doc, epoch))
    digest.update(bytes).update('\n')
  return digest.digest('hex')
}

export function socket(server, principal, hash) {
  const frames = [],
    waiting = new Map()
  let receive, error
  const fail = (message) => {
    error = new Error(message)
    for (const { reject } of waiting.values()) reject(error)
    waiting.clear()
  }
  const next = (kind) => {
    if (error) return Promise.reject(error)
    const event = Promise.withResolvers()
    waiting.set(kind, event)
    return event.promise
  }
  server.accept(
    {
      onMessage(fn) {
        receive = fn
      },
      onClose() {},
      close(code, reason) {
        fail(`socket closed ${code}: ${reason}`)
      },
      send(raw) {
        assert.equal(typeof raw, 'string', 'matching hello hash needs no base bytes')
        const frame = JSON.parse(raw)
        if (frame.t === 'error' || frame.t === 'stale') fail(JSON.stringify(frame))
        if (frame.t === 'change' && frame.client.startsWith('history:')) frames.push(frame)
        waiting.get(frame.t)?.resolve(frame)
        waiting.delete(frame.t)
      },
    },
    principal,
  )
  const ready = next('welcome')
  receive(JSON.stringify({ t: 'hello', p: 1, format, fileHash: hash }))
  return {
    ready,
    frames,
    submit(base, ops) {
      const ack = next('ack')
      receive(JSON.stringify({ t: 'submit', base, seq: 1, ops }))
      return ack
    },
  }
}
