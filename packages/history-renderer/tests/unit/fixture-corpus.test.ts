/**
 * Every DOCX, XLSX, PPTX and PDF in the repository (and, with GENERATED_FIXTURES, the files
 * tests/generate-basic-documents.py makes) at version 0, then with one edit in the wire form its
 * editor sends and a selective undo of that edit under a peer's later one. Version 0 is the base
 * itself. An edit is either replayed with every part it does not touch kept byte for byte, or
 * refused as unsupported with the renderer's reason; files from Word, LibreOffice, PowerPoint,
 * python-docx, openpyxl, python-pptx and pypdf must take it.
 */
import '@seuoffice/engine-wasm/node'
import { execFileSync } from 'node:child_process'
import { createHash } from 'node:crypto'
import { readdirSync, readFileSync } from 'node:fs'
import { join, relative } from 'node:path'
import { HistoryError, type HistoryRequest, type Json } from '@seuoffice/collab/client'
import { getSlideNotes, openPptx, slideDurableId } from '@seuoffice/pptx-engine'
import { readBasicWorkbook } from '@seuoffice/xlsx-gateway/gateway/xlsx-gateway'
import JSZip from 'jszip'
import { PDFDocument } from 'pdf-lib'
import { describe, expect, it } from 'vitest'
import { applyOp } from '../../../../apps/slides/src/web/collab-ops'
import { editorCellOp, typedCell } from '../../../../apps/sheets/tests/collab/editor-wire'
import { render } from '../../src/replay'
import { editorBlocks, editorText, expectEditorPositions } from './docs-editor'

const root = join(import.meta.dirname, '../../../..')
const tracked = execFileSync('git', ['ls-files', '*.docx', '*.xlsx', '*.pptx', '*.pdf'], {
  cwd: root,
  encoding: 'utf8',
})
  .split('\n')
  .filter(Boolean)
  .map((path) => join(root, path))
const generatedDir = process.env.GENERATED_FIXTURES
const generated = generatedDir
  ? readdirSync(generatedDir)
      .filter((name) => /^plain\.(?:docx|xlsx|pptx|pdf)$/.test(name))
      .map((name) => join(generatedDir, name))
  : []
// A password-protected Office file is a compound file, not a ZIP: the archive gate refuses it.
const encryptedPackage = /\/encrypted-fixtures\/office-\w+-password\.docx$/
const mustEdit = new Set([
  'apps/docs/tests/encrypted-fixtures/office-agile-plain.docx',
  'apps/docs/tests/encrypted-fixtures/office-standard-plain.docx',
  'fixtures/generated/simple.docx',
  'apps/shell/build/shell-new/blank.xlsx',
  'apps/shell/build/shell-new/blank.pptx',
  'packages/pptx-engine/tests/fixtures/01_standard_business.pptx',
  'packages/pptx-engine/tests/fixtures/05_unicode_cjk_emoji.pptx',
  ...generated.map((path) => relative(root, path)),
])

const schemas: Record<string, number> = { docx: 4, xlsx: 3, pptx: 3, pdf: 2 }

function request(base: Uint8Array, name: string, changes: Json[][], undo = false) {
  const history: HistoryRequest = {
    baseHash: createHash('sha256').update(base).digest('hex'),
    epoch: 'corpus',
    format: { name, schema: schemas[name]! },
    version: changes.length,
    changes: changes.map((ops, i) => ({
      v: i + 1,
      author: `person-${i}`,
      client: `client-${i}`,
      seq: 1,
      ops,
    })),
  }
  return undo ? { ...history, undo: { epoch: 'corpus', v: 1, index: 0 } } : history
}

interface Edit {
  alice: Json
  bob: Json
  /** what the file holds after Alice's edit alone, and after undoing it under Bob's */
  read(bytes: Uint8Array): Promise<unknown>
  atAlice: unknown
  afterUndo: unknown
  /** package parts Alice's edit may rewrite; every other base part stays byte for byte */
  rewrites: readonly string[]
}

const plainMarks = new Set(['bold', 'italic', 'underline', 'strike', 'docTextStyle'])
type MarkJson = { type: string; attrs?: Record<string, unknown> }
// Complex-script runs: the editor resolves them through the cs chain this reader does not model.
const plainMark = ({ type, attrs }: MarkJson) =>
  plainMarks.has(type) && attrs?.csFont == null && attrs?.cs == null

