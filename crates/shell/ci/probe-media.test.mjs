// Unit tests of the PDF reader of the probe checker (`cd crates/shell/ci && npx vitest run
// probe-media.test.mjs`).

import { describe, expect, it } from 'vitest'
import { fillsNear, marker, measurePdf } from './probe-media.mjs'

// A one-page PDF in the shape of a printed page with a stamped header: the page draws a form
// XObject and then a black square of its own, and the form draws a red one.
function pageWithForm() {
  const stream = (dictionary, body) =>
    `<<${dictionary}/Length ${body.length}>>\nstream\n${body}\nendstream`
  const objects = [
    '<</Type/Catalog/Pages 2 0 R>>',
    '<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 600 800]>>',
    '<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<</XObject<</Stamp 5 0 R>>>>>>',
    stream('', 'q /Stamp Do Q 0 0 0 rg 50 700 36 36 re f'),
    stream('/Type/XObject/Subtype/Form/BBox[0 0 600 40]', '1 0 0 rg 0 0 600 40 re f'),
  ]
  const body = objects.map((object, index) => `${index + 1} 0 obj\n${object}\nendobj\n`).join('')
  return Buffer.from(`%PDF-1.4\n${body}trailer\n<</Root 1 0 R>>\n%%EOF\n`, 'latin1')
}

describe('measurePdf', () => {
  it('reads the fills of a page that draws a form XObject, once each', () => {
    const [page] = measurePdf(pageWithForm()).pages
    expect(page.mediaBox).toEqual([0, 0, 600, 800])
    expect(fillsNear(page, [255, 0, 0])).toHaveLength(1)
    expect(fillsNear(page, [0, 0, 0])).toHaveLength(1)
    expect(marker(page)).toMatchObject({ left: 50, top: 64, width: 36 })
  })
})
