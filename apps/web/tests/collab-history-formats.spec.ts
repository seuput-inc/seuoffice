import { createHash } from 'node:crypto'
import { expect, test } from '@playwright/test'
import type { Json } from '@seuoffice/collab/client'
import { parseDocx } from '@seuoffice/docx-engine'
import { createBlankPptx, getSlideNotes, openPptx } from '@seuoffice/pptx-engine'
import { readBasicWorkbook } from '@seuoffice/xlsx-gateway/gateway/xlsx-gateway'
import { PDFDocument } from 'pdf-lib'
import { createHistoryServer, memoryHistoryStore } from '../../../packages/collab/src/server/index'
import { buildDocx } from '../../../packages/docx-engine/tests/helpers/build-docx'
import { buildCompatibilityFixture } from '../../sheets/tests/fixture-builder'
import {
  BUNDLE_HOST,
  buildEmbedScript,
  HOST_NAME,
  type HostWindow,
  origin,
  serveHostAndBundle,
} from './cross-origin-embed'

type Render = Parameters<typeof createHistoryServer>[0]['render']

interface Example {
  name: string
  schema: number
  base(): Promise<Uint8Array>
  alice: Json
  independent: Json
  dependent: Json
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

const examples: Example[] = [
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
    async reopen(bytes) {
      const pdf = await PDFDocument.load(bytes)
      return [pdf.getTitle(), pdf.getPage(0).getRotation().angle]
    },
    atAlice: ['Alice', 0],
    afterUndo: ['Base', 90],
  },
]

async function verifyHistory(example: Example, render: Render): Promise<void> {
  const bytes = await example.base()
  const baseHash = createHash('sha256').update(bytes).digest('hex')
  const epoch = 'synthetic-epoch'
  const format = { name: example.name, schema: example.schema }
  const expected = { epoch, version: 2 }
  const undo = { expected, target: { epoch, v: 1, index: 0 }, requestId: 'undo' }
  const restore = { expected, source: { epoch, version: 1 }, requestId: 'fork' }
  for (const dependent of [false, true]) {
    const documentId = example.name
    const alice = { documentId, userId: 'alice', name: 'Alice', canEdit: true }
    const store = memoryHistoryStore((p) => p.documentId === documentId && p.userId === 'alice')
    const server = createHistoryServer({ store, render })
    try {
      await store.create(documentId, { id: epoch, base: bytes, format, baseHash })
      for (const [base, author, op] of [
        [0, 'alice', example.alice],
        [1, 'bob', dependent ? example.dependent : example.independent],
      ] as const) {
        expect(
          await store.append(documentId, epoch, {
            base,
            author,
            client: author,
            seq: 1,
            ops: [op],
          }),
        ).toEqual({ kind: 'appended', v: base + 1 })
      }
      if (dependent) {
        await expect(server.undoHistory(alice, undo)).rejects.toThrow('conflict')
        expect(await store.read(documentId, epoch, 0, 3)).toHaveLength(2)
        continue
      }
      await expect(
        server.restoreHistory(alice, { ...restore, source: { epoch: 'missing', version: 1 } }),
      ).rejects.toThrow(/unavailable/)
      const { revisionId } = await server.restoreHistory(alice, restore)
      const fork = await store.readRevision(alice, revisionId)
      expect(await example.reopen(fork.bytes)).toEqual(example.atAlice)
      expect(await store.load(documentId)).toMatchObject({ id: epoch, head: 2 })
      expect(await server.undoHistory(alice, undo)).toEqual({ kind: 'undone', version: 3 })
      const head = await server.exportHistory(alice, { epoch, version: 3 })
      expect(await example.reopen(head.bytes)).toEqual(example.afterUndo)
      expect((await store.readRevision(alice, revisionId)).bytes).toEqual(fork.bytes)
    } finally {
      await server.close()
    }
  }
}

test.beforeAll(buildEmbedScript)
for (const example of examples) {
  test(`${example.name} history reopens at a version and compensates only an independent edit`, async ({
    page,
  }) => {
    test.setTimeout(180_000)
    const { host, bundle } = await serveHostAndBundle()
    try {
      await page.goto(`${origin(host, HOST_NAME)}/host.html`)
      await verifyHistory(example, async (bytes, history) => {
        const result = await page.evaluate(
          async ({ baseUrl, fileName, bytes, history }) => {
            try {
              const sdk = (window as unknown as HostWindow).SeuofficeEmbed
              const {
                fileName: name,
                bytes: file,
                compensation,
              } = await sdk.renderHistory(document.getElementById('viewer')!, {
                baseUrl,
                fileName,
                bytes: new Uint8Array(bytes),
                history: JSON.parse(history),
              })
              return { ok: true, fileName: name, bytes: [...file], compensation } as const
            } catch (error) {
              if (!(error instanceof Error) || !('code' in error)) throw error
              return { ok: false, code: error.code } as const
            }
          },
          {
            baseUrl: `${origin(bundle, BUNDLE_HOST)}/`,
            fileName: `example.${example.name}`,
            bytes: [...bytes],
            history: JSON.stringify(history),
          },
        )
        if (!result.ok) throw new Error(String(result.code))
        return {
          fileName: result.fileName,
          bytes: new Uint8Array(result.bytes),
          compensation: result.compensation,
        }
      })
    } finally {
      await Promise.all([host.close(), bundle.close()])
    }
  })
}
