import { createHash } from 'node:crypto'
import { PDFDocument, PDFHexString, PDFName } from 'pdf-lib'
import { describe, expect, it } from 'vitest'
import { render } from './render'

function openPdf(base: Uint8Array) {
  return render(base, {
    epoch: 'admission',
    format: { name: 'pdf', schema: 2 },
    baseHash: createHash('sha256').update(base).digest('hex'),
    version: 0,
    changes: [],
  })
}

async function markupFile(change = '') {
  const doc = await PDFDocument.create({ updateMetadata: false })
  const page = doc.addPage([300, 200])
  const ctx = doc.context
  const rect = [10, 15, 30, 25]
  const line = '1 0 0 RG 0.8 w 10 15.8 m 30 15.8 l S'
  const form = ctx.stream(
    change === 'page paint in a form'
      ? '10 15 20 10 re f'
      : change === 'oversize form'
        ? line.padEnd(2049, ' ')
        : change === 'extra paint group'
          ? `${line} ${line}`
          : change === 'unrelated geometry'
            ? line.replace('30 15.8 l', '45 17 l')
            : line,
    {
      Type: 'XObject',
      Subtype: 'Form',
      BBox: change === 'different BBox' ? [0, 0, 100, 100] : rect,
      Resources: change === 'nested form resource' ? { XObject: {} } : {},
    },
  )
  if (change === 'external stream') form.dict.set(PDFName.of('F'), PDFHexString.fromText('file'))
  const appearance = ctx.register(form)
  const annot = ctx.obj({
    Type: 'Annot',
    Subtype: 'Underline',
    F: 4,
    Rect: rect,
    QuadPoints: [10, 25, 30, 25, 10, 15, 30, 15],
    C: [1, 0, 0],
    T: PDFHexString.fromText('Author'),
    P: page.ref,
    AP: { N: appearance },
  })
  const annotation = ctx.register(annot)
  if (change === 'action on annotation') annot.set(PDFName.of('A'), ctx.obj({ S: 'Named' }))
  if (change === 'overlong subject')
    annot.set(PDFName.of('Subj'), PDFHexString.fromText('a'.repeat(201)))
  if (change === 'self as appearance') annot.set(PDFName.of('AP'), ctx.obj({ N: annotation }))
  if (change === 'another owner') annot.set(PDFName.of('P'), doc.addPage().ref)
  if (change === 'form as page contents') page.node.set(PDFName.of('Contents'), appearance)
  if (change === 'deep direct object') annot.set(PDFName.of('C'), ctx.obj([[[[[[[[0]]]]]]]]))
  if (change === 'over quad count')
    annot.set(
      PDFName.of('QuadPoints'),
      ctx.obj(Array(17).fill([10, 25, 30, 25, 10, 15, 30, 15]).flat()),
    )
  page.node.set(
    PDFName.of('Annots'),
    ctx.obj(change === 'repeated annot' ? [annotation, annotation] : [annotation]),
  )
  return doc.save({ useObjectStreams: false })
}

describe('bounded PDF markup admission', () => {
  it('admits a reachable passive markup and only its own appearance', async () => {
    await expect(openPdf(await markupFile())).resolves.toBeDefined()
  })
  it.each([
    'action on annotation',
    'self as appearance',
    'another owner',
    'form as page contents',
    'repeated annot',
    'nested form resource',
    'different BBox',
    'page paint in a form',
    'extra paint group',
    'unrelated geometry',
    'deep direct object',
    'over quad count',
    'external stream',
    'oversize form',
    'overlong subject',
  ])('rejects %s', async (change) => {
    await expect(openPdf(await markupFile(change))).rejects.toMatchObject({ code: 'unsupported' })
  })
})

async function stampFile(paint: string, extraState = false) {
  const doc = await PDFDocument.create({ updateMetadata: false })
  const page = doc.addPage()
  const ctx = doc.context
  const font = ctx.register(
    ctx.obj({
      Type: 'Font',
      Subtype: 'Type1',
      BaseFont: 'Helvetica-Bold',
      Encoding: 'WinAnsiEncoding',
    }),
  )
  page.node.set(
    PDFName.of('Resources'),
    ctx.obj({
      Font: { HRStampBold: font },
      ExtGState: {
        HRStampOpacity: extraState
          ? { Type: 'ExtGState', ca: 0.25, SMask: 'None' }
          : { Type: 'ExtGState', ca: 0.25 },
      },
    }),
  )
  page.node.set(PDFName.of('Contents'), ctx.register(ctx.stream(paint)))
  return doc.save({ useObjectStreams: false })
}

const stamp = 'q /HRStampOpacity gs BT 0 0 0 rg /HRStampBold 12 Tf 1 0 0 1 10 20 Tm <41> Tj ET Q'
describe('bounded PDF stamp admission', () => {
  it('admits a typed opacity resource referenced by a stamp', async () => {
    await expect(openPdf(await stampFile(stamp))).resolves.toBeDefined()
  })
  it.each([
    stamp.replace('0 0 0 rg', '2 0 0 rg'),
    stamp.replace('12 Tf', '0 Tf'),
    stamp.replace('/HRStampOpacity gs', '/Missing gs'),
    stamp.replace('1 0 0 1', '1 1 0 1'),
    'q 0 0 0 rg 1 0 0 1 10 20 cm /Span << /ActualText <FEFFAC00> >> BDC 0 0 m 1 0 l 1 1 l h f EMC Q',
  ])('rejects paint outside the stamp grammar: %s', async (paint) => {
    await expect(openPdf(await stampFile(paint))).rejects.toMatchObject({ code: 'unsupported' })
  })
  it('rejects other graphics state properties even with valid content', async () => {
    await expect(openPdf(await stampFile(stamp, true))).rejects.toMatchObject({
      code: 'unsupported',
    })
  })
})
