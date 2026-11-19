import { describe, expect, it } from 'vitest'

import { fontFamilyKey, rangeCovers } from '../src/html-picture'

const characters = (text: string): Set<number> =>
  new Set(Array.from(text, (character) => character.codePointAt(0)!))

describe('which font faces a picture of the page takes along', () => {
  it('takes a face whose range holds a character of the text', () => {
    expect(rangeCovers('U+AC00-D7A3', characters('report 한글'))).toBe(true)
    expect(rangeCovers('U+0041', characters('A'))).toBe(true)
  })

  it('leaves out a face that covers none of the text', () => {
    expect(rangeCovers('U+AC00-D7A3', characters('report'))).toBe(false)
    expect(rangeCovers('U+0400-04FF, U+0500-052F', characters('report 한글'))).toBe(false)
  })

  it('reads a list of ranges, in either case', () => {
    expect(rangeCovers('U+0400-04FF, u+61-7a', characters('x'))).toBe(true)
  })

  it('reads a wildcard range', () => {
    expect(rangeCovers('U+4??', characters('\u0416'))).toBe(true)
    expect(rangeCovers('U+4??', characters('\u0500'))).toBe(false)
  })

  it('reads a range beyond the basic plane', () => {
    expect(rangeCovers('U+1F600-1F64F', characters('\u{1F600}'))).toBe(true)
  })

  it('takes a face that names no range: it covers every character', () => {
    expect(rangeCovers('', characters('x'))).toBe(true)
  })

  it('names a family the same from a stylesheet and from a computed style', () => {
    expect(fontFamilyKey('"Noto Sans KR"')).toBe(fontFamilyKey(' noto sans kr '))
    expect(fontFamilyKey("'Pretendard'")).toBe('pretendard')
  })
})
