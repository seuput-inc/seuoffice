import { readFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import { dirname, join } from 'node:path'
import init, { HwpDocument } from '@rhwp/core'
import { beforeAll, expect, it } from 'vitest'
import { applyHwpText } from '../src/shared/collab-text-model'
import type { HwpPosition } from '../src/shared/collab-text'

const require = createRequire(import.meta.url)
beforeAll(async () => {
  await init({
    module_or_path: readFileSync(join(dirname(require.resolve('@rhwp/core')), 'rhwp_bg.wasm')),
  })
})

const inner = [
  { controlIndex: 0, cellIndex: 9, cellParaIndex: 3 },
  { controlIndex: 0, cellIndex: 1, cellParaIndex: 0 },
]
const at = (offset: number, path: HwpPosition['path'] = [], paragraph = 0): HwpPosition => ({
  section: 0,
  paragraph,
  path,
  offset,
})

it.each(['hwp', 'hwpx'] as const)(
  'keeps a nested-cell edit and a body paragraph split on %s reopen',
  (format) => {
    const doc = new HwpDocument(readFileSync(join(__dirname, 'fixtures/nested-form.hwp')))
    try {
      const readCell = (source: HwpDocument) =>
        source.getTextInCellByPath(0, 2, JSON.stringify(inner), 0, 100)
      expect(readCell(doc)).toBe('정기시험')
      applyHwpText(doc, {
        kind: 'splice',
        from: at(2, inner, 2),
        to: at(4, inner, 2),
        text: '점검',
      })
      const tail = doc.getParagraphCount(0) - 1
      const end = at(doc.getParagraphLength(0, tail), [], tail)
      applyHwpText(doc, { kind: 'splice', from: end, to: end, text: '첫째🙂\n둘째' })
      const reopened = new HwpDocument(format === 'hwp' ? doc.exportHwp() : doc.exportHwpx())
      try {
        expect(readCell(reopened)).toBe('정기점검')
        expect(reopened.getTextRange(0, tail, end.offset, 3)).toBe('첫째🙂')
        expect(reopened.getTextRange(0, tail + 1, 0, 2)).toBe('둘째')
      } finally {
        reopened.free()
      }
    } finally {
      doc.free()
    }
  },
)

it('rejects a stale endpoint before deleting the valid part of its range', () => {
  const doc = new HwpDocument(readFileSync(join(__dirname, 'fixtures/nested-form.hwp')))
  try {
    expect(() =>
      applyHwpText(doc, { kind: 'splice', from: at(2, inner, 2), to: at(5, inner, 2), text: '' }),
    ).toThrow()
    expect(doc.getTextInCellByPath(0, 2, JSON.stringify(inner), 0, 4)).toBe('정기시험')
  } finally {
    doc.free()
  }
})
