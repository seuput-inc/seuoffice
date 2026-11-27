import assert from 'node:assert/strict'
import test from 'node:test'
import { frameGaps } from '../apps/web/tests/bench-xlsx-frame-gaps.ts'

test('a stall across either edge counts, but intervals touching only an endpoint do not', () => {
  const frames = [0, 120, 140, 280, 400]
  assert.deepEqual(frameGaps(frames, 10, 270), [
    { atMs: 110, gapMs: 120 },
    { atMs: 130, gapMs: 20 },
    { atMs: 270, gapMs: 140 },
  ])
  assert.deepEqual(frameGaps(frames, 120, 140), [{ atMs: 20, gapMs: 20 }])
  assert.deepEqual(frameGaps(frames, 150, 200), [{ atMs: 130, gapMs: 140 }])
})

test('missing samples cannot certify a scroll gap below the limit', () => {
  for (const frames of [[], [110, 200], [100, 190]]) {
    assert.throws(() => frameGaps(frames, 100, 200), /do not cover/)
  }
})
