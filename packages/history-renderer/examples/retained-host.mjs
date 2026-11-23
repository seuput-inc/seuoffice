import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import { createHistoryServer, memoryHistoryStore } from '@seuoffice/collab/server'
import { renderBytes } from '@seuoffice/history-renderer'
import JSZip from 'jszip'

const xml = 'http://schemas.openxmlformats.org'
const zip = new JSZip()
zip.file(
  '[Content_Types].xml',
  `<Types xmlns="${xml}/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>`,
)
zip.file(
  '_rels/.rels',
  `<Relationships xmlns="${xml}/package/2006/relationships"><Relationship Id="rId1" Type="${xml}/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>`,
)
zip.file(
  'word/document.xml',
  `<w:document xmlns:w="${xml}/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>One</w:t></w:r></w:p><w:p><w:r><w:t>Two</w:t></w:r></w:p></w:body></w:document>`,
)
const base = await zip.generateAsync({ type: 'uint8array' })
const epoch = 'demo-epoch'
const documentId = 'demo-document'
const alice = { documentId, userId: 'alice', name: 'Alice', canEdit: true }
const store = memoryHistoryStore((p) => p.documentId === documentId && p.userId === alice.userId)
const server = createHistoryServer({
  store,
  render: (bytes, retained) =>
    renderBytes(bytes, new TextEncoder().encode(JSON.stringify(retained))),
})
try {
  await store.create(documentId, {
    id: epoch,
    base,
    baseHash: createHash('sha256').update(base).digest('hex'),
    format: { name: 'docx', schema: 4 },
  })
  async function append(head, author, pos, text) {
    const step = {
      stepType: 'replace',
      from: pos,
      to: pos,
      slice: { content: [{ type: 'text', text }] },
    }
    const receipt = await store.append(documentId, epoch, {
      base: head,
      client: author,
      author,
      seq: 1,
      ops: [{ kind: 'steps', steps: [step] }],
    })
    assert.equal(receipt.kind, 'appended')
  }
  await append(0, 'alice', 4, '!')
  await append(1, 'bob', 10, '?')
  const command = {
    requestId: 'undo-alice',
    expected: { epoch, version: 2 },
    target: { epoch, v: 1, index: 0 },
  }
  const receipt = await server.undoHistory(alice, command)
  assert.deepEqual(receipt, { kind: 'undone', version: 3 })
  assert.deepEqual(await server.undoHistory(alice, command), receipt)
  const { bytes } = await server.exportHistory(alice, { epoch, version: 3 })
  const reopened = await (await JSZip.loadAsync(bytes)).file('word/document.xml').async('string')
  assert.match(reopened, />One</)
  assert.match(reopened, />Two\?</)
  console.log('Version 3: Alice’s edit was undone; Bob’s “Two?” remains.')
} finally {
  await server.close()
}
