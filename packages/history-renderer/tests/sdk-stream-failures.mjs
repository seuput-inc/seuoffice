import assert from 'node:assert/strict'
import { mkdtemp } from 'node:fs/promises'
import { join } from 'node:path'
import { renderStream } from '@seuoffice/history-renderer'
import { epoch, makeHistory, makePdf, MiB, turn } from './stream-proof-fixture.mjs'
import {
  alice,
  bob,
  chunks,
  connectDb,
  counts,
  format,
  ownTarget,
  point,
  seed,
  socket,
} from './sdk-stream-fixture.mjs'

export async function failures(root) {
  const dir = await mkdtemp(join(root, 'faults-'))
  const pdf = makePdf(dir, MiB),
    log = makeHistory(dir, pdf, 64 * 1024)
  const head = point(epoch, log.version),
    target = { epoch, v: 1, index: 1 }
  const source = point(epoch, 1)
  for (const [label, action, fault] of [
    ['export revoked on read', 'export', 'read-acl'],
    ['export revoked before publication', 'export', 'final-acl'],
    ['restore revoked before commit', 'restore', 'final-acl'],
    ['undo target grant revoked before commit', 'undo', 'final-target'],
    ['other author cannot impersonate target', 'undo', 'wrong-target'],
    ['cancel after allocation', 'restore', 'cancel-created'],
    ['cancel after first private chunk', 'export', 'cancel-write'],
    ['cancel immediately before commit', 'undo', 'cancel-commit'],
    ['private stage write fails', 'restore', 'write'],
    ['worker rejects conflicting rotation', 'undo', 'worker'],
    ['socket changes head during export', 'export', 'socket'],
    ['socket changes head during undo', 'undo', 'socket'],
    ['epoch replaced during restore', 'restore', 'epoch'],
    ['rollback revision before receipt', 'restore', 'receipt'],
    ['rollback compensation before receipt', 'undo', 'receipt'],
  ]) {
    const controller = new AbortController()
    let granted = true,
      targetGranted = true,
      renders = 0,
      writes = 0,
      hookHits = 0,
      peer
    const p = fault === 'wrong-target' ? bob : alice
    const host = connectDb(
      ':memory:',
      (who) => who.documentId === alice.documentId && granted,
      (who, stored) => targetGranted && ownTarget(who, stored),
      async (base, header, frames, stage, signal) => {
        renders++
        const file = await renderStream(
          base,
          header,
          frames,
          {
            ...stage,
            async write(bytes) {
              await stage.write(bytes)
              if (++writes === 1 && (fault === 'cancel-write' || fault === 'write')) {
                assert(counts(host.db).private > 0)
                hookHits++
                if (fault === 'write') throw new Error('private write failed')
                controller.abort()
              }
            },
          },
          signal,
        )
        if (fault === 'socket') {
          const ack = await peer.submit(head.version, [turn('competing-east', 1, 270, 180)])
          assert.equal(ack.v, head.version + 1)
          hookHits++
        } else if (fault === 'epoch') {
          await host.server.reset(p.documentId)
          host.seed(p.documentId, {
            id: 'replacement',
            format,
            baseHash: pdf.hash,
            baseChunks: chunks(pdf.path),
          })
          hookHits++
        }
        return file
      },
    )
    try {
      seed(host, pdf, log, epoch)
      let expected = head
      if (fault === 'worker') {
        assert.equal(
          (
            await host.store.append(p.documentId, epoch, {
              base: head.version,
              client: 'peer',
              seq: 1,
              author: 'Bob',
              ops: [turn('later-west', 0, 90, 180)],
            })
          ).kind,
          'appended',
        )
        expected = point(epoch, head.version + 1)
      }
      if (fault === 'socket') {
        peer = socket(host.server, bob, pdf.hash)
        await peer.ready
      }
      const method = action === 'export' ? 'publishHistoryExport' : 'commitHistoryStream'
      const final = host.store[method].bind(host.store)
      host.store[method] = (...args) => {
        if (fault === 'final-acl') {
          granted = false
          hookHits++
        }
        if (fault === 'final-target') {
          targetGranted = false
          hookHits++
        }
        if (fault === 'cancel-commit') {
          controller.abort()
          hookHits++
        }
        return final(...args)
      }
      if (fault === 'read-acl' || fault === 'cancel-created') {
        const create = host.store.createHistoryStage.bind(host.store)
        host.store.createHistoryStage = async (...args) => {
          const stage = await create(...args)
          assert.equal(counts(host.db).private, 1)
          if (fault === 'cancel-created') controller.abort()
          else granted = false
          hookHits++
          return stage
        }
      }
      if (fault === 'receipt') {
        const mutated =
          action === 'restore'
            ? "EXISTS (SELECT 1 FROM objects WHERE kind='revision')"
            : 'EXISTS (SELECT 1 FROM undone) AND EXISTS (SELECT 1 FROM frames WHERE v=NEW.result)'
        host.db.exec(`CREATE TRIGGER refuse_receipt BEFORE INSERT ON receipts BEGIN
          SELECT CASE WHEN ${mutated} THEN RAISE(ABORT, 'receipt unavailable')
          ELSE RAISE(ABORT, 'mutation was not visible inside transaction') END; END`)
      }
      const input = {
        requestId: label,
        expected,
        ...(action === 'restore' ? { source } : { target: { ...target, author: p.userId } }),
      }
      const before = counts(host.db)
      const failure =
        fault === 'receipt'
          ? /receipt unavailable/
          : fault === 'write'
            ? /private write failed/
            : fault === 'worker'
              ? { code: 'conflict' }
              : fault.startsWith('cancel')
                ? { name: 'AbortError' }
                : fault === 'socket' || fault === 'epoch'
                  ? /stale|fence/i
                  : /forbidden|target|denied/i
      await assert.rejects(
        host.server[action + 'History'](p, action === 'export' ? source : input, controller.signal),
        failure,
        label,
      )
      assert.equal(hookHits, ['wrong-target', 'worker', 'receipt'].includes(fault) ? 0 : 1, label)
      assert.equal(
        renders,
        ['wrong-target', 'read-acl', 'cancel-created'].includes(fault) ? 0 : 1,
        label,
      )
      const after = counts(host.db)
      if (fault === 'epoch') {
        assert.equal((await host.store.load(p.documentId)).id, 'replacement')
        assert.equal(after.base, before.base + 1)
        assert.deepEqual(after, { ...before, base: before.base + 1, chunks: before.chunks * 2 })
      } else {
        assert.deepEqual(
          after,
          { ...before, frames: before.frames + Number(fault === 'socket') },
          label,
        )
        const live = await host.store.load(p.documentId)
        assert.deepEqual(
          [live.id, live.head, live.saved],
          [epoch, expected.version + Number(fault === 'socket'), 0],
        )
      }
      assert.equal(after.private + after.export + after.receipts + after.revision + after.undone, 0)
      console.log(JSON.stringify({ rejected: label, renders, privateWrites: writes, after }))
    } finally {
      await host.close()
    }
  }
  let canRead = true
  const host = connectDb(':memory:', () => canRead)
  try {
    seed(host, pdf, log, epoch)
    const before = counts(host.db)
    const artifact = await host.server.exportHistory(alice, source)
    try {
      assert.equal(counts(host.db).export, 1)
      const reader = artifact.open()[Symbol.asyncIterator]()
      assert((await reader.next()).value.byteLength > 0)
      canRead = false
      await assert.rejects(reader.next(), /forbidden/)
    } finally {
      await artifact.dispose()
    }
    assert.deepEqual(counts(host.db), before)
    console.log(
      JSON.stringify({ rejected: 'export reader revoked after first chunk', after: before }),
    )
  } finally {
    await host.close()
  }
}
