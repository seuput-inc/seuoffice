import katex from 'katex'
import { escapeAttr, xmlClean } from '../../../../packages/pdf-structure/src/epub/xml'

const KATEX_WRAPPER = /^<span class="katex">(<math[\s\S]*<\/math>)<\/span>$/

export function latexMathMl(input: unknown): string | null {
  if (
    !input ||
    typeof input !== 'object' ||
    !('latex' in input) ||
    typeof input.latex !== 'string' ||
    !('display' in input) ||
    typeof input.display !== 'boolean'
  ) {
    throw new TypeError('invalid LaTeX MathML request')
  }
  let html: string
  try {
    html = katex.renderToString(xmlClean(input.latex), {
      output: 'mathml',
      throwOnError: true,
      displayMode: input.display,
      strict: 'ignore',
    })
  } catch (error) {
    if (error instanceof katex.ParseError) return null
    throw error
  }
  const math = KATEX_WRAPPER.exec(html)?.[1] ?? html
  return math.replace(/^<math /, `<math alttext="${escapeAttr(input.latex)}" `)
}

export function canonicalLocale(input: unknown): string | null {
  if (!input || typeof input !== 'object' || !('tag' in input) || typeof input.tag !== 'string') {
    throw new TypeError('invalid locale request')
  }
  const tag = input.tag.trim().replace(/_/g, '-')
  if (!tag) return null
  try {
    return Intl.getCanonicalLocales(tag)[0] ?? null
  } catch (error) {
    if (error instanceof RangeError) return null
    throw error
  }
}
