import { describe, expect, it } from 'vitest'
import { encodeSlideNumbers, reviveSlideNumbers } from '../slides'

describe('slide native numeric wire', () => {
  it('revives nested non-finite jscompat markers without changing ordinary objects or bytes', () => {
    const bytes = new Uint8Array([1, 2])
    const input = { nested: [{ $num: 'NaN' }, { $num: 'Infinity' }, { $num: '-Infinity' }], ordinary: { $num: 'Infinity', other: 1 }, bytes }
    const output = reviveSlideNumbers(input) as { nested: number[]; ordinary: unknown; bytes: Uint8Array }
    expect(Number.isNaN(output.nested[0])).toBe(true)
    expect(output.nested.slice(1)).toEqual([Infinity, -Infinity])
    expect(output.ordinary).toEqual(input.ordinary)
    expect(output.bytes).toBe(bytes)
    expect(input.nested[0]).toEqual({ $num: 'NaN' })
  })

  it('encodes nested non-finite measurement results for native', () => {
    const value = { rows: [NaN, Infinity, -Infinity, 1.5], zero: 0 }
    expect(encodeSlideNumbers(value)).toEqual({
      rows: [{ $num: 'NaN' }, { $num: 'Infinity' }, { $num: '-Infinity' }, 1.5],
      zero: 0,
    })
  })
})
