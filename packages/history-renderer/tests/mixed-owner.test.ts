import assert from 'node:assert/strict'
import { DatabaseSync } from 'node:sqlite'
import { buffer } from 'node:stream/consumers'
import type {
  Change,
  HistoryByteSource,
  Json,
  Principal,
  StreamHistoryServer,
  StreamHistoryStore,
  UndoTargetPolicy,
} from '@seuoffice/collab/server'
import JSZip from 'jszip'
import { PDFDocument, PDFName } from 'pdf-lib'
import { it } from 'vitest'
import { hash, mixedDocuments } from './mixed-owner-fixtures'

const epoch = 'retained'
const point = (version: number) => ({ epoch, version })
const fork = { requestId: 'fork', expected: point(2), source: point(1) }
const undo = { requestId: 'undo', expected: point(2), target: { epoch, v: 1, index: 0 } }
const editor = 'writer-account'
const curator = 'auth:curator-é'
const principal = (documentId: string, userId = curator): Principal => ({
  documentId,
  userId,
  name: 'Display name',
  canEdit: true,
})
const frame = (v: number, author: string, op: Json): Change => ({
  v,
  seq: 1,
  client: `editor-${v}`,
  author,
  ops: [op],
})
function retained(change: Change) {
  const { v, seq, client, author, ops } = change
  return { v, seq, client, author, opCount: ops.length, bytes: Buffer.from(JSON.stringify(change)) }
}

async function artifactBytes(
  artifact: HistoryByteSource & { fileName: string; dispose?(): Promise<void> },
  extension: string,
) {
  try {
    assert.equal(artifact.fileName, `history.${extension}`)
    const bytes = await buffer(artifact.open())
    assert(bytes.length > 0)
    assert.equal(bytes.length, artifact.byteLength)
    return bytes
  } finally {
    await artifact.dispose?.()
  }
}

async function duplicate<T>(run: () => Promise<T>) {
  const [first, second] = await Promise.all([run(), run()])
  assert.deepEqual(second, first)
  assert.deepEqual(await run(), first)
  return first
}

