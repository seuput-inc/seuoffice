// Run against the built collab/renderer pair on a supported sandbox host with node --test.
import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import { EventEmitter, once } from 'node:events'
import { readFile } from 'node:fs/promises'
import { DatabaseSync } from 'node:sqlite'
import { test } from 'node:test'
import { createHistoryServer, memoryHistoryStore } from '@seuoffice/collab/server'
import { renderBytes, renderHistory } from '@seuoffice/history-renderer'
import { createStreamStore } from '../../history-renderer/examples/stream-store.mjs'
import { HwpDocument, initSync } from '../../../vendor/rhwp-core/rhwp.js'

const p = { documentId: 'hangul', userId: 'editor', name: 'Editor', canEdit: true }
const sources = new Map()
initSync({
  module: await readFile(new URL('../../../vendor/rhwp-core/rhwp_bg.wasm', import.meta.url)),
})
const source = HwpDocument.createEmpty()
try {
  source.createBlankDocument()
  source.splitParagraph(0, 0, source.textToLogicalOffset(0, 0, 0))
  source.insertText(0, 1, 0, 'A')
  sources.set('hwp', source.exportHwp())
  sources.set('hwpx', source.exportHwpx())
} finally {
  source.free()
}

function socket(server, format, bytes) {
  const events = new EventEmitter()
  let receive
  let reason
  server.accept(
    {
      send(raw) {
        if (typeof raw !== 'string') return
        const message = JSON.parse(raw)
        events.emit(message.t, message)
      },
      close(_code, detail) {
        reason = detail
      },
      onMessage(handler) {
        receive = handler
      },
      onClose() {},
    },
    p,
  )
  const reply = async (input, event) => {
    const next = once(events, event, { signal: AbortSignal.timeout(20_000) })
    receive(input instanceof Uint8Array ? input : JSON.stringify(input))
    return (await next)[0]
  }
  return {
    hello: (event = 'need_base') =>
      reply(
        {
          t: 'hello',
          p: 1,
          format: { name: format, schema: 1 },
          fileHash: createHash('sha256').update(bytes).digest('hex'),
        },
        event,
      ),
    reply,
    reason: () => reason,
  }
}

