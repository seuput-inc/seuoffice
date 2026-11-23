import assert from 'node:assert/strict'
import { createWriteStream } from 'node:fs'
import { mkdir, mkdtemp, stat } from 'node:fs/promises'
import { join, resolve } from 'node:path'
import { DatabaseSync } from 'node:sqlite'
import { renderStream } from '@seuoffice/history-renderer'
import { epoch, fileHash, makeHistory, makePdf, MiB, sha } from './stream-proof-fixture.mjs'
import { exactPdf, freshLog } from './stream-proof-exact.mjs'
import {
  alice,
  bob,
  chunks,
  connectDb,
  counts,
  createStreamHost,
  downloadHistory,
  format,
  ownTarget,
  point,
  reopen,
  retainedDigest,
  save,
  seed,
  socket,
} from './sdk-stream-fixture.mjs'
import { failures } from './sdk-stream-failures.mjs'

const [mode, out] = process.argv.slice(2)
assert(
  out && ['small', 'exact', 'faults', 'all'].includes(mode),
  'node sdk-stream.mjs <small|exact|faults|all> <scratch-directory>',
)
await mkdir(out, { recursive: true })
const root = await mkdtemp(join(resolve(out), 'sdk-stream-'))
console.log(JSON.stringify({ root, node: process.version, mode }))
const limit = 32 * MiB
const grants = (who, action) =>
  who.documentId === alice.documentId && (action === 'export' || who.canEdit === true)

function observed(path, authorize = grants, target = ownTarget) {
  const calls = []
  let rendezvous
  const host = connectDb(
    path,
    authorize,
    target,
    async (source, header, changes, stage, signal) => {
      const trace = {
        base: source.byteLength,
        json: header.length + ',"changes":[]'.length,
        frames: 0,
        staged: 0,
      }
      calls.push(trace)
      const digest = sha()
      async function* countFrames() {
        for await (const bytes of changes) {
          trace.json += (trace.frames++ ? 1 : 0) + bytes.length
          digest.update(bytes).update('\n')
          yield bytes
        }
        trace.historySha256 = digest.digest('hex')
      }
      const outputHash = sha()
      const result = await renderStream(
        source,
        header,
        countFrames(),
        {
          ...stage,
          async write(bytes) {
            await stage.write(bytes)
            trace.staged += bytes.length
            outputHash.update(bytes)
          },
        },
        signal,
      )
      trace.completed = true
      trace.outputSha256 = outputHash.digest('hex')
      if (rendezvous) await rendezvous()
      return result
    },
  )
  return {
    ...host,
    calls,
    async duplicates(run) {
      const barrier = Promise.withResolvers()
      let arrived = 0
      rendezvous = () => {
        if (++arrived === 2) {
          assert.equal(counts(host.db).private, 2, 'both renders own a distinct private stage')
          barrier.resolve()
        }
        return barrier.promise
      }
      try {
        const [first, second] = await Promise.all([run(), run()])
        assert.equal(arrived, 2)
        assert.deepEqual(second, first)
        return first
      } finally {
        rendezvous = undefined
        barrier.resolve()
      }
    },
  }
}

async function output(host, pdf, dir, label, point, meta, angles) {
  const path = join(dir, label + '.pdf')
  const artifact = await host.server.exportHistory(alice, point)
  assert.equal(counts(host.db).export, 1)
  await save(artifact, path)
  assert.equal(counts(host.db).export, 0)
  return (await reopen(pdf, path, meta, angles)).sha256
}

