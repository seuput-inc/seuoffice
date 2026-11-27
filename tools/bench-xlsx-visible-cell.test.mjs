import assert from 'node:assert/strict'
import test from 'node:test'
import { visibleCell } from '../apps/web/tests/bench-xlsx-visible-cell.ts'

const word = (text, x = 145, y = 74) => `5\t1\t42\t1\t1\t1\t${x}\t${y}\t40\t9\t90.26\t${text}`

test('only the fixture value inside B2 certifies the captured sheet', () => {
  const text = '689.77'
  for (const tsv of [word('Opening'), word('689.7'), word(text, 145, 8), word(text, 215)]) {
    assert.equal(visibleCell(tsv, 'xl', text), null)
  }
  assert.equal(visibleCell(word('720.23'), 'xl', '720.23'), null)
  assert.deepEqual(visibleCell(`${word('Opening')}\r\n${word(text)}\r\n`, 'xl', text), {
    text,
    x: 145,
    y: 74,
    width: 40,
    height: 9,
  })
})
