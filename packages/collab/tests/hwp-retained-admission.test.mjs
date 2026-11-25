// Run against the built collab/renderer pair on a supported sandbox host with node --test.
import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import { EventEmitter, once } from 'node:events'
import { readFile } from 'node:fs/promises'
import { DatabaseSync } from 'node:sqlite'
import { test } from 'node:test'
import { strFromU8, strToU8, unzipSync, zipSync } from 'fflate'
import { crc32, deflateSync } from 'node:zlib'
import {
  createCollabServer,
  createHistoryServer,
  memoryHistoryStore,
} from '@seuoffice/collab/server'
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

function socket(server, format, bytes, principal = p) {
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
    principal,
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

const blank = await readFile(
  new URL('../../../apps/hwp/src/renderer/assets/blank.hwp', import.meta.url),
)
const at = (paragraph, offset) => ({ section: 0, paragraph, path: [], offset })
// What the HWP editor sends for its first keys on its blank document: Hangul composition, a
// syllable, a paragraph split and a key on the new paragraph (captureHwpText of Studio's calls).
const typing = [
  { kind: 'splice', from: at(0, 0), to: at(0, 0), text: 'ㄱ' },
  { kind: 'nativeText', at: at(0, 0), action: 'bodyLocal', count: 1, text: '가', field: null },
  { kind: 'splice', from: at(0, 1), to: at(0, 1), text: '나' },
  { kind: 'splice', from: at(0, 2), to: at(0, 2), text: '\n' },
  { kind: 'splice', from: at(1, 0), to: at(1, 0), text: '다' },
]

function reopened(bytes, read) {
  const doc = new HwpDocument(bytes)
  try {
    return read(doc)
  } finally {
    doc.free()
  }
}

test('bytes: two editors type on the editor blank HWP past the first key, then export and undo', async () => {
  const store = memoryHistoryStore(
    () => true,
    () => true,
  )
  const server = createHistoryServer({
    store,
    render: (base, request) => renderBytes(base, Buffer.from(JSON.stringify(request))),
  })
  try {
    const alice = socket(server, 'hwp', blank, { ...p, userId: 'alice', name: 'Alice' })
    await alice.hello()
    const { epoch } = await alice.reply(blank, 'welcome')
    const bob = socket(server, 'hwp', blank, { ...p, userId: 'bob', name: 'Bob' })
    await bob.hello('welcome')
    const seqs = new Map()
    for (const [i, op] of typing.entries()) {
      const editor = i === 2 || i === 4 ? bob : alice
      const seq = (seqs.get(editor) ?? 0) + 1
      seqs.set(editor, seq)
      const submit = { t: 'submit', seq, base: i, ops: [op] }
      assert.deepEqual(await editor.reply(submit, 'ack'), { t: 'ack', seq, v: i + 1 })
    }
    const text = ({ bytes }) =>
      reopened(bytes, (doc) => [doc.getTextRange(0, 0, 0, 2), doc.getTextRange(0, 1, 0, 1)])
    assert.deepEqual(text(await server.exportHistory(p, { epoch, version: 5 })), ['가나', '다'])
    const undo = {
      requestId: 'undo-da',
      expected: { epoch, version: 5 },
      target: { epoch, v: 5, index: 0 },
    }
    assert.deepEqual(await server.undoHistory({ ...p, userId: 'bob' }, undo), {
      kind: 'undone',
      version: 6,
    })
    const after = await server.exportHistory(p, { epoch, version: 6 })
    assert.equal(
      reopened(after.bytes, (doc) => doc.getParagraphLength(0, 1)),
      0,
    )
  } finally {
    await server.close()
  }
})

// As a host that creates the epoch row over HTTP before the editors' sockets join: the first
// socket finds an existing epoch, so no base upload is admitted before the first edit.
for (const exact of [false, true]) {
  test(`bytes: two editors co-edit an epoch the host created at upload, on a store ${exact ? 'with' : 'without'} exact history writes`, async () => {
    const { exactHistoryChange, ...plain } = memoryHistoryStore(
      () => true,
      () => true,
    )
    assert.equal(exactHistoryChange, true)
    const store = exact ? { ...plain, exactHistoryChange } : plain
    const baseHash = createHash('sha256').update(blank).digest('hex')
    const format = { name: 'hwp', schema: 1 }
    await store.create(p.documentId, { id: 'upload', format, baseHash, base: blank })
    const server = createHistoryServer({
      store,
      render: (base, request) => renderBytes(base, Buffer.from(JSON.stringify(request))),
    })
    try {
      const alice = socket(server, 'hwp', blank, { ...p, userId: 'alice' })
      await alice.hello('welcome')
      const bob = socket(server, 'hwp', blank, { ...p, userId: 'bob' })
      await bob.hello('welcome')
      const seqs = new Map()
      for (const [i, op] of typing.entries()) {
        const editor = i === 2 || i === 4 ? bob : alice
        const seq = (seqs.get(editor) ?? 0) + 1
        seqs.set(editor, seq)
        const submit = { t: 'submit', seq, base: i, ops: [op] }
        assert.deepEqual(await editor.reply(submit, 'ack'), { t: 'ack', seq, v: i + 1 })
      }
      const text = (version) =>
        server
          .exportHistory(p, { epoch: 'upload', version })
          .then(({ bytes }) =>
            reopened(bytes, (doc) => [doc.getTextRange(0, 0, 0, 2), doc.getParagraphLength(0, 1)]),
          )
      assert.deepEqual(await text(5), ['가나', 1])
      const undo = server.undoHistory(
        { ...p, userId: 'bob' },
        {
          requestId: 'undo-da',
          expected: { epoch: 'upload', version: 5 },
          target: { epoch: 'upload', v: 5, index: 0 },
        },
      )
      if (exact) {
        assert.deepEqual(await undo, { kind: 'undone', version: 6 })
        assert.deepEqual(await text(6), ['가나', 0])
      } else {
        // Undo appends a change the server made; only a store that records it verbatim may.
        await assert.rejects(undo, /exact history writes/)
        assert.equal((await store.load(p.documentId)).head, 5)
      }
      assert.equal(alice.reason(), undefined)
      assert.equal(bob.reason(), undefined)
    } finally {
      await server.close()
    }
  })
}

test('bytes: an editor joining an epoch the renderer cannot replay is refused at join with the reason', async () => {
  const store = memoryHistoryStore(() => true)
  const opaque = await readFile(
    new URL('../../../apps/hwp/tests/fixtures/collab-opaque-record.hwp', import.meta.url),
  )
  const plain = createCollabServer({ store })
  const seeder = socket(plain, 'hwp', opaque)
  await seeder.hello()
  await seeder.reply(opaque, 'welcome')
  await plain.close()
  const server = createHistoryServer({
    store,
    render: (base, request) => renderBytes(base, Buffer.from(JSON.stringify(request))),
  })
  try {
    const refusal = await socket(server, 'hwp', opaque).hello('error')
    assert.equal(refusal.code, 'schema_mismatch')
    assert.match(refusal.reason, /existing session cannot be replayed: the renderer refused it/)
  } finally {
    await server.close()
  }
})

// Hancom Office files: its version, print settings, compatibility options, metadata and numbering
// or bullet definitions are copied by every save and must come back from history unchanged.
for (const name of ['title-table-zone-fill', 'para-float-tail-table']) {
  test(`bytes: Hancom ${name}.hwpx co-edits and its history export keeps every copied part`, async () => {
    const source = await readFile(
      new URL(`../../../apps/hwp/tests/fixtures/${name}.hwpx`, import.meta.url),
    )
    const copied = (bytes) => {
      const parts = unzipSync(bytes)
      const head = strFromU8(parts['Contents/header.xml'])
      return [
        strFromU8(parts['version.xml']),
        strFromU8(parts['settings.xml']),
        /<hh:numberings[\s\S]*?<\/hh:numberings>/.exec(head)?.[0],
        /<hh:bullets[\s\S]*?<\/hh:bullets>/.exec(head)?.[0],
        head.slice(head.indexOf('</hh:refList>')),
        /<opf:metadata[\s\S]*?<\/opf:metadata>/.exec(strFromU8(parts['Contents/content.hpf']))?.[0],
      ]
    }
    assert.match(copied(source)[0], /application="Hancom Office Hangul"/)
    const store = memoryHistoryStore(
      () => true,
      () => true,
    )
    const server = createHistoryServer({
      store,
      render: (base, request) => renderBytes(base, Buffer.from(JSON.stringify(request))),
    })
    try {
      const editor = socket(server, 'hwpx', source)
      await editor.hello()
      const { epoch } = await editor.reply(source, 'welcome')
      const paragraph = (headType) => ({
        kind: 'command',
        action: 'paragraph',
        at: at(0, 0),
        props: { headType, paraLevel: 1 },
      })
      // Typing, then paragraph heads that refer to the numbering and bullet definitions by id.
      const ops = [{ ...typing[0], text: '공' }, paragraph('Number'), paragraph('Bullet')]
      for (const [i, op] of ops.entries()) {
        const submit = { t: 'submit', seq: i + 1, base: i, ops: [op] }
        assert.deepEqual(await editor.reply(submit, 'ack'), { t: 'ack', seq: i + 1, v: i + 1 })
      }
      const file = await server.exportHistory(p, { epoch, version: 3 })
      assert.equal(
        reopened(file.bytes, (doc) => doc.getTextRange(0, 0, 0, 1)),
        '공',
      )
      // The heads were applied: the first paragraph now uses a new paragraph shape.
      const head = (bytes) =>
        /<hp:p [^>]*paraPrIDRef="(\d+)"/.exec(
          strFromU8(unzipSync(bytes)['Contents/section0.xml']),
        )[1]
      assert.notEqual(head(file.bytes), head(source))
      assert.deepEqual(copied(file.bytes), copied(source))
    } finally {
      await server.close()
    }
  })
}
