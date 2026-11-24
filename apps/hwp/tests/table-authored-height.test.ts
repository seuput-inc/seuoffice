import { readFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import { dirname, join } from 'node:path'
import init, { HwpDocument } from '@rhwp/core'
import { beforeAll, expect, it } from 'vitest'

beforeAll(async () => {
  const dir = dirname(createRequire(import.meta.url).resolve('@rhwp/core'))
  await init({ module_or_path: readFileSync(join(dir, 'rhwp_bg.wasm')) })
})

// Patch 0048: a cell width edit keeps the authored table height instead of the sum of row heights.
it('keeps the authored table height through a cell width edit and both save formats', () => {
  const doc = HwpDocument.createEmpty()
  try {
    doc.createBlankDocument()
    const { paraIdx, controlIdx } = JSON.parse(doc.createTable(0, 0, 0, 2, 2))
    const height = (d: HwpDocument) =>
      JSON.parse(d.getTableProperties(0, paraIdx, controlIdx)).tableHeight
    const authored = height(doc)
    const rows = [0, 2].map((cell) =>
      Number(JSON.parse(doc.getCellProperties(0, paraIdx, controlIdx, cell)).height),
    )
    expect(authored).toBeGreaterThan(rows[0]! + rows[1]!)
    const edit = doc.setCellProperties(0, paraIdx, controlIdx, 0, JSON.stringify({ width: 15000 }))
    expect(JSON.parse(edit).ok).toBe(true)
    expect(height(doc)).toBe(authored)
    for (const bytes of [doc.exportHwp(), doc.exportHwpx()]) {
      const reopened = new HwpDocument(bytes)
      try {
        expect(height(reopened)).toBe(authored)
      } finally {
        reopened.free()
      }
    }
  } finally {
    doc.free()
  }
})
