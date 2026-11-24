// Run against the built collab/renderer pair on a supported sandbox host with node --test.
import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import { EventEmitter, once } from 'node:events'
import { readFile } from 'node:fs/promises'
import { DatabaseSync } from 'node:sqlite'
import { test } from 'node:test'
import { strFromU8, strToU8, unzipSync, zipSync } from 'fflate'
import { crc32, deflateSync } from 'node:zlib'
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
    const store = withoutCheckpoints(
      db
        ? createStreamStore(
            db,
            () => true,
            () => true,
          ).store
        : memoryHistoryStore(
            () => true,
            () => true,
          ),
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

      let opaque
      if (format === 'hwp') {
        opaque = await readFile(
          new URL('../../../apps/hwp/tests/fixtures/collab-opaque-record.hwp', import.meta.url),
        )
      } else {
        const parts = unzipSync(sources.get('hwpx'))
        const section = 'Contents/section0.xml'
        parts[section] = strToU8(
          strFromU8(parts[section]).replace('<hp:p ', '<hp:p data-sample="1" '),
        )
        opaque = zipSync(parts)
      }
      const unknown = socket(server, format, opaque)
      await unknown.hello()
      assert.equal((await unknown.reply(opaque, 'error')).code, 'schema_mismatch')
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

      // This batch fits by itself, but 255 more operations would exceed 256 in the prefix, and
      // neither store keeps HWP checkpoints.
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

/** A retained store as a host may write one: without the optional HWP checkpoint pair. */
function withoutCheckpoints(store) {
  const { writeHwpCheckpoint, readHwpCheckpoint, ...plain } = store
  assert.equal(typeof writeHwpCheckpoint, 'function')
  assert.equal(typeof readHwpCheckpoint, 'function')
  return plain
}

function splice(offset, text) {
  const at = { section: 0, paragraph: 1, path: [], offset }
  return { kind: 'splice', from: at, to: at, text }
}

for (const format of ['hwp', 'hwpx']) {
  test(`bytes: ${format} history continues past the replay limits from a checkpoint the renderer reopened`, async () => {
    const store = memoryHistoryStore(
      () => true,
      () => true,
    )
    const server = createHistoryServer({
      store,
      render: (base, request) => renderBytes(base, Buffer.from(JSON.stringify(request))),
    })
    try {
      const bytes = sources.get(format)
      const editor = socket(server, format, bytes)
      await editor.hello()
      const { epoch } = await editor.reply(bytes, 'welcome')
      const submit = (seq, base, ops) => editor.reply({ t: 'submit', seq, base, ops }, 'ack')
      assert.deepEqual(await submit(1, 0, [splice(1, 'B')]), { t: 'ack', seq: 1, v: 1 })
      // 1 + 256 ops exceed one replay window; the server keeps the file at version 1 and goes on.
      assert.deepEqual(await submit(2, 1, Array(256).fill(splice(2, 'C'))), {
        t: 'ack',
        seq: 2,
        v: 2,
      })
      assert.equal((await store.readHwpCheckpoint(p.documentId, epoch, 2)).version, 1)
      for (const [version, text] of [
        [1, 'AB'],
        [2, 'AB' + 'C'.repeat(256)],
      ]) {
        const file = await server.exportHistory(p, { epoch, version })
        const reopened = new HwpDocument(file.bytes)
        try {
          assert.equal(reopened.getTextRange(0, 1, 0, text.length), text)
        } finally {
          reopened.free()
        }
      }
      await assert.rejects(
        server.undoHistory(p, {
          requestId: 'before-checkpoint',
          expected: { epoch, version: 2 },
          target: { epoch, v: 1, index: 0 },
        }),
        /older than the saved history point/,
      )
    } finally {
      await server.close()
    }
  })
}

test('stream: an editor without the export grant still submits HWP edits', async () => {
  const db = new DatabaseSync(':memory:')
  const { store } = createStreamStore(
    db,
    (_p, action) => action !== 'export',
    () => true,
  )
  const server = createHistoryServer({ store, renderStream: renderHistory })
  try {
    const bytes = sources.get('hwpx')
    const editor = socket(server, 'hwpx', bytes)
    await editor.hello()
    await editor.reply(bytes, 'welcome')
    for (const [seq, text] of [
      [1, 'B'],
      [2, 'C'],
    ]) {
      const submit = { t: 'submit', seq, base: seq - 1, ops: [splice(seq, text)] }
      assert.deepEqual(await editor.reply(submit, 'ack'), { t: 'ack', seq, v: seq })
    }
  } finally {
    await server.close()
    db.close()
  }
})

/** A PNG of random pixels, so its size is about `side * side * 3` bytes. */
function noisePng(side) {
  const chunk = (type, data) => {
    const head = Buffer.alloc(8)
    head.writeUInt32BE(data.length)
    head.write(type, 4, 'ascii')
    const crc = Buffer.alloc(4)
    crc.writeUInt32BE(crc32(Buffer.concat([head.subarray(4), data])))
    return Buffer.concat([head, data, crc])
  }
  const ihdr = Buffer.alloc(13)
  ihdr.writeUInt32BE(side, 0)
  ihdr.writeUInt32BE(side, 4)
  ihdr.set([8, 2, 0, 0, 0], 8)
  const rows = Buffer.alloc((side * 3 + 1) * side)
  for (let i = 0; i < rows.length; i++) rows[i] = i % (side * 3 + 1) ? (i * 7919) % 251 : 0
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk('IHDR', ihdr),
    chunk('IDAT', deflateSync(rows, { level: 0 })),
    chunk('IEND', Buffer.alloc(0)),
  ])
}

