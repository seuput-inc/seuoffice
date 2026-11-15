import { afterEach, describe, expect, it, vi } from 'vitest'
import katex from 'katex'
import { latexToMathMl } from '../../../../packages/pdf-structure/src/epub/math'
import { canonicalLocale, latexMathMl } from './math'

afterEach(() => vi.restoreAllMocks())

describe('PDF MathML service', () => {
  it.each([
    { latex: String.raw`\frac{x}{2}`, display: false },
    { latex: String.raw`\sum_{i=0}^n i`, display: true },
    { latex: 'x < y & z', display: false },
    { latex: 'x\u0001 + y', display: false },
  ])('matches the original KaTeX output for $latex', ({ latex, display }) => {
    expect(latexMathMl({ latex, display })).toBe(latexToMathMl(latex, display))
  })

  it('returns null for a KaTeX parse failure', () => {
    expect(latexMathMl({ latex: String.raw`\unknowncommand`, display: false })).toBeNull()
  })

  it('does not disguise a service error as a parse failure', () => {
    vi.spyOn(katex, 'renderToString').mockImplementation(() => {
      throw new TypeError('renderer failure')
    })
    expect(() => latexMathMl({ latex: 'x', display: false })).toThrow('renderer failure')
    expect(() => latexMathMl({ latex: 'x' })).toThrow('invalid LaTeX MathML request')
  })
})

describe('PDF locale canonicalization', () => {
  it.each([
    ['en_us', 'en-US'],
    [' zh_hant_tw ', 'zh-Hant-TW'],
    ['de-DE', 'de-DE'],
    ['', null],
    ['not a locale', null],
  ])('normalizes %s', (tag, expected) => {
    expect(canonicalLocale({ tag })).toBe(expected)
  })

  it('propagates service failures instead of returning null', () => {
    vi.spyOn(Intl, 'getCanonicalLocales').mockImplementation(() => {
      throw new TypeError('locale service failure')
    })
    expect(() => canonicalLocale({ tag: 'en' })).toThrow('locale service failure')
    expect(() => canonicalLocale({ tag: 3 })).toThrow('invalid locale request')
  })
})