async function restoreCase(dir, pdf, log, epoch, exact) {
  const dbPath = join(dir, 'restore.sqlite')
  const host = observed(dbPath)
  const head = point(epoch, log.version),
    prefix = point(epoch, exact ? 0 : 1)
  const request = { requestId: 'fork terrain', expected: head, source: head }
  const path = join(dir, 'revision.pdf')
  let receipt, headHash
  try {
    seed(host, pdf, log, epoch)
    const before = counts(host.db)
    await output(
      host,
      pdf,
      dir,
      'prefix',
      prefix,
      exact ? { subject: pdf.subject } : log.prefixMeta,
      exact ? [0, 0] : [90, 0],
    )
    headHash = await output(host, pdf, dir, 'head', head, log.lastMeta, [90, 270])
    assert.deepEqual(counts(host.db), before)
    const trace = host.calls.at(-1)
    assert.deepEqual(
      [trace.base, trace.json, trace.frames, trace.historySha256],
      [pdf.size, log.jsonBytes, log.version, await fileHash(log.path)],
    )
    assert.equal(trace.staged, (await stat(join(dir, 'head.pdf'))).size)
    assert.equal(trace.outputSha256, headHash)
    receipt = await host.duplicates(() => host.server.restoreHistory(alice, request))
    assert.equal(receipt.kind, 'restored')
    const after = counts(host.db)
    assert.deepEqual(after, { ...before, chunks: after.chunks, receipts: 1, revision: 1 })
    assert(after.chunks > before.chunks)
    const renders = host.calls.length
    assert.deepEqual(await host.server.restoreHistory(alice, request), receipt)
    await assert.rejects(
      host.server.restoreHistory(alice, { ...request, source: prefix }),
      /conflicting/,
    )
    assert.equal(host.calls.length, renders)
    assert.deepEqual(counts(host.db), after)
    await save(host.readRevision(alice, receipt.revisionId), path)
    assert.equal((await reopen(pdf, path, log.lastMeta, [90, 270])).sha256, headHash)
    const live = await host.store.load(alice.documentId)
    assert.deepEqual(
      [live.id, live.head, live.saved, live.baseHash],
      [epoch, log.version, 0, pdf.hash],
    )
    assert.equal(retainedDigest(host.db, alice.documentId, epoch), trace.historySha256)
    console.log(JSON.stringify({ command: 'restoreHistory', receipt, trace, after }))
  } finally {
    await host.close()
  }

  const database = new DatabaseSync(dbPath)
  const forkOwner = { ...bob, documentId: 'review-copy' }
  const restarted = createStreamHost(
    database,
    (who, action) =>
      grants(who, action) ||
      (who.documentId === forkOwner.documentId && who.userId === forkOwner.userId),
  )
  try {
    const before = counts(database)
    assert.deepEqual(await restarted.server.restoreHistory(alice, request), receipt)
    assert.deepEqual(counts(database), before)
    const forkEpoch = 'field-review'
    restarted.seed(forkOwner.documentId, {
      id: forkEpoch,
      format,
      baseHash: headHash,
      baseChunks: chunks(path),
    })
    const reviewer = socket(restarted.server, forkOwner, headHash)
    await reviewer.ready
    const edit = { title: 'Field review', subject: 'Eastern ridge review' }
    assert.equal(
      (await reviewer.submit(0, [{ id: 'fork-note', puts: [{ b: 'metadata', v: edit }] }])).v,
      1,
    )
    const edited = join(dir, 'edited-fork.pdf')
    await downloadHistory(
      restarted.server,
      forkOwner,
      point(forkEpoch, 1),
      createWriteStream(edited, { flags: 'wx' }),
    )
    assert.notEqual((await reopen(pdf, edited, edit, [90, 270])).sha256, headHash)
    const originalHead = await restarted.store.load(alice.documentId)
    assert.deepEqual(
      [originalHead.id, originalHead.head, originalHead.baseHash],
      [epoch, log.version, pdf.hash],
    )
    await restarted.server.reset(alice.documentId)
    const archived = join(dir, 'archived.pdf')
    await save(await restarted.server.exportHistory(alice, head), archived)
    assert.equal(await fileHash(archived), headHash)
    const reread = join(dir, 'revision-after-fork.pdf')
    await save(restarted.readRevision(alice, receipt.revisionId), reread)
    assert.equal(await fileHash(reread), headHash)
    assert.equal(retainedDigest(database, alice.documentId, epoch), await fileHash(log.path))
    assert.equal(counts(database).private + counts(database).export, 0)
  } finally {
    await restarted.server.close()
    database.close()
  }
}

