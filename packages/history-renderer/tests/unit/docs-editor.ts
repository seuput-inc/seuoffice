/**
 * The document the Docs editor shows once a DOCX is loaded, and the check that the renderer's
 * model puts every paragraph where that document has it.
 */
import type { Json } from '@seuoffice/collab/client'
import { parseDocx } from '@seuoffice/docx-engine'
import { type Extensions, getSchema } from '@tiptap/core'
import { EditorState } from '@tiptap/pm/state'
import { tableEditing } from '@tiptap/pm/tables'
import type { Node as PmNode } from 'prosemirror-model'
import { expect } from 'vitest'
import { docxModel } from '../../src/docx'

export type PmJson = { type: string; text?: string; content?: PmJson[]; marks?: Json[] }
// The Docs file-to-editor conversion. A static import would type-check the Docs renderer here,
// whose page globals clash with the Slides ones this package already type-checks.
const convert: {
  blocksToPmDoc(blocks: unknown, sections: undefined, options: unknown): unknown
  pmDocOptions(parsed: unknown): unknown
} = await import(
  /* @vite-ignore */ new URL(
    '../../../../apps/docs/src/renderer/editor/convert.ts',
    import.meta.url,
  ).href
)

// The Docs editor's schema, with the table plugin that repairs tables as the document loads.
const { editorExtensions } = await import(
  /* @vite-ignore */ new URL(
    '../../../../apps/docs/src/renderer/editor/extensions.ts',
    import.meta.url,
  ).href
)
const editorSchema = getSchema(editorExtensions as Extensions)
export const editorTextBlocks = new Set(['docParagraph', 'docHeading', 'docListItem'])

export interface EditorBlock {
  pos: number
  size: number
  path: string
  node: PmJson
}

/** Every text block of the document the Docs editor shows once loaded, with its position */
export async function editorBlocks(bytes: Uint8Array) {
  const parsed = await parseDocx(bytes)
  const json = convert.blocksToPmDoc(parsed.blocks, undefined, convert.pmDocOptions(parsed))
  // setContent, then what the plugins append to it (tableEditing's fixTables).
  const empty = EditorState.create({ schema: editorSchema, plugins: [tableEditing()] })
  const content = editorSchema.nodeFromJSON(json).content
  const doc = empty.apply(empty.tr.replaceWith(0, empty.doc.content.size, content)).doc
  const blocks: EditorBlock[] = []
  const walk = (parent: PmNode, start: number, path: string) =>
    parent.forEach((node, offset, i) => {
      const pos = start + offset
      if (editorTextBlocks.has(node.type.name))
        blocks.push({ pos, size: node.nodeSize, path: `${path}/${i}`, node: node.toJSON() })
      else if (!node.isLeaf) walk(node, pos + 1, `${path}/${i}`)
    })
  walk(doc, 0, '')
  return { blocks, size: doc.content.size }
}

export const editorText = (node: PmJson) =>
  (node.content ?? []).map((child) => (child.type === 'text' ? child.text : '\n')).join('')

/** The renderer's text blocks sit where the editor's are, and the ones it may rewrite read alike */
export async function expectEditorPositions(bytes: Uint8Array) {
  const editor = await editorBlocks(bytes)
  const doc = await docxModel(bytes)
  const blocks: { pos: number; size: number; text?: string }[] = []
  doc.descendants((node, pos) => {
    if (!node.isTextblock) return true
    const text = node.attrs.source === null ? undefined : editorText(node.toJSON())
    blocks.push({ pos, size: node.nodeSize, ...(text === undefined ? {} : { text }) })
    return false
  })
  expect(doc.content.size).toBe(editor.size)
  expect(blocks.map(({ pos, size }) => [pos, size])).toEqual(
    editor.blocks.map(({ pos, size }) => [pos, size]),
  )
  blocks.forEach(({ text }, i) => {
    if (text !== undefined) expect(text).toBe(editorText(editor.blocks[i]!.node))
  })
}
