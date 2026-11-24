/**
 * Records the engine does not model survive an edit elsewhere and the web save
 * (vendor/rhwp-core/patches/0047).
 *
 * `opaque-record.hwp` is the editor's `blank.hwp` with two paragraphs written by the engine
 * and one record the engine does not know (tag 0x3fe, level 1, payload
 * `vendor-opaque-metadata`) appended to the first paragraph.
 */
import { readFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import { dirname, join } from 'node:path'
import { beforeAll, expect, it } from 'vitest'
import init, { HwpDocument } from '@rhwp/core'
import { openHwp, PARA_HEADER, recordsOf, SECTION, streamOf } from '../src/main/hwp5-file'
import { openHwpFromBytes } from '../src/web/open-hwp'
import { webSaveBytes } from '../src/web/save-hwp'

const require = createRequire(import.meta.url)
const FIXTURE = join(__dirname, 'fixtures', 'opaque-record.hwp')
const OPAQUE = 0x3fe
const PAYLOAD = Buffer.from('vendor-opaque-metadata')

beforeAll(async () => {
  const wasm = join(dirname(require.resolve('@rhwp/core')), 'rhwp_bg.wasm')
  await init({ module_or_path: readFileSync(wasm) })
})

/** The opaque records of each body paragraph, by paragraph. */
function opaqueByParagraph(file: Uint8Array): Buffer[][] {
  const opened = openHwp(file)
  const section = opened && streamOf(opened, SECTION(0))
  const records = section && recordsOf(section)
  if (!section || !records) throw new Error('unreadable section')
  const out: Buffer[][] = []
  for (const record of records) {
    if (record.tag === PARA_HEADER && record.level === 0) out.push([])
    else if (record.tag === OPAQUE) out.at(-1)?.push(section.subarray(record.body, record.end))
  }
  return out
}

it('keeps a record of an untouched paragraph through an edit and the web save', () => {
  const bytes = new Uint8Array(readFileSync(FIXTURE))
  expect(opaqueByParagraph(bytes)).toEqual([[PAYLOAD], []])
  const opened = openHwpFromBytes(
    { id: 'opaque', fileName: 'opaque-record.hwp', bytes, virtualPath: '/web/opaque-record.hwp' },
    true,
  )
  if (!opened) throw new Error('open failed')

  const doc = new HwpDocument(opened.bytes)
  let exported: Uint8Array
  try {
    doc.insertText(0, 1, 0, '가')
    exported = doc.exportHwp()
  } finally {
    doc.free()
  }
  const saved = webSaveBytes(opened.base, 'hwp', exported, [])

  if (!saved.ok) throw new Error(saved.error)
  expect(saved.tailsLost).toBe(false)
  expect(opaqueByParagraph(saved.bytes)).toEqual([[PAYLOAD], []])
})
