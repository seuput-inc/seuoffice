import '@seuoffice/engine-wasm/node'
import { createHash } from 'node:crypto'
import { HistoryError, type HistoryRequest, type Json } from '@seuoffice/collab/client'
import { describe, expect, it } from 'vitest'
import { buildDocx } from '../../../docx-engine/tests/helpers/build-docx'
import { render } from '../../src/replay'
import { editorBlocks, editorText, expectEditorPositions } from './docs-editor'

const p = (text = '') => (text ? `<w:p><w:r><w:t>${text}</w:t></w:r></w:p>` : '<w:p/>')
const tc = (text = '', tcPr = '') => `<w:tc>${tcPr && `<w:tcPr>${tcPr}</w:tcPr>`}${p(text)}</w:tc>`
const table = (columns: number, rows: string) =>
  `<w:tbl><w:tblPr/><w:tblGrid>${'<w:gridCol w:w="1500"/>'.repeat(columns)}</w:tblGrid>${rows}</w:tbl>`

// Tables whose cells the engine, convert.ts or the editor's table plugin do not keep one to one
// with the w:tc elements of the XML.
const cases: Record<string, string> = {
  'a legacy horizontal merge beside a grid gap':
    table(
      4,
      `<w:tr><w:trPr><w:gridAfter w:val="1"/></w:trPr>${tc('A', '<w:hMerge w:val="restart"/>')}${tc('', '<w:hMerge/>')}${tc()}</w:tr>`,
    ) + p('after'),
  'a row starting after a grid gap':
    table(
      3,
      `<w:tr><w:trPr><w:gridBefore w:val="1"/></w:trPr>${tc('B')}${tc('C')}</w:tr><w:tr>${tc('a')}${tc('b')}${tc('c')}</w:tr>`,
    ) + p('after'),
  'a cell inside a content control':
    table(
      2,
      `<w:tr><w:sdt><w:sdtPr/><w:sdtContent>${tc('S')}</w:sdtContent></w:sdt>${tc('T')}</w:tr><w:tr>${tc('a')}${tc('b')}</w:tr>`,
    ) + p('after'),
  'a legacy horizontal merge beside a content control':
    table(
      3,
      `<w:tr>${tc('A', '<w:hMerge w:val="restart"/>')}${tc('', '<w:hMerge/>')}<w:sdt><w:sdtContent>${tc()}</w:sdtContent></w:sdt></w:tr>`,
    ) + p('after'),
  'a row the table plugin fills up':
    table(2, `<w:tr>${tc('a')}${tc('b')}</w:tr><w:tr>${tc('c')}</w:tr>`) + p('after'),
  'a merged-away cell holding a nested table':
    table(
      2,
      `<w:tr><w:tc><w:tcPr><w:vMerge/></w:tcPr>${table(1, `<w:tr>${tc('in')}</w:tr>`)}${p()}</w:tc>${tc('b')}</w:tr>`,
    ) + p('after'),
  'a body ending with a table': p('before') + table(2, `<w:tr>${tc('a')}${tc('b')}</w:tr>`),
}

function request(base: Uint8Array, ops: Json): HistoryRequest {
  return {
    baseHash: createHash('sha256').update(base).digest('hex'),
    epoch: 'tables',
    format: { name: 'docx', schema: 4 },
    version: 1,
    changes: [{ v: 1, author: 'alice', client: 'alice', seq: 1, ops: [ops] }],
  }
}

const typed = (pos: number, text: string): Json => ({
  kind: 'steps',
  steps: [
    { stepType: 'replace', from: pos, to: pos, slice: { content: [{ type: 'text', text }] } },
  ],
})

describe.each(Object.entries(cases))('%s', (_, bodyXml) => {
  const file = buildDocx({ bodyXml })

  it('puts every paragraph where the Docs editor has it', async () => {
    await expectEditorPositions(await file)
  })

  it('writes typed text into the paragraph the editor shows it in, or refuses it', async () => {
    const bytes = await file
    const { blocks } = await editorBlocks(bytes)
    for (const { pos, path, node } of blocks) {
      let out
      try {
        out = await render(bytes, request(bytes, typed(pos + 1, 'x')))
      } catch (error) {
        expect(error).toBeInstanceOf(HistoryError)
        expect(error).toMatchObject({ code: 'unsupported' })
        continue
      }
      const after = await editorBlocks(out.bytes)
      expect(after.blocks.map((block) => block.path)).toEqual(blocks.map((block) => block.path))
      for (const block of after.blocks)
        expect(editorText(block.node), block.path).toBe(
          (block.path === path ? 'x' : '') +
            editorText(blocks.find((b) => b.path === block.path)!.node),
        )
      expect(editorText(after.blocks.find((block) => block.path === path)!.node)).toBe(
        'x' + editorText(node),
      )
    }
  })
})