async function undoCase(dir, pdf, log, epoch, budget) {
  const longAuthor = 'é'.repeat(1024)
  let grant = true
  const host = observed(
    join(dir, 'undo.sqlite'),
    grants,
    (who, target) =>
      grant && target.author === 'Alice' && [alice.userId, longAuthor].includes(who.userId),
  )
  try {
    seed(host, pdf, log, epoch)
    const before = counts(host.db)
    const expected = point(epoch, log.version)
    const command = { requestId: 'undo west', expected, target: { epoch, v: 1, index: 1 } }
    if (budget) {
      const p = budget === limit ? alice : { ...alice, userId: longAuthor }
      await assert.rejects(host.server.undoHistory(p, command), { code: 'unsupported' })
      const trace = host.calls.at(-1)
      assert.deepEqual(
        [trace.base, trace.json, trace.frames, trace.historySha256],
        [pdf.size, budget, log.version, await fileHash(log.path)],
      )
      assert.deepEqual(counts(host.db), before)
      if (budget === limit) {
        assert.equal(trace.staged, 0, 'the worker refuses compensation at the exact cap')
        console.log(
          JSON.stringify({ rejected: 'undo replay at cap', trace, after: counts(host.db) }),
        )
        return
      }
      assert(
        trace.completed && trace.staged > 0,
        'the real worker succeeded before the SDK rejected the actual author size',
      )
      console.log(
        JSON.stringify({
          rejected: 'actual author exceeds replay cap',
          authorBytes: Buffer.byteLength(longAuthor),
          trace,
          after: counts(host.db),
        }),
      )
    }
    const peer = socket(host.server, bob, pdf.hash)
    await peer.ready
    const receipt = await host.duplicates(() => host.server.undoHistory(alice, command))
    assert.deepEqual(receipt, { kind: 'undone', version: log.version + 1 })
    const after = counts(host.db)
    assert.deepEqual(after, { ...before, frames: before.frames + 1, receipts: 1, undone: 1 })
    const renders = host.calls.length
    assert.deepEqual(await host.server.undoHistory(alice, command), receipt)
    await assert.rejects(
      host.server.undoHistory(alice, { ...command, expected: point(epoch, receipt.version) }),
      /conflicting/,
    )
    grant = false
    await assert.rejects(host.server.undoHistory(alice, command), /target/)
    grant = true
    assert.equal(host.calls.length, renders)
    assert.deepEqual(counts(host.db), after)
    const appended = await host.store.read(alice.documentId, epoch, log.version, 1)
    assert.equal(appended.length, 1)
    const [change] = appended
    assert.match(change.client, /^history:[\da-f]{8}-(?:[\da-f]{4}-){3}[\da-f]{12}$/)
    assert.deepEqual(
      [change.seq, change.author, change.v, change.ops.length],
      [1, alice.userId, receipt.version, 1],
    )
    assert.deepEqual(change.ops[0].puts, [{ b: 'rotations', k: 0, was: 90, v: 0 }])
    assert.deepEqual(peer.frames, [{ t: 'change', ...change }])
    const staged = host.calls.at(-1).outputSha256
    assert.equal(
      await output(
        host,
        pdf,
        dir,
        'replayed',
        point(epoch, receipt.version),
        log.lastMeta,
        [0, 270],
      ),
      staged,
    )
    assert.equal(host.calls.at(-1).frames, receipt.version)
    assert(host.calls.at(-1).json <= limit)
    assert.deepEqual(counts(host.db), after)
    console.log(
      JSON.stringify({ command: 'undoHistory', receipt, replay: host.calls.at(-1), after }),
    )
  } finally {
    await host.close()
  }
}

async function exactLog(dir, pdf, epoch, budget, undo) {
  await mkdir(dir, { recursive: true })
  const log = await freshLog(dir, pdf, budget, epoch, undo)
  return { ...log, version: log.newHeader.version, lastMeta: log.finalMeta, jsonBytes: budget }
}

if (mode === 'small' || mode === 'all') {
  const dir = join(root, 'small')
  await mkdir(dir)
  const pdf = makePdf(dir, MiB),
    log = makeHistory(dir, pdf, MiB / 4)
  await restoreCase(dir, pdf, log, epoch, false)
  await undoCase(dir, pdf, log, epoch)
}
if (mode === 'faults' || mode === 'all') await failures(root)
if (mode === 'exact' || mode === 'all') {
  const dir = join(root, 'exact')
  await mkdir(dir)
  const pdf = exactPdf(dir)
  assert.equal(pdf.size, 50 * MiB)
  assert.equal((await reopen(pdf, pdf.path, { subject: pdf.subject }, [0, 0])).sha256, pdf.hash)
  const head = await exactLog(dir, pdf, epoch, limit, false)
  assert.deepEqual([head.version, head.ops], [1075, 34292])
  await restoreCase(dir, pdf, head, epoch, true)
  for (const [id, budget] of [
    ['survey-capacity', limit],
    ['survey-reserved', limit - 1024],
  ]) {
    const sub = join(dir, id),
      log = await exactLog(sub, pdf, id, budget, true)
    await undoCase(sub, pdf, log, id, budget)
    assert.equal(await fileHash(log.path), log.hash)
  }
  assert.equal(await fileHash(pdf.path), pdf.hash)
  assert.equal(await fileHash(head.path), head.hash)
}
console.log(JSON.stringify({ passed: mode, root }))