for (const [mode, format] of [
  ['bytes', 'hwp'],
  ['stream', 'hwpx'],
]) {
  test(`${mode}: admit only restorable ${format} socket history, without rejecting retries`, async () => {
    const db = mode === 'stream' ? new DatabaseSync(':memory:') : null
    const store = db
      ? createStreamStore(
          db,
          () => true,
          () => true,
        ).store
      : memoryHistoryStore(
          () => true,
          () => true,
        )
    let renders = 0
    const server = db
      ? createHistoryServer({
          store,
          renderStream: (...args) => {
            renders++
            return renderHistory(...args)
          },
        })
      : createHistoryServer({
          store,
          render: (base, request) => {
            renders++
            return renderBytes(base, Buffer.from(JSON.stringify(request)))
          },
        })
    try {
      const wrongSource = sources.get(format === 'hwp' ? 'hwpx' : 'hwp')
      const refused = socket(server, format, wrongSource)
      await refused.hello()
      assert.equal((await refused.reply(wrongSource, 'error')).code, 'schema_mismatch')
      assert.match(refused.reason(), /HWP.*history/i)
      assert.equal(await store.load(p.documentId), null)
      if (db) assert.equal(db.prepare('SELECT count(*) AS n FROM objects').get().n, 0)

      const bytes = sources.get(format)
      const editor = socket(server, format, bytes)
      await editor.hello()
      const { epoch } = await editor.reply(bytes, 'welcome')
      const at = { section: 0, paragraph: 1, path: [], offset: 1 }
      const op = { kind: 'splice', from: at, to: at, text: 'B' }
      const end = { ...at, offset: 2 }
      const afterB = { kind: 'splice', from: end, to: end, text: 'C' }
      const invalid = { kind: 'unrepresentable' }
      const submit = (seq, base, ops, event) => editor.reply({ t: 'submit', seq, base, ops }, event)
      assert.deepEqual(await submit(1, 0, [op], 'ack'), { t: 'ack', seq: 1, v: 1 })
      const beforeRetries = renders
      assert.deepEqual(await submit(1, 1, [invalid], 'ack'), { t: 'ack', seq: 1, v: 1 })
      assert.deepEqual(await submit(2, 0, [invalid], 'stale'), {
        t: 'stale',
        seq: 2,
        base: 0,
        head: 1,
      })
      assert.equal(renders, beforeRetries)
      assert.deepEqual(await submit(2, 1, [afterB], 'ack'), { t: 'ack', seq: 2, v: 2 })

      const peer = socket(server, format, bytes)
      await peer.hello('welcome')
      const bad = await peer.reply({ t: 'submit', seq: 1, base: 2, ops: [invalid] }, 'error')
      assert.equal(bad.code, 'schema_mismatch')
      assert.equal((await store.load(p.documentId)).head, 2)

      const oversized = socket(server, format, bytes)
      await oversized.hello('welcome')
      assert.equal(
        (
          await oversized.reply(
            {
              t: 'submit',
              seq: 1,
              base: 2,
              ops: Array(257).fill(op),
            },
            'error',
          )
        ).code,
        'too_large',
      )

      // This batch fits by itself, but 255 more operations would exceed 256 in the prefix.
      const refusal = await submit(3, 2, Array(255).fill(op), 'error')
      assert.ok(['too_large', 'schema_mismatch'].includes(refusal.code))
      assert.match(editor.reason(), /HWP.*history/i)
      assert.equal((await store.load(p.documentId)).head, 2)
      assert.deepEqual(
        (await store.read(p.documentId, epoch, 0, 3)).map((c) => c.ops),
        [[op], [afterB]],
      )
      const artifact = await server.exportHistory(p, { epoch, version: 2 })
      try {
        const chunks = []
        if (db) for await (const chunk of artifact.open()) chunks.push(chunk)
        const reopened = new HwpDocument(db ? Buffer.concat(chunks) : artifact.bytes)
        try {
          assert.equal(reopened.getSourceFormat(), format)
          assert.equal(reopened.getTextRange(0, 1, 0, 3), 'ABC')
        } finally {
          reopened.free()
        }
      } finally {
        await artifact.dispose?.()
      }
      const undo = {
        requestId: 'undo-C',
        expected: { epoch, version: 2 },
        target: { epoch, v: 2, index: 0 },
      }
      // The inverse fits, but its authenticated author would exceed the sandbox text budget.
      await assert.rejects(server.undoHistory({ ...p, userId: 'é'.repeat(131072) }, undo), {
        code: 'unsupported',
      })
      assert.equal((await store.load(p.documentId)).head, 2)
      assert.deepEqual(await server.undoHistory(p, undo), { kind: 'undone', version: 3 })
      const [compensation] = await store.read(p.documentId, epoch, 2, 1)
      assert.equal(compensation.author, p.userId)
      assert.match(compensation.client, /^history:/)
    } finally {
      await server.close()
      db?.close()
    }
  })
}

test('a legacy byte store cannot seed HWP without promising exact undo writes', async () => {
  const { exactHistoryChange, ...legacy } = memoryHistoryStore(() => true)
  assert.equal(exactHistoryChange, true)
  const server = createHistoryServer({
    store: legacy,
    render: (base, request) => renderBytes(base, Buffer.from(JSON.stringify(request))),
  })
  try {
    const bytes = sources.get('hwp')
    const editor = socket(server, 'hwp', bytes)
    await editor.hello()
    assert.equal((await editor.reply(bytes, 'error')).code, 'schema_mismatch')
    assert.match(editor.reason(), /exact undo change/)
    assert.equal(await legacy.load(p.documentId), null)
  } finally {
    await server.close()
  }
})
