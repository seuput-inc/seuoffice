import { createHash } from 'node:crypto'
import { renderBytes } from '@seuoffice/history-renderer'
import { render } from './render'
import type { HistoryRequest } from '@seuoffice/collab/client'
import JSZip from 'jszip'
import { describe, expect, it } from 'vitest'

function request(bytes: Uint8Array): HistoryRequest {
  return {
    epoch: 'epoch',
    baseHash: createHash('sha256').update(bytes).digest('hex'),
    changes: [],
    format: { name: 'docx', schema: 4 },
    version: 0,
  }
}

describe('byte and replay budgets', () => {
  it('rejects malformed UTF-8 or JSON before interpreting an operation', async () => {
    for (const bytes of [Uint8Array.of(0xff), new TextEncoder().encode('{')]) {
      await expect(renderBytes(Uint8Array.of(1), bytes)).rejects.toMatchObject({
        code: 'invalid_history',
      })
    }
  })

  it('refuses oversized input and oversized expanded parts', async () => {
    const bytes = new Uint8Array(2 * 1024 * 1024 + 1)
    await expect(render(bytes, request(bytes))).rejects.toMatchObject({ code: 'invalid_history' })
    const zip = new JSZip()
    zip.file('[Content_Types].xml', '<Types/>')
    zip.file('_rels/.rels', '<Relationships/>')
    zip.file('word/document.xml', 'x'.repeat(1024 * 1024 + 1))
    const zipped = await zip.generateAsync({ type: 'uint8array', compression: 'DEFLATE' })
    await expect(render(zipped, request(zipped))).rejects.toMatchObject({ code: 'invalid_history' })
  })

  it('bounds the entire prefix before opening the document', async () => {
    const bytes = Uint8Array.of(1)
    // An unsupported schema distinguishes preflight rejection from an attempt to open the file.
    const req = { ...request(bytes), format: { name: 'docx', schema: 42 } }
    const edit = { v: 1, client: 'a', seq: 1, author: 'a', ops: [0] }
    for (const changes of [
      Array.from({ length: 129 }, (_, i) => ({ ...edit, seq: i + 1, v: i + 1 })),
      [{ ...edit, ops: Array(257).fill(0) }],
      [{ ...edit, ops: ['a'.repeat(256 * 1024)] }],
    ]) {
      await expect(
        render(bytes, { ...req, changes, version: changes.length }),
      ).rejects.toMatchObject({ code: 'invalid_history' })
    }
  })
})
