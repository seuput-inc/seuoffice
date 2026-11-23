import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import { pathToFileURL } from 'node:url'
import { resolve } from 'node:path'
import JSZip from 'jszip'
import { SaxesParser } from 'saxes'

// Run from an isolated consumer beside the bundled trace-xlsx-guest.mjs. Never ship this probe.
const { launch } = await import(
  new URL('sandbox.js', import.meta.resolve('@seuoffice/history-renderer')).href
)
const guest = pathToFileURL(resolve('trace-xlsx-guest.mjs'))
const ns = 'http://schemas.openxmlformats.org/spreadsheetml/2006/main'
const pkg = 'http://schemas.openxmlformats.org/package/2006/'
const rel = 'http://schemas.openxmlformats.org/officeDocument/2006/relationships'
const part = 'xl/worksheets/sheet1.xml'
const value = (r, c) => ((r * 83 + c * 37) % 10000) / 10
const address = (r, c) => String.fromCharCode(65 + c) + r
async function book(rows, cols) {
  const z = new JSZip()
  z.file(
    '[Content_Types].xml',
    `<Types xmlns="${pkg}content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/${part}" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/></Types>`,
  )
  z.file(
    '_rels/.rels',
    `<Relationships xmlns="${pkg}relationships"><Relationship Id="r1" Type="${rel}/officeDocument" Target="xl/workbook.xml"/></Relationships>`,
  )
  z.file(
    'xl/workbook.xml',
    `<workbook xmlns="${ns}" xmlns:r="${rel}"><sheets><sheet name="Survey" sheetId="1" r:id="r1"/></sheets></workbook>`,
  )
  z.file(
    'xl/_rels/workbook.xml.rels',
    `<Relationships xmlns="${pkg}relationships"><Relationship Id="r1" Type="${rel}/worksheet" Target="worksheets/sheet1.xml"/></Relationships>`,
  )
  const xml =
    `<worksheet xmlns="${ns}"><sheetData>` +
    Array.from(
      { length: rows },
      (_, i) =>
        `<row r="${i + 1}">` +
        Array.from(
          { length: cols },
          (_, c) => `<c r="${address(i + 1, c)}"><v>${value(i + 1, c)}</v></c>`,
        ).join('') +
        '</row>',
    ).join('') +
    '</sheetData></worksheet>'
  z.file(part, xml)
  return {
    base: await z.generateAsync({ type: 'nodebuffer', compression: 'DEFLATE' }),
    partBytes: Buffer.byteLength(xml),
  }
}
function reopen(xml, rows, cols) {
  let key,
    text,
    count = 0
  const p = new SaxesParser()
  p.on('opentag', (tag) => {
    if (tag.name === 'c') key = tag.attributes.r
    if (tag.name === 'v') text = ''
  })
  p.on('text', (v) => {
    if (text !== undefined) text += v
  })
  p.on('closetag', (tag) => {
    if (tag.name === 'v') {
      const row = Math.floor(count / cols) + 1,
        col = count % cols
      assert.equal(key, address(row, col))
      assert.equal(Number(text), row === 1 && col === 1 ? -42 : value(row, col))
      count++
      text = undefined
    }
  })
  p.write(xml).close()
  assert.equal(count, rows * cols)
}
const traces = []
for (const [rows, cols] of [
  [20, 8],
  [300, 16],
  [1000, 26],
]) {
  const { base, partBytes } = await book(rows, cols)
  const change = (v, col, n) => ({
    v,
    author: v === 1 ? 'A' : 'B',
    client: `device${v}`,
    seq: 1,
    ops: [
      {
        id: `edit${v}`,
        m: [
          [
            'sheet.mutation.set-range-values',
            { subUnitId: 'sheet-1', cellValue: { 0: { [col]: { v: n, t: 2 } } } },
          ],
        ],
      },
    ],
  })
  const h = Buffer.from(
    JSON.stringify({
      epoch: 'survey',
      version: 2,
      format: { name: 'xlsx', schema: 3 },
      baseHash: createHash('sha256').update(base).digest('hex'),
      changes: [change(1, 0, -21), change(2, 1, -42)],
      undo: { epoch: 'survey', v: 1, index: 0 },
    }),
  )
  const size = Buffer.alloc(4)
  size.writeUInt32LE(base.length)
  for (let rep = 0; rep < (rows === 1000 ? 1 : 5); rep++) {
    const start = performance.now()
    const frame = Buffer.concat([size, base, h])
    const spawn = performance.now()
    const output = await launch(guest, frame)
    const end = performance.now()
    const split = 4 + output.readUInt32LE(0)
    const { ms, error } = JSON.parse(output.toString('utf8', 4, split))
    const bytes = output.subarray(split)
    if (rows === 1000) {
      assert.equal(error?.phase, 'engineOpen')
      assert.equal(error.code, 'unsupported')
      assert.equal(bytes.length, 0)
    } else {
      assert.equal(error, undefined)
      reopen(await (await JSZip.loadAsync(bytes)).file(part).async('string'), rows, cols)
    }
    traces.push({
      rows,
      cols,
      rep,
      baseBytes: base.length,
      partBytes,
      jsonBytes: h.length,
      frameBytes: frame.length,
      stdoutBytes: output.length,
      resultBytes: bytes.length,
      concatMs: spawn - start,
      launchToStdoutMs: end - spawn,
      ...ms,
      error,
    })
  }
}
console.log(JSON.stringify(traces))