// Paragraphs whose text the editor reads differently from the file, or that the body does not hold
// one to one: the renderer refuses them instead of rewriting them.
const docxRefusals: Record<string, RegExp> = {
  // Symbol-font private-use characters show as the font's glyphs.
  'packages/docx-engine/tests/fixtures/truths/symbol-face-factors/probe.docx': /reads differently/,
  // A paragraph with two w:pStyle and two w:rStyle.
  'packages/docx-engine/tests/fixtures/truths/bidi-mark-cs-size-last-style-ref/probe.docx':
    /does not hold as such/,
}

/**
 * Alice types at the start of the last ordinary paragraph (text, tabs and line breaks in plain
 * character formatting, wherever it is, after whatever the body holds before it), Bob at its end,
 * as the Docs editor does.
 */
async function docxEdit(bytes: Uint8Array): Promise<Edit | undefined> {
  const target = (await editorBlocks(bytes)).blocks
    .reverse()
    .find(
      ({ node }) =>
        !!node.content?.length &&
        node.content.every(
          (child) =>
            (child.type === 'text' || child.type === 'hardBreak') &&
            (child.marks ?? []).every((mark) => plainMark(mark as MarkJson)),
        ),
    )
  if (!target) return undefined
  const paragraph = target.node.content!
  const text = editorText(target.node)
  // Typed text takes the marks of the text it is typed next to.
  const insert = (pos: number, typed: string, marks: Json[] | undefined): Json => ({
    kind: 'steps',
    steps: [
      {
        stepType: 'replace',
        from: pos,
        to: pos,
        slice: { content: [{ type: 'text', text: typed, ...(marks ? { marks } : {}) }] },
      },
    ],
  })
  return {
    alice: insert(target.pos + 1, 'A', paragraph[0]!.marks),
    bob: insert(target.pos + target.size, 'B', paragraph.at(-1)!.marks),
    async read(file) {
      const block = (await editorBlocks(file)).blocks.find(({ path }) => path === target.path)
      return block && editorText(block.node)
    },
    atAlice: 'A' + text,
    afterUndo: text + 'B',
    rewrites: ['word/document.xml'],
  }
}

async function xlsxEdit(bytes: Uint8Array): Promise<Edit> {
  const { snapshot } = await readBasicWorkbook(Buffer.from(bytes))
  const sheet = snapshot.sheets[0]!.id
  return {
    alice: await editorCellOp('alice', sheet, { 0: { 6: typedCell('Alice') } }),
    bob: await editorCellOp('bob', sheet, { 1: { 6: typedCell('Bob') } }),
    async read(file) {
      const { cells } = (await readBasicWorkbook(Buffer.from(file))).snapshot.sheets[0]!
      return [cells.G1?.value ?? null, cells.G2?.value ?? null]
    },
    atAlice: ['Alice', null],
    afterUndo: [null, 'Bob'],
    // The planner asks for a recalculation on load (calcPr) when the workbook has none.
    rewrites: ['xl/worksheets/sheet1.xml', 'xl/workbook.xml'],
  }
}

async function pptxEdit(bytes: Uint8Array): Promise<Edit> {
  const { deck, archive } = await openPptx(bytes)
  const slide = slideDurableId(deck.slides[0]!)
  const count = deck.slides.length
  // The page that makes the slide names it from the op id; peers check that name.
  const blank = { kind: 'blankSlide', slide, before: false, made: '' } as const
  const made = applyOp(await openPptx(bytes), { id: 'alice', edit: blank })!.made!
  return {
    alice: { id: 'alice', edit: { ...blank, made } },
    bob: { id: 'bob', edit: { kind: 'notes', slide, text: 'Bob' } },
    async read(file) {
      const opened = await openPptx(file)
      return [opened.deck.slides.length, getSlideNotes(opened.archive, opened.deck.slides[0]!.path)]
    },
    atAlice: [count + 1, getSlideNotes(archive, deck.slides[0]!.path)],
    afterUndo: [count, 'Bob'],
    rewrites: ['ppt/presentation.xml', 'ppt/_rels/presentation.xml.rels', '[Content_Types].xml'],
  }
}

