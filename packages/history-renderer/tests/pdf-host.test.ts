import { PDFArray, PDFDict, PDFDocument, PDFHexString, PDFName, PDFStream } from 'pdf-lib'
import * as server from '@seuoffice/collab/server'
import { it } from 'vitest'
import { examples, verifyHistory } from '../../../apps/web/tests/collab-history-cases'
import { render } from './render'

const pdf = examples.find((e) => e.name === 'pdf')!
const stamp = (text: string) => ({
  id: 'stamp',
  puts: [
    {
      b: 'stampCfg',
      v: { hf: null, wm: { text, sizeRatio: 0.1, angle: 35, opacity: 0.36, color: '#123456' } },
    },
  ],
})
const markup = (subject: string) => ({
  id: 'markup',
  puts: [
    {
      b: 'markups',
      id: 'note',
      v: {
        id: 'note',
        pageIndex: 0,
        type: 'highlight',
        color: [1, 1, 0],
        subject,
        quads: [[12, 24, 45, 24, 12, 15, 45, 15]],
      },
    },
  ],
})

async function reopen(bytes: Uint8Array) {
  const doc = await PDFDocument.load(bytes, { updateMetadata: false })
  const page = doc.getPage(0)
  const refs = page.node.Annots()
  const note = refs?.size()
    ? refs.lookup(0, PDFDict).lookup(PDFName.of('Subj'), PDFHexString).decodeText()
    : undefined
  const contents = page.node.Contents()
  const streams =
    contents instanceof PDFArray
      ? contents.asArray().map((v) => doc.context.lookup(v, PDFStream))
      : contents
        ? [contents]
        : []
  const paint = streams.map((s) => Buffer.from(s.getContents()).toString('ascii')).join('\n')
  return [doc.getTitle(), note, paint.includes(Buffer.from('Review').toString('hex').toUpperCase())]
}

for (const target of ['markup', 'stamp']) {
  it(`keeps the peer's ${target === 'stamp' ? 'markup' : 'stamp'} and the independent PDF fork when the host undoes its ${target}`, async () => {
    const isStamp = target === 'stamp'
    await verifyHistory(
      server,
      {
        ...pdf,
        reopen,
        alice: isStamp ? stamp('Review') : markup('검토'),
        independent: isStamp ? markup('검토') : stamp('Review'),
        dependent: isStamp ? stamp('Final') : markup('재검토'),
        atAlice: ['Base', isStamp ? undefined : '검토', isStamp],
        afterUndo: ['Base', isStamp ? '검토' : undefined, !isStamp],
      },
      render,
    )
  }, 30_000)
}
