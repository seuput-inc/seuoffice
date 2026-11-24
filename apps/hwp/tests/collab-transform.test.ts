import { readFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import { dirname, join } from 'node:path'
import init, { HwpDocument } from '@rhwp/core'
import JSZip from 'jszip'
import { beforeAll, expect, it } from 'vitest'
import { applyHwpOp, type HwpOp } from '../src/shared/collab-editor-ops'
import { captureHwpStructure } from '../src/shared/collab-structure'
import type { HwpPosition, HwpTextSplice } from '../src/shared/collab-text'
import { rebaseHwpOps } from '../src/shared/collab-transform'

const fixture = (name: string) => readFileSync(join(__dirname, 'fixtures', name))
let image: Uint8Array
beforeAll(async () => {
  const require = createRequire(import.meta.url)
  await init({
    module_or_path: readFileSync(join(dirname(require.resolve('@rhwp/core')), 'rhwp_bg.wasm')),
  })
  image = await (
    await JSZip.loadAsync(fixture('picture-brightness-contrast-cell-fill.hwpx'))
  )
    .file('BinData/image1.png')!
    .async('uint8array')
})
const at = (paragraph: number, offset = 0, path: HwpPosition['path'] = []): HwpPosition => ({
  section: 0,
  paragraph,
  path,
  offset,
})
const cell = (index: number, paragraph = 12) =>
  at(paragraph, 0, [{ controlIndex: 0, cellIndex: index, cellParaIndex: 0 }])
const ins = (p: HwpPosition, text: string): HwpTextSplice => ({
  kind: 'splice',
  from: p,
  to: p,
  text,
})
const del = (paragraph: number) => ({ kind: 'deleteParagraph', section: 0, paragraph }) as const
const row = { kind: 'deleteRow', section: 0, paragraph: 12, control: 0, row: 0 } as const
function picture(doc: HwpDocument, p: HwpPosition) {
  const op = captureHwpStructure(doc, 'insertPictureInCell', [
    0,
    p.paragraph,
    p.offset,
    JSON.stringify(p.path),
    image,
    4800,
    450,
    64,
    6,
    'png',
    '공동 편집 그림',
    null,
    null,
  ])
  if (op?.kind !== 'insertCellPicture') throw new Error('Expected a picture')
  return op
}
function replay(doc: HwpDocument, ops: readonly HwpOp[]) {
  for (const op of ops) applyHwpOp(doc, op)
}

it('keeps a lost picture available for copying and replays dependent typing only in the surviving row', () => {
  const doc = new HwpDocument(fixture('cell-line-break-form.hwpx'))
  try {
    const removed = picture(doc, cell(0))
    const second = JSON.parse(doc.getTableDimensions(0, 12, 0)).colCount
    const kept = cell(second),
      text = doc.getTextInCellByPath(0, 12, JSON.stringify(kept.path), 0, 100)
    const r = rebaseHwpOps(
      doc,
      [row],
      [removed, ins(kept, '가🙂'), ins({ ...kept, offset: 2 }, '끝')],
    )
    expect(r.deleted).toEqual([removed])
    expect(r.retry).toEqual([])
    expect(r.mapCaret(cell(0))).toBeNull()
    expect(r.mapCaret({ ...kept, offset: 3 })).toEqual({ ...cell(0), offset: 3 })
    expect(doc.getTextInCellByPath(0, 12, JSON.stringify(cell(0).path), 0, 100)).toBe(text)
    replay(doc, r.replay)
    expect(doc.getTextInCellByPath(0, 12, JSON.stringify(cell(0).path), 0, 103)).toBe(
      '가🙂끝' + text,
    )
  } finally {
    doc.free()
  }
})

it('applies a later genuine row deletion even when the server accepted the picture first', () => {
  const doc = new HwpDocument(fixture('cell-line-break-form.hwpx'))
  try {
    const r = rebaseHwpOps(doc, [picture(doc, cell(0))], [row, ins(cell(0), '남음')])
    expect(r.deleted).toEqual([])
    expect(r.retry).toEqual([])
    expect(JSON.parse(doc.getTableDimensions(0, 12, 0)).rowCount).toBe(2)
    replay(doc, r.replay)
    expect(JSON.parse(doc.getTableDimensions(0, 12, 0)).rowCount).toBe(1)
    expect(doc.getTextInCellByPath(0, 12, JSON.stringify(cell(0).path), 0, 2)).toBe('남음')
  } finally {
    doc.free()
  }
})

it('does not retarget a split and typing into a deleted paragraph, even when the next paragraph takes its index', () => {
  const doc = new HwpDocument(fixture('generated-form-tables.hwpx'))
  try {
    const split = ins(at(2, 1), '\n'),
      lost = ins(at(3), '복구용')
    const original = doc.getTextRange(0, 3, 0, 300)
    const r = rebaseHwpOps(doc, [del(2)], [split, lost, ins(at(4), '남음')])
    expect(r.deleted).toEqual([split, lost])
    expect(r.retry).toEqual([])
    expect(r.mapCaret(at(3, 1))).toBeNull()
    replay(doc, r.replay)
    expect(doc.getTextRange(0, 2, 0, 302)).toBe('남음' + original)
  } finally {
    doc.free()
  }
})

it('keeps a legitimate paragraph deletion after confirmed typing and shifts a dependent nested edit through both axes', () => {
  const doc = new HwpDocument(fixture('cell-line-break-form.hwpx'))
  try {
    const r = rebaseHwpOps(
      doc,
      [ins(at(1), '\n'), ins(at(3), '서버')],
      [del(2), ins(cell(0, 3), '계속')],
    )
    expect(r.deleted).toEqual([])
    expect(r.retry).toEqual([])
    replay(doc, r.replay)
    expect(doc.getTextInCellByPath(0, 4, JSON.stringify(cell(0).path), 0, 5)).toBe('계속키워드')
  } finally {
    doc.free()
  }
})

it('transforms a sequential remote split/merge and a replacement around a remote insertion', () => {
  const doc = new HwpDocument(fixture('generated-form-tables.hwpx'))
  try {
    const old = [...doc.getTextRange(0, 2, 0, 300)]
    expect(old.length).toBeGreaterThanOrEqual(4)
    const remote: HwpTextSplice[] = [
      ins(at(2, 2), '\n!'),
      { kind: 'splice', from: at(2, 2), to: at(3), text: '' },
    ]
    const pending: HwpTextSplice[] = [
      { kind: 'splice', from: at(2, 1), to: at(2, 4), text: '한🙂' },
      ins(at(2, 2), '글'),
    ]
    const r = rebaseHwpOps(doc, remote, pending)
    expect(r.deleted).toEqual([])
    expect(r.retry).toEqual([])
    replay(doc, r.replay)
    expect(doc.getTextRange(0, 2, 0, 302)).toBe(old[0] + '한글🙂!' + old.slice(4).join(''))
  } finally {
    doc.free()
  }
})

it('follows the actual table control after its host splits instead of dropping a nested edit', () => {
  const doc = new HwpDocument(fixture('nested-form.hwp'))
  try {
    const target = at(2, 1, [
      { controlIndex: 0, cellIndex: 9, cellParaIndex: 3 },
      { controlIndex: 0, cellIndex: 1, cellParaIndex: 0 },
    ])
    const split = ins(at(2), '\n')
    const r = rebaseHwpOps(doc, [split], [ins(target, '甲'), ins({ ...target, offset: 2 }, '乙')])
    expect(r.deleted).toEqual([])
    expect(r.retry).toEqual([])
    const p = r.mapCaret({ ...target, offset: 3 })!
    replay(doc, r.replay)
    expect(doc.getTextInCellByPath(0, p.paragraph, JSON.stringify(p.path), 1, 2)).toBe('甲乙')
  } finally {
    doc.free()
  }
})

it.each([2, 3])(
  'does not let deleting original paragraph %i erase both halves of a confirmed merge',
  (index) => {
    const doc = new HwpDocument(fixture('generated-form-tables.hwpx'))
    try {
      const a = doc.getTextRange(0, 2, 0, 1000),
        b = doc.getTextRange(0, 3, 0, 1000)
      const join: HwpTextSplice = {
        kind: 'splice',
        from: at(2, [...a].length),
        to: at(3),
        text: '',
      }
      const r = rebaseHwpOps(doc, [join], [del(index), ins(at(3), '별도')])
      expect(r.retry).toEqual([del(index)])
      expect(r.deleted).toEqual([])
      replay(doc, r.replay)
      expect(doc.getTextRange(0, 2, 0, 2000)).toBe(a + b)
      expect(doc.getTextRange(0, 3, 0, 2)).toBe('별도')
    } finally {
      doc.free()
    }
  },
)

it('drops typing in a wholly consumed paragraph but preserves typing in its surviving successor', () => {
  const doc = new HwpDocument(fixture('generated-form-tables.hwpx'))
  try {
    const b = doc.getTextRange(0, 3, 0, 1000)
    const lost = ins(at(2, 1), '삭제된 문단')
    const erase: HwpTextSplice = { kind: 'splice', from: at(2), to: at(3), text: '' }
    const r = rebaseHwpOps(doc, [erase], [lost, ins(at(3), '앞')])
    expect(r.deleted).toEqual([lost])
    expect(r.retry).toEqual([])
    replay(doc, r.replay)
    expect(doc.getTextRange(0, 2, 0, 1001)).toBe('앞' + b)
  } finally {
    doc.free()
  }
})

it('maps a later command to the memo this client actually created, not a concurrent memo that got its provisional ID', () => {
  const bytes = fixture('generated-form-tables.hwpx'),
    doc = new HwpDocument(bytes),
    author = new HwpDocument(bytes)
  try {
    const local = {
      kind: 'insertMemo',
      at: at(2, 1),
      end: 2,
      author: '저자',
      text: '원본',
      unixMs: 1000,
      offsetMinutesEast: 0,
    } as const
    const remote = { ...local, at: at(3, 1) }
    applyHwpOp(author, local)
    const saved = (JSON.parse(author.getMemoList()) as { id: number; text: string }[]).find(
      (m) => m.text === '원본',
    )!
    const patch = {
      kind: 'command',
      action: 'memoText',
      id: saved.id,
      before: '원본',
      text: '수정',
    } as const
    const r = rebaseHwpOps(doc, [remote], [local, patch])
    expect(r.deleted).toEqual([])
    expect(r.retry).toEqual([])
    replay(doc, r.replay)
    const memos: { paraIndex: number; text: string }[] = JSON.parse(doc.getMemoList())
    expect(memos).toEqual(
      expect.arrayContaining([
        expect.objectContaining({ paraIndex: 2, text: '수정' }),
        expect.objectContaining({ paraIndex: 3, text: '원본' }),
      ]),
    )
  } finally {
    author.free()
    doc.free()
  }
})

it('does not advance confirmed if a later remote operand is invalid', () => {
  const doc = new HwpDocument(fixture('generated-form-tables.hwpx'))
  try {
    const prior = doc.getTextRange(0, 2, 0, 1000)
    expect(() => rebaseHwpOps(doc, [ins(at(2), '미확정'), del(1000000)], [])).toThrow()
    expect(doc.getTextRange(0, 2, 0, 1000)).toBe(prior)
  } finally {
    doc.free()
  }
})

it('keeps server-first row insertion order and types in each independently created row', () => {
  const doc = new HwpDocument(fixture('cell-line-break-form.hwpx'))
  try {
    const add = {
      kind: 'command',
      action: 'grid',
      at: at(2),
      control: 0,
      axis: 'row',
      index: 0,
      side: 'after',
    } as const
    const r = rebaseHwpOps(doc, [add, ins(cell(1, 2), '서버')], [add, ins(cell(1, 2), '로컬')])
    expect(r.deleted).toEqual([])
    expect(r.retry).toEqual([])
    replay(doc, r.replay)
    expect(JSON.parse(doc.getTableDimensions(0, 2, 0)).rowCount).toBe(4)
    expect(doc.getTextInCellByPath(0, 2, JSON.stringify(cell(1).path), 0, 2)).toBe('서버')
    expect(doc.getTextInCellByPath(0, 2, JSON.stringify(cell(2).path), 0, 2)).toBe('로컬')
  } finally {
    doc.free()
  }
})