function picture(png, side) {
  return {
    kind: 'insertCellPicture',
    at: { section: 0, paragraph: 1, path: [], offset: 0 },
    anchor: 'paper',
    data: png.toString('base64'),
    extension: 'png',
    description: '',
    width: side * 75,
    height: side * 75,
    naturalWidthPx: side,
    naturalHeightPx: side,
    offsetXHu: null,
    offsetYHu: null,
  }
}

test('stream: two pictures that overflow one replay request continue from a checkpoint', async () => {
  const db = new DatabaseSync(':memory:')
  const { store } = createStreamStore(
    db,
    () => true,
    () => true,
  )
  const server = createHistoryServer({ store, renderStream: renderHistory })
  try {
    const bytes = sources.get('hwpx')
    const editor = socket(server, 'hwpx', bytes)
    await editor.hello()
    const { epoch } = await editor.reply(bytes, 'welcome')
    // About 140 KB and 100 KB: each fits one request, together they pass its 256 KiB of text.
    const sides = [216, 182]
    for (const [i, side] of sides.entries()) {
      const png = noisePng(side)
      assert.ok(png.length > (i ? 95_000 : 135_000), `picture ${i + 1} is ${png.length} B`)
      const submit = { t: 'submit', seq: i + 1, base: i, ops: [picture(png, side)] }
      assert.deepEqual(await editor.reply(submit, 'ack'), { t: 'ack', seq: i + 1, v: i + 1 })
    }
    assert.equal(editor.reason(), undefined)
    assert.equal((await store.readHwpCheckpoint(p.documentId, epoch, 2)).version, 1)
    const artifact = await server.exportHistory(p, { epoch, version: 2 })
    try {
      const chunks = []
      for await (const chunk of artifact.open()) chunks.push(chunk)
      const parts = Object.keys(unzipSync(Buffer.concat(chunks)))
      assert.equal(parts.filter((name) => name.startsWith('BinData/')).length, 2)
    } finally {
      await artifact.dispose()
    }
  } finally {
    await server.close()
    db.close()
  }
})

test('bytes: run-heavy changes past the renderer node budget continue from a checkpoint', async () => {
  const store = memoryHistoryStore(
    () => true,
    () => true,
  )
  const server = createHistoryServer({
    store,
    render: (base, request) => renderBytes(base, Buffer.from(JSON.stringify(request))),
  })
  try {
    const bytes = sources.get('hwp')
    const editor = socket(server, 'hwp', bytes)
    await editor.hello()
    const { epoch } = await editor.reply(bytes, 'welcome')
    // 4,000 one-character runs are about 12,000 JSON nodes: two changes pass the renderer's
    // 16,384 while staying far below its change, op and text limits.
    const runs = (count) => ({
      kind: 'restoreText',
      at: { section: 0, paragraph: 1, path: [], offset: 1 },
      text: 'x'.repeat(count),
      runs: Array.from({ length: count }, (_, i) => [i, 0]),
    })
    for (const seq of [1, 2]) {
      const submit = { t: 'submit', seq, base: seq - 1, ops: [runs(4000)] }
      assert.deepEqual(await editor.reply(submit, 'ack'), { t: 'ack', seq, v: seq })
    }
    assert.equal(editor.reason(), undefined)
    assert.equal((await store.readHwpCheckpoint(p.documentId, epoch, 2)).version, 1)
    const file = await server.exportHistory(p, { epoch, version: 2 })
    const reopened = new HwpDocument(file.bytes)
    try {
      assert.equal(reopened.getTextRange(0, 1, 0, 8001), 'A' + 'x'.repeat(8000))
    } finally {
      reopened.free()
    }
  } finally {
    await server.close()
  }
})

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
