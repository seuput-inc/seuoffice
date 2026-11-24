import { readFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import { dirname, join } from 'node:path'
import init, { HwpDocument } from '@rhwp/core'
import { beforeAll, describe, expect, it } from 'vitest'
import {
  mapHwpPosition,
  parseHwpPosition,
  parseHwpTextSplice,
  parseHwpTextSplices,
  rebaseHwpText,
  type HwpPosition,
  type HwpTextSplice,
} from '../src/shared/collab-text'
import { applyHwpText } from '../src/shared/collab-text-model'

const at = (offset: number, paragraph = 0, path: HwpPosition['path'] = []): HwpPosition => ({
  section: 0,
  paragraph,
  path,
  offset,
})
const splice = (from: HwpPosition, to: HwpPosition, text = ''): HwpTextSplice => ({
  kind: 'splice',
  from,
  to,
  text,
})
const insert = (pos: HwpPosition, text: string) => splice(pos, pos, text)
const soft = (pos: HwpPosition): HwpTextSplice => ({
  kind: 'lineBreak',
  from: pos,
  to: pos,
  text: '\n',
})
const cell = (controlIndex: number, cellIndex: number, cellParaIndex: number) => ({
  controlIndex,
  cellIndex,
  cellParaIndex,
})

function applyBody(base: string[], ops: HwpTextSplice[]): string[] {
  const lines = base.slice()
  for (const { from, to, text, kind } of ops) {
    const first = [...lines[from.paragraph]],
      last = [...lines[to.paragraph]]
    if (from.offset > first.length || to.offset > last.length || from.path.length || to.path.length)
      throw Error('Invalid test address')
    const addition = kind === 'splice' ? text.split('\n') : [text]
    addition[0] = first.slice(0, from.offset).join('') + addition[0]
    addition[addition.length - 1] += last.slice(to.offset).join('')
    lines.splice(from.paragraph, to.paragraph - from.paragraph + 1, ...addition)
  }
  return lines
}

function diamond(base: string[], a: HwpTextSplice[], b: HwpTextSplice[], expected: string[]) {
  const pair = rebaseHwpText(a, b)
  if (!pair) throw Error(`Unexpected conflict: ${JSON.stringify({ a, b })}`)
  expect(applyBody(applyBody(base, a), pair.pending)).toEqual(expected)
  expect(applyBody(applyBody(base, b), pair.confirmed)).toEqual(expected)
}

describe('HWP text wire format', () => {
  it('checks every coordinate, the full container ancestry, direction and canonical text', () => {
    const nested = at(2, 3, [cell(4, 5, 6), cell(7, 8, 1)])
    const end = at(0, 3, [cell(4, 5, 6), cell(7, 8, 2)])
    expect(parseHwpTextSplices([splice(nested, end, '가\t🙂\n'), soft(nested)])).toEqual([
      splice(nested, end, '가\t🙂\n'),
      soft(nested),
    ])
    const invalid = [
      splice(end, nested),
      splice(nested, nested),
      splice(nested, { ...end, section: 1 }),
      splice(nested, { ...end, paragraph: 4 }),
      splice(nested, at(0, 3, [cell(4, 5, 0), cell(7, 8, 2)])),
      splice(nested, at(0, 3, [cell(4, 5, 6), cell(7, 9, 2)])),
      { ...soft(nested), to: end },
      { ...soft(nested), text: 'x' },
      { ...soft(nested), kind: 'execute' },
      { ...soft(nested), method: 'insertText' },
      insert(nested, '\r\n'),
      insert(nested, '\ud800'),
      insert(nested, '\u0000'),
      insert(nested, 'x'.repeat(262145)),
      insert(at(0x7fffffff), 'x'),
      insert(at(0, 0x7fffffff), '\n'),
    ]
    for (const value of invalid) expect(parseHwpTextSplice(value)).toBeNull()
    for (const coordinate of [-1, 0.5, NaN, Infinity, '0', 0x80000000]) {
      expect(parseHwpPosition({ ...nested, offset: coordinate })).toBeNull()
      expect(
        parseHwpPosition({ ...nested, path: [{ ...nested.path[0], controlIndex: coordinate }] }),
      ).toBeNull()
    }
    expect(parseHwpPosition({ ...nested, path: Array(17).fill(cell(0, 0, 0)) })).toBeNull()
    expect(parseHwpPosition({ ...nested, path: Array(1) })).toBeNull()
    expect(parseHwpTextSplices(Array(257).fill(soft(nested)))).toBeNull()
    expect(
      parseHwpTextSplices([insert(nested, 'a'.repeat(140000)), insert(nested, 'b'.repeat(140000))]),
    ).toBeNull()
  })
})

describe('text convergence', () => {
  it('keeps both insertions, with confirmed first at the same offset', () => {
    diamond(['가나'], [insert(at(1), '🙂')], [insert(at(1), '𝄞')], ['가🙂𝄞나'])
    diamond(['가나'], [soft(at(1))], [insert(at(1), '\n')], ['가\n', '나'])
  })

  it('keeps subsequent edits on both sides of a split replacement', () => {
    diamond(
      ['abcdef'],
      [splice(at(1), at(5), 'X\n🙂'), insert(at(1, 1), '!')],
      [insert(at(3), 'Y'), insert(at(4), 'Z'), splice(at(7), at(8))],
      ['aX', '🙂!YZ'],
    )
  })

  it('preserves a surviving insert when a later local delete crosses multiple rebased fragments', () => {
    diamond(
      ['abcdefghi'],
      [splice(at(1), at(8)), insert(at(1), '甲')],
      [insert(at(2), 'X'), insert(at(6), 'Y'), splice(at(3), at(8))],
      ['a甲Xi'],
    )
  })

  it('commutes every pair of ranges through splits, merges and overlapping replacements', () => {
    // Original scalar slots are a, 🙂, paragraph boundary, b. Keep a slot unless either edit deletes it.
    const slots = ['a', '🙂', '\n', 'b']
    const positions = [at(0), at(1), at(2), at(0, 1), at(1, 1)]
    for (let l = 0; l < positions.length; l++)
      for (let r = l; r < positions.length; r++) {
        for (let x = 0; x < positions.length; x++)
          for (let y = x; y < positions.length; y++) {
            for (const left of ['', '甲', '\n甲'])
              for (const right of ['', '乙', '乙\n']) {
                if ((!left && l === r) || (!right && x === y)) continue
                let expected = ''
                for (let i = 0; i <= slots.length; i++) {
                  if (i === l) expected += left
                  if (i === x) expected += right
                  if (i < slots.length && !(l <= i && i < r) && !(x <= i && i < y))
                    expected += slots[i]
                }
                diamond(
                  ['a🙂', 'b'],
                  [splice(positions[l], positions[r], left)],
                  [splice(positions[x], positions[y], right)],
                  expected.split('\n'),
                )
              }
          }
      }
  })
})

describe('cursor and nested control positions', () => {
  it('maps both selection affinities in scalar positions without turning Shift+Enter into a paragraph', () => {
    const root = at(3),
      split = insert(root, '🙂\n𝄞a')
    expect(mapHwpPosition(root, [split], 'before')).toEqual(root)
    expect(mapHwpPosition(root, [split])).toEqual(at(2, 1))
    expect(mapHwpPosition(at(4), [split])).toEqual(at(3, 1))
    expect(mapHwpPosition(at(4), [soft(root)])).toEqual(at(5))
    expect(mapHwpPosition(at(0x7fffffff), [soft(root)])).toBeNull()
    expect(rebaseHwpText([insert(at(0), 'x')], [insert(at(0x7ffffffe), 'a\n')])).toBeNull()
    const join = splice(at(3), at(2, 1), '🙂')
    expect(mapHwpPosition(at(1, 1), [join], 'before')).toEqual(root)
    expect(mapHwpPosition(at(5, 1), [join])).toEqual(at(7))
  })

  it('shifts descendants of later paragraphs and refuses to guess which split control kept its cell', () => {
    const outer = cell(2, 3, 5),
      inner = cell(4, 1, 2)
    const nested = at(7, 4, [outer, inner])
    expect(mapHwpPosition(nested, [insert(at(1, 3), '\n')])).toEqual({ ...nested, paragraph: 5 })
    expect(mapHwpPosition(nested, [splice(at(1, 2), at(0, 3))])).toEqual({
      ...nested,
      paragraph: 3,
    })
    const earlier = insert(at(1, 4, [cell(2, 3, 2)]), '\n\n')
    expect(mapHwpPosition(nested, [earlier])).toEqual(at(7, 4, [cell(2, 3, 7), inner]))
    const split = insert(at(1, 4, [outer]), '\n')
    const inside = insert(nested, 'x')
    expect(rebaseHwpText([split], [inside])).toBeNull()
    const moves = new Map([
      [split, [{ beforeParagraph: 5, beforeControl: 4, afterParagraph: 6, afterControl: 1 }]],
    ])
    const moved = at(7, 4, [cell(2, 3, 6), cell(1, 1, 2)])
    expect(mapHwpPosition(nested, [split], 'after', moves)).toEqual(moved)
    const claim = moves.get(split)![0]
    expect(
      mapHwpPosition(
        nested,
        [split],
        'after',
        new Map([[split, [claim, { ...claim, afterControl: 2 }]]]),
      ),
    ).toBeNull()
    expect(rebaseHwpText([split], [inside], moves)).toEqual({
      confirmed: [split],
      pending: [insert(moved, 'x')],
    })
    expect(
      rebaseHwpText([split], [splice(at(4, 4, [outer]), at(5, 4, [outer])), inside], moves),
    ).toBeNull()
    expect(mapHwpPosition(at(7, 4, [cell(2, 0, 5), inner]), [split])).toEqual(
      at(7, 4, [cell(2, 0, 5), inner]),
    )
  })
})

const require = createRequire(import.meta.url)
beforeAll(async () => {
  await init({
    module_or_path: readFileSync(join(dirname(require.resolve('@rhwp/core')), 'rhwp_bg.wasm')),
  })
})

it('replays both orders in an HWP nested cell without swallowing a peer insertion or a soft break', () => {
  const path = [cell(0, 9, 3), cell(0, 1, 0)]
  const first = [splice(at(1, 2, path), at(4, 2, path), '🙂\n끝')]
  const second = [insert(at(2, 2, path), '새'), soft(at(3, 2, path))]
  const pair = rebaseHwpText(first, second)!
  const bytes = readFileSync(join(__dirname, 'fixtures/nested-form.hwp'))
  for (const ops of [
    [...first, ...pair.pending],
    [...second, ...pair.confirmed],
  ]) {
    const doc = new HwpDocument(bytes)
    try {
      for (const op of ops) applyHwpText(doc, op)
      expect(doc.getTextInCellByPath(0, 2, JSON.stringify(path), 0, 100)).toBe('정🙂')
      expect(doc.getTextInCellByPath(0, 2, JSON.stringify([path[0], cell(0, 1, 1)]), 0, 100)).toBe(
        '끝새\n',
      )
    } finally {
      doc.free()
    }
  }
})