async function pdfEdit(bytes: Uint8Array): Promise<Edit> {
  const base = await PDFDocument.load(bytes, { updateMetadata: false })
  const turn = base.getPage(0).getRotation().angle
  return {
    alice: { id: 'alice', puts: [{ b: 'metadata', v: { title: 'Alice' } }] },
    bob: { id: 'bob', puts: [{ b: 'rotations', k: 0, was: 0, v: 90 }] },
    async read(file) {
      const pdf = await PDFDocument.load(file, { updateMetadata: false })
      return [pdf.getTitle(), pdf.getPage(0).getRotation().angle]
    },
    atAlice: ['Alice', turn],
    afterUndo: [base.getTitle(), (turn + 90) % 360],
    rewrites: [],
  }
}

const edits: Record<string, (bytes: Uint8Array) => Promise<Edit | undefined>> = {
  docx: docxEdit,
  xlsx: xlsxEdit,
  pptx: pptxEdit,
  pdf: pdfEdit,
}

async function parts(bytes: Uint8Array) {
  const zip = await JSZip.loadAsync(bytes)
  const out = new Map<string, Uint8Array>()
  for (const entry of Object.values(zip.files))
    if (!entry.dir) out.set(entry.name, await entry.async('uint8array'))
  return out
}

/** The replayed DOCX body differs from the base inside one paragraph only. */
function oneParagraph(before: string, after: string) {
  let start = 0
  while (start < before.length && before[start] === after[start]) start++
  let end = 0
  while (
    end < before.length - start &&
    before[before.length - 1 - end] === after[after.length - 1 - end]
  )
    end++
  return !before.slice(start, before.length - end).includes('</w:p>')
}

describe.each([...tracked, ...generated].map((path) => [relative(root, path), path] as const))(
  '%s',
  (name, path) => {
    const bytes = new Uint8Array(readFileSync(path))
    const format = path.split('.').pop()!.toLowerCase()

    it.skipIf(encryptedPackage.test(path))('returns the base itself at version 0', async () => {
      const zero = await render(bytes, request(bytes, format, []))
      expect(Buffer.compare(zero.bytes, bytes)).toBe(0)
    })

    it.runIf(format === 'docx' && !encryptedPackage.test(path))(
      'puts every paragraph where the Docs editor has it',
      () => expectEditorPositions(bytes),
    )

    it.skipIf(encryptedPackage.test(path))(
      'replays and selectively undoes an editor edit, or refuses it as unsupported',
      async () => {
        const edit = await edits[format]!(bytes).catch(() => undefined)
        expect(edit || !mustEdit.has(name)).toBeTruthy()
        if (!edit) return
        let alice
        try {
          alice = await render(bytes, request(bytes, format, [[edit.alice]]))
        } catch (error) {
          expect(error).toBeInstanceOf(HistoryError)
          expect(error).toMatchObject({ code: 'unsupported' })
          // Typing into an ordinary paragraph works whatever else the document holds.
          const message = (error as Error).message
          if (docxRefusals[name]) expect(message).toMatch(docxRefusals[name])
          else expect(mustEdit.has(name) || format === 'docx', message).toBe(false)
          return
        }
        expect(await edit.read(alice.bytes)).toEqual(edit.atAlice)
        if (format !== 'pdf') {
          const [before, after] = [await parts(bytes), await parts(alice.bytes)]
          for (const [part, data] of before) {
            if (!edit.rewrites.includes(part)) expect(after.get(part), part).toEqual(data)
          }
          if (format === 'docx') {
            const decode = (b: Map<string, Uint8Array>) =>
              new TextDecoder().decode(b.get('word/document.xml'))
            expect(oneParagraph(decode(before), decode(after))).toBe(true)
          }
        }
        const changes = [[edit.alice], [edit.bob]]
        const undone = await render(bytes, request(bytes, format, changes, true))
        expect(undone.compensation).toHaveLength(1)
        expect(await edit.read(undone.bytes)).toEqual(edit.afterUndo)
        // A peer that appends the compensation to the log gets the same file.
        const peer = await render(bytes, request(bytes, format, [...changes, undone.compensation!]))
        expect(await edit.read(peer.bytes)).toEqual(edit.afterUndo)
      },
      30_000,
    )
  },
)
