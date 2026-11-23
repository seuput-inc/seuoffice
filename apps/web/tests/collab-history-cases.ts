import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import type { Json } from '@seuoffice/collab/client'
import { parseDocx } from '@seuoffice/docx-engine'
import { createBlankPptx, getSlideNotes, openPptx } from '@seuoffice/pptx-engine'
import { readBasicWorkbook } from '@seuoffice/xlsx-gateway/gateway/xlsx-gateway'
import { PDFDocument } from 'pdf-lib'
import type * as HistoryServer from '../../../packages/collab/src/server/index'
import { buildDocx } from '../../../packages/docx-engine/tests/helpers/build-docx'
import { buildCompatibilityFixture } from '../../sheets/tests/fixture-builder'

type Render = Parameters<typeof HistoryServer.createHistoryServer>[0]['render']

interface Example {
  name: string
  schema: number
  base(): Promise<Uint8Array>
  alice: Json
  independent: Json
  dependent: Json
  unsupportedUndo: Json
  reopen(bytes: Uint8Array): Promise<unknown>
  atAlice: unknown
  afterUndo: unknown
}

const insert = (at: number, text: string): Json => ({
  kind: 'steps',
  steps: [{ stepType: 'replace', from: at, to: at, slice: { content: [{ type: 'text', text }] } }],
})
const cell = (id: string, col: number, value: string | number): Json => ({
  id,
  m: [
    [
      'sheet.mutation.set-range-values',
      { subUnitId: 'sheet-1', cellValue: { 0: { [col]: { v: value } } } },
    ],
  ],
})
const notes = (slide: string): Json => ({ id: 'b', edit: { kind: 'notes', slide, text: 'Bob' } })
const title = (id: string, value: string): Json => ({
  id,
  puts: [{ b: 'metadata', v: { title: value } }],
})

export const examples: Example[] = [
  {
    name: 'docx',
    schema: 4,
    base: () =>
      buildDocx({
        bodyXml: '<w:p><w:r><w:t>base</w:t></w:r></w:p><w:p><w:r><w:t>next</w:t></w:r></w:p>',
      }),
    alice: insert(3, 'AAA'),
    independent: insert(1, 'B'),
    dependent: insert(4, 'B'),
    unsupportedUndo: { kind: 'steps', steps: [{ stepType: 'replace', from: 1, to: 1 }] },
    reopen: async (bytes) =>
      (await parseDocx(bytes)).blocks.flatMap((block) =>
        block.runs ? [block.runs.map((run) => run.text).join('')] : [],
      ),
    atAlice: ['baAAAse', 'next'],
    afterUndo: ['Bbase', 'next'],
  },
  {
    name: 'xlsx',
    schema: 3,
    base: buildCompatibilityFixture,
    alice: cell('a', 0, 'Alice'),
    independent: cell('b', 1, 22),
    dependent: cell('b', 0, 'Bob'),
    unsupportedUndo: {
      id: 'formula',
      m: [
        [
          'sheet.mutation.set-range-values',
          { subUnitId: 'sheet-1', cellValue: { 0: { 0: { f: '=B1' } } } },
        ],
      ],
    },
    async reopen(bytes) {
      const { cells } = (await readBasicWorkbook(Buffer.from(bytes))).snapshot.sheets[0]!
      return [cells.A1?.value, cells.B1?.value]
    },
    atAlice: ['Alice', 10],
    afterUndo: ['Old', 22],
  },
  {
    name: 'pptx',
    schema: 3,
    base: createBlankPptx,
    alice: { id: 'a', edit: { kind: 'blankSlide', slide: 's_1', before: false, made: 's_2' } },
    independent: notes('s_1'),
    dependent: notes('s_2'),
    unsupportedUndo: notes('s_1'),
    async reopen(bytes) {
      const { deck, archive } = await openPptx(bytes)
      return deck.slides.map((slide) => getSlideNotes(archive, slide.path))
    },
    atAlice: ['', ''],
    afterUndo: ['Bob'],
  },
  {
    name: 'pdf',
    schema: 2,
    async base() {
      const pdf = await PDFDocument.create()
      pdf.addPage()
      pdf.setTitle('Base')
      return pdf.save()
    },
    alice: title('a', 'Alice'),
    independent: { id: 'b', puts: [{ b: 'rotations', k: 0, was: 0, v: 90 }] },
    dependent: title('b', 'Bob'),
    unsupportedUndo: { id: 'order', puts: [{ b: 'order', was: [0], v: [0] }] },
    async reopen(bytes) {
      const pdf = await PDFDocument.load(bytes)
      return [pdf.getTitle(), pdf.getPage(0).getRotation().angle]
    },
    atAlice: ['Alice', 0],
    afterUndo: ['Base', 90],
  },
]

export async function verifyHistory(
  {
    createHistoryServer,
    memoryHistoryStore,
  }: Pick<typeof HistoryServer, 'createHistoryServer' | 'memoryHistoryStore'>,
  example: Example,
  render: Render,
): Promise<void> {
  const bytes = await example.base()
  const baseHash = createHash('sha256').update(bytes).digest('hex')
  const epoch = 'synthetic-epoch'
  const format = { name: example.name, schema: example.schema }
  const expected = { epoch, version: 2 }
  const undo = { expected, target: { epoch, v: 1, index: 0 }, requestId: 'undo' }
  const restore = { expected, source: { epoch, version: 1 }, requestId: 'fork' }
  for (const mode of ['independent', 'dependent', 'unsupported'] as const) {
    const documentId = example.name
    const alice = { documentId, userId: 'alice', name: 'Alice', canEdit: true }
    const store = memoryHistoryStore((p) => p.documentId === documentId && p.userId === 'alice')
    const server = createHistoryServer({ store, render })
    try {
      await store.create(documentId, { id: epoch, base: bytes, format, baseHash })
      for (const [base, author, op] of [
        [0, 'alice', mode === 'unsupported' ? example.unsupportedUndo : example.alice],
        [1, 'bob', mode === 'dependent' ? example.dependent : example.independent],
      ] as const) {
        assert.deepEqual(
          await store.append(documentId, epoch, {
            base,
            author,
            client: author,
            seq: 1,
            ops: [op],
          }),
          { kind: 'appended', v: base + 1 },
        )
      }
      if (mode !== 'independent') {
        await assert.rejects(
          server.undoHistory(alice, undo),
          mode === 'dependent' ? /conflict/ : /unsupported/,
        )
        assert.equal((await store.read(documentId, epoch, 0, 3)).length, 2)
        continue
      }
      await assert.rejects(
        server.restoreHistory(alice, { ...restore, source: { epoch: 'missing', version: 1 } }),
        /unavailable/,
      )
      const { revisionId } = await server.restoreHistory(alice, restore)
      const fork = await store.readRevision(alice, revisionId)
      assert.deepEqual(await example.reopen(fork.bytes), example.atAlice)
      const state = await store.load(documentId)
      assert.deepEqual([state?.id, state?.head], [epoch, 2])
      assert.deepEqual(await server.undoHistory(alice, undo), { kind: 'undone', version: 3 })
      const head = await server.exportHistory(alice, { epoch, version: 3 })
      assert.deepEqual(await example.reopen(head.bytes), example.afterUndo)
      assert.deepEqual((await store.readRevision(alice, revisionId)).bytes, fork.bytes)
    } finally {
      await server.close()
    }
  }
}
