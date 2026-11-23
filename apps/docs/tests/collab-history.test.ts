import { HistoryError, type CollabAdapter, type Json } from '@seuoffice/collab/client'
import { parseDocx, readSections } from '@seuoffice/docx-engine'
import { Editor } from '@tiptap/core'
import { afterEach, beforeAll, describe, expect, it } from 'vitest'
import { buildDocx } from '../../../packages/docx-engine/tests/helpers/build-docx'
import { blocksToPmDoc, pmDocOptions } from '../src/renderer/editor/convert'
import { editorExtensions } from '../src/renderer/editor/extensions'
import type { DocsOp } from '../src/web/collab-adapter'
import { openDocsCollab } from '../src/web/collab-side'
import { fakeApp } from './helpers/collab-app'

const path = '/web/memo.docx'
const untouched = '<w:p w:rsidR="00112233"><w:r><w:t>next</w:t></w:r></w:p>'
let base: Uint8Array
const close: (() => void)[] = []

beforeAll(async () => {
  base = await buildDocx({ bodyXml: '<w:p><w:r><w:t>base</w:t></w:r></w:p>' + untouched })
})
afterEach(() => {
  for (const dispose of close.splice(0)) dispose()
})

interface Page {
  editor: Editor
  adapter: CollabAdapter<DocsOp>
  sent: DocsOp[]
}

async function page(bytes: Uint8Array): Promise<Page> {
  const parsed = await parseDocx(bytes)
  const sections = readSections(parsed)
  const editor = new Editor({
    extensions: editorExtensions,
    content: blocksToPmDoc(parsed.blocks, sections, pmDocOptions(parsed)),
  })
  const { context } = fakeApp({ editor, sections, doc: { filePath: path, format: 'docx', parsed } })
  const adapter = await openDocsCollab(
    { editor, context },
    { path, fileName: 'memo.docx', finish: async (plain) => plain },
  )
  const sent: DocsOp[] = []
  const off = adapter.listen((ops) => sent.push(...ops))
  close.push(() => {
    off()
    editor.destroy()
  })
  return { editor, adapter, sent }
}

function confirmed(page: Page): Json[] {
  const ops = page.sent.splice(0)
  page.adapter.confirmed?.(ops)
  return ops.map((op) => page.adapter.encode(op))
}

function replay(page: Page, wire: Json[]): void {
  page.adapter.integrate(
    wire.map((op) => page.adapter.decode(op)),
    [],
  )
}

async function expectFile(page: Page, first: string): Promise<void> {
  const file = await page.adapter.produceFile()
  expect(file.fileName).toBe('memo.docx')
  const parsed = await parseDocx(file.bytes)
  expect(parsed.blocks[1].originalXml).toBe(untouched)
  const reopened = page.editor.schema.nodeFromJSON(
    blocksToPmDoc(parsed.blocks, readSections(parsed), pmDocOptions(parsed)),
  )
  expect(reopened.child(0).textContent).toBe(first)
  expect(reopened.child(1).textContent).toBe('next')
}

describe('DOCX history in an editor opened from the epoch file', () => {
  it('reopens Alice’s version, then takes back her edit at head without removing Bob’s', async () => {
    const alice = await page(base)
    const bob = await page(base)
    const detached = await page(base)
    alice.editor.view.dispatch(alice.editor.state.tr.insertText('AAA', 3, 5))
    const first = confirmed(alice)
    expect(first).toHaveLength(1)
    replay(bob, first)
    bob.editor.view.dispatch(bob.editor.state.tr.insertText('B', 1))
    const second = confirmed(bob)
    replay(alice, second)
    const plan = detached.adapter.captureUndo!(detached.adapter.decode(first[0]))
    replay(detached, first)
    await expectFile(detached, 'baAAA')
    replay(detached, second)
    expect(plan([])).toBeNull()
    const undo = plan(second.map((op) => detached.adapter.decode(op)))
    expect(undo).not.toBeNull()
    await expectFile(detached, 'BbaAAA')
    const wire = undo!.map((op) => detached.adapter.encode(op))
    replay(alice, wire)
    replay(bob, wire)
    await expectFile(alice, 'Bbase')
    await expectFile(bob, 'Bbase')
    expect(detached.sent).toEqual([])
  })

  it('refuses even if Bob edits inside Alice’s insertion and later restores the same letters', async () => {
    const alice = await page(base)
    const bob = await page(base)
    const detached = await page(base)
    alice.editor.view.dispatch(alice.editor.state.tr.insertText('AAA', 3))
    const first = confirmed(alice)
    const plan = detached.adapter.captureUndo!(detached.adapter.decode(first[0]))
    replay(detached, first)
    replay(bob, first)
    bob.editor.view.dispatch(bob.editor.state.tr.insertText('X', 4, 5))
    bob.editor.view.dispatch(bob.editor.state.tr.insertText('A', 4, 5))
    const later = confirmed(bob)
    replay(detached, later)
    expect(plan(later.map((op) => detached.adapter.decode(op)))).toBeNull()
    await expectFile(detached, 'baAAAse')
  })

  it('maps both inverse steps of a compound replacement across a preceding peer insertion', async () => {
    const alice = await page(base)
    const bob = await page(base)
    const detached = await page(base)
    alice.editor.view.dispatch(alice.editor.state.tr.insertText('XY', 3).insertText('Z', 4, 5))
    const first = confirmed(alice)
    const plan = detached.adapter.captureUndo!(detached.adapter.decode(first[0]))
    replay(detached, first)
    replay(bob, first)
    bob.editor.view.dispatch(bob.editor.state.tr.insertText('B', 1))
    const later = confirmed(bob)
    replay(detached, later)
    const undo = plan(later.map((op) => detached.adapter.decode(op)))
    expect(undo).not.toBeNull()
    detached.adapter.integrate([...undo!], [])
    await expectFile(detached, 'Bbase')
  })

  it('keeps a peer’s formatting inside the selected text even though marks do not move positions', async () => {
    const alice = await page(base)
    const bob = await page(base)
    const detached = await page(base)
    alice.editor.view.dispatch(alice.editor.state.tr.insertText('AAA', 3))
    const first = confirmed(alice)
    const plan = detached.adapter.captureUndo!(detached.adapter.decode(first[0]))
    replay(detached, first)
    replay(bob, first)
    bob.editor.view.dispatch(
      bob.editor.state.tr.addMark(4, 5, bob.editor.schema.marks.bold.create()),
    )
    const later = confirmed(bob)
    replay(detached, later)
    expect(plan(later.map((op) => detached.adapter.decode(op)))).toBeNull()
    expect(detached.editor.state.doc.rangeHasMark(4, 5, detached.editor.schema.marks.bold)).toBe(
      true,
    )
  })

  it('does not invent a text inverse for a side entry or a paragraph split', async () => {
    const alice = await page(base)
    const detached = await page(base)
    expect(() =>
      detached.adapter.captureUndo!({ kind: 'side', key: 'footer', value: null }),
    ).toThrow(HistoryError)
    alice.editor.view.dispatch(alice.editor.state.tr.split(3))
    const [split] = confirmed(alice)
    expect(() => detached.adapter.captureUndo!(detached.adapter.decode(split))).toThrow(
      HistoryError,
    )
    await expectFile(detached, 'base')
  })
})