it('one authenticated owner handles mixed documents and failed commands', async () => {
  const { docx, xlsx, pptx, pdf, small, large } = await mixedDocuments()
  type Doc = typeof docx
  // A static relative import would bundle the example instead of loading the consumer's package.
  const renderer = import.meta.resolve('@seuoffice/history-renderer')
  assert(new URL(renderer).pathname.endsWith('/dist/index.js'))
  const { createStreamHost } = await import(new URL('../examples/stream-host.mjs', renderer).href)
  const db = new DatabaseSync(':memory:')
  const grants = new Set<string>()
  const longCurator = 'é'.repeat(1024)
  const allow = (p: Principal) =>
    grants.has(p.documentId) && [curator, longCurator, 'peer-account'].includes(p.userId)
  const allowTarget: UndoTargetPolicy = (p, target) =>
    p.userId !== 'peer-account' && target.author === editor
  const host = createStreamHost(db, allow, allowTarget)
  const server: StreamHistoryServer = host.server
  const store: StreamHistoryStore = host.store
  function seed(
    doc: Doc,
    frames = [frame(1, editor, doc.alice), frame(2, 'peer-account', doc.independent)].map(retained),
    baseHash = hash(doc.bytes),
  ) {
    grants.add(doc.id)
    assert.equal(
      host.seed(doc.id, {
        id: epoch,
        format: { name: doc.name, schema: doc.schema },
        baseHash,
        baseChunks: [doc.bytes],
        changes: frames,
      }),
      'created',
    )
  }
  const totals = db.prepare(`SELECT
    (SELECT count(*) FROM objects WHERE kind='revision') AS revisions,
    (SELECT count(*) FROM objects WHERE kind='base') AS bases,
    (SELECT count(*) FROM objects WHERE kind IN ('private','export')) AS transient,
    (SELECT count(*) FROM chunks) AS chunks, (SELECT count(*) FROM frames) AS frames,
    (SELECT count(*) FROM receipts) AS receipts, (SELECT count(*) FROM undone) AS undone`)
  const state = () => ({
    ...totals.get(),
    epochs: db
      .prepare('SELECT doc,epoch,head,saved,base_hash FROM epochs ORDER BY doc,epoch')
      .all(),
  })
  async function rejected(run: () => Promise<unknown>, error: RegExp | { code: string }) {
    const before = state()
    await assert.rejects(run, error)
    assert.deepEqual(state(), before)
  }
  try {
    const documents = [docx, large, xlsx, pdf, pptx, small]
    for (const doc of documents) seed(doc)
    const revisions = new Map<string, string>()
    async function restoreDoc(doc: Doc) {
      const p = principal(doc.id)
      const run = () => server.restoreHistory(p, fork)
      const result = doc === docx ? await duplicate(run) : await run()
      assert.equal(result.kind, 'restored')
      revisions.set(doc.id, result.revisionId)
      const bytes = await artifactBytes(host.readRevision(p, result.revisionId), doc.name)
      assert.deepEqual(await doc.reopen(bytes), doc.atAlice, doc.id)
      assert.deepEqual(await store.load(doc.id), {
        id: epoch,
        format: { name: doc.name, schema: doc.schema },
        baseHash: hash(doc.bytes),
        head: 2,
        saved: 0,
      })
    }
    async function undoDoc(doc: Doc) {
      const p = principal(doc.id)
      const run = () => server.undoHistory(p, undo)
      const result = doc === small ? await duplicate(run) : await run()
      assert.deepEqual(result, { kind: 'undone', version: 3 })
      const changes = await store.read(doc.id, epoch, 0, 4)
      assert.deepEqual(changes.slice(0, 2), [
        frame(1, editor, doc.alice),
        frame(2, 'peer-account', doc.independent),
      ])
      assert.equal(changes.length, 3)
      const { v, client, seq, author, ops } = changes[2]!
      assert.match(client, /^history:[\da-f]{8}(?:-[\da-f]{4}){3}-[\da-f]{12}$/)
      assert.deepEqual([v, seq, author], [3, 1, p.userId])
      assert(ops.length > 0)
      const bytes = await artifactBytes(await server.exportHistory(p, point(3)), doc.name)
      assert.deepEqual(await doc.reopen(bytes), doc.afterUndo, doc.id)
    }
    for (let i = 0; i < documents.length; i += 2) {
      await Promise.all([restoreDoc(documents[i]!), restoreDoc(documents[i + 1]!)])
    }
    assert.equal(new Set(revisions.values()).size, documents.length)
    const chunksAfterRestore = totals.get()!.chunks
    for (let i = documents.length - 2; i >= 0; i -= 2) {
      await Promise.all([undoDoc(documents[i]!), undoDoc(documents[i + 1]!)])
    }
    assert.deepEqual(
      { ...totals.get() },
      {
        bases: 6,
        transient: 0,
        revisions: 6,
        receipts: 12,
        undone: 6,
        frames: 18,
        chunks: chunksAfterRestore,
      },
    )
    const p = principal(docx.id)
    await rejected(() => server.restoreHistory(p, { ...fork, requestId: 'old head' }), /stale/i)
    await rejected(() => server.restoreHistory(p, { ...fork, source: point(0) }), /conflicting/i)
    await rejected(
      () => server.exportHistory(principal(pdf.id, 'no-grant'), point(1)),
      /forbidden/i,
    )
    await rejected(() => server.undoHistory(principal(small.id, 'peer-account'), undo), /target/i)
    assert.throws(() => host.readRevision(p, revisions.get(large.id)!), /unavailable/i)
    const prior = state()
    const saved = await artifactBytes(host.readRevision(p, revisions.get(docx.id)!), docx.name)
    assert.deepEqual(await docx.reopen(saved), docx.atAlice)
    assert.deepEqual(await server.restoreHistory(p, fork), {
      kind: 'restored',
      revisionId: revisions.get(docx.id),
    })
    assert.deepEqual(state(), prior)

    const overlap = { ...xlsx, id: 'overlap' }
    seed(overlap, [
      retained(frame(1, editor, xlsx.alice)),
      retained(frame(2, 'peer-account', xlsx.dependent)),
    ])
    await rejected(() => server.undoHistory(principal(overlap.id), undo), { code: 'conflict' })

    const wrongHash = { ...small, id: 'wrong-hash' }
    seed(wrongHash, [], hash(pdf.bytes))
    await rejected(() => server.exportHistory(principal(wrongHash.id), point(0)), {
      code: 'invalid_history',
    })
    const gap = { ...pptx, id: 'frame-gap' }
    seed(gap, [{ ...retained(frame(2, editor, pptx.alice)), v: 1 }])
    await rejected(
      () => server.restoreHistory(principal(gap.id), { ...fork, expected: point(1) }),
      { code: 'invalid_history' },
    )

    const zip = await JSZip.loadAsync(docx.bytes)
    for (const part of ['[Content_Types].xml', '_rels/.rels', 'word/document.xml']) {
      zip.file(part, (await zip.file(part)!.async('string')) + ' '.repeat(704 * 1024))
    }
    const oversized = {
      ...docx,
      id: 'oversized',
      bytes: await zip.generateAsync({ type: 'uint8array', compression: 'STORE' }),
    }
    assert(oversized.bytes.length > 2 * 1024 * 1024)
    seed(oversized, [])
    await rejected(() => server.exportHistory(principal(oversized.id), point(0)), {
      code: 'unsupported',
    })
    const drawing = await PDFDocument.create()
    drawing.addPage().drawEllipse({ x: 100, y: 120, xScale: 8, yScale: 12 })
    const unsupported = { ...pdf, id: 'ellipse', bytes: await drawing.save() }
    // Version 0 is the base itself; the PDF profile refuses the first edit.
    seed(unsupported, [retained(frame(1, editor, pdf.alice))])
    await rejected(
      () =>
        server.restoreHistory(principal(unsupported.id), {
          ...fork,
          expected: point(1),
          source: point(1),
        }),
      { code: 'unsupported' },
    )
    const screen = drawing.addPage([512, 384])
    const rows: string[] = []
    for (let y = 0; y < 384; y++) {
      const squares: string[] = []
      for (let x = y % 2; x < 512; x += 2) squares.push(`${x} ${y} 1 1 re f`)
      rows.push(squares.join('\n'))
    }
    screen.node.set(
      PDFName.Contents,
      drawing.context.register(drawing.context.stream(rows.join('\n'))),
    )
    const checkerboard = {
      ...pdf,
      id: 'vector-screen',
      bytes: await drawing.save({ useObjectStreams: false }),
    }
    assert(checkerboard.bytes.length > 1024 * 1024 && checkerboard.bytes.length < 2 * 1024 * 1024)
    assert.equal((await PDFDocument.load(checkerboard.bytes)).getPageCount(), 2)
    seed(checkerboard, [])
    await rejected(() => server.exportHistory(principal(checkerboard.id), point(0)), {
      code: 'unsupported',
    })

    // The prefix fits the legacy 256 KiB text budget. Only the authenticated long author's appended frame exceeds it.
    const tight = { ...pdf, id: 'compensation-budget' }
    seed(tight, [
      retained(frame(1, editor, pdf.alice)),
      retained(frame(2, 'b'.repeat(255 * 1024), pdf.independent)),
    ])
    await rejected(() => server.undoHistory(principal(tight.id, longCurator), undo), {
      code: 'unsupported',
    })
    const shortAuthor = principal(tight.id)
    assert.deepEqual(await server.undoHistory(shortAuthor, undo), { kind: 'undone', version: 3 })
    const bytes = await artifactBytes(await server.exportHistory(shortAuthor, point(3)), pdf.name)
    assert.deepEqual(await pdf.reopen(bytes), pdf.afterUndo)
  } finally {
    await server.close()
    db.close()
  }
}, 120_000)
