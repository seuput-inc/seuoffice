import { applyStoredAiReadings } from '../../../../packages/pdf-structure/src/ai-read/merge'
import { treeToMarkdown } from '../../../../packages/pdf-structure/src/markdown/write'
import { treeToStructuredText } from '../../../../packages/pdf-structure/src/structured-text/format'
import {
  buildBilingualTree,
  buildTranslatedTree,
  translationReport,
  type TranslationReport,
} from '../../../../packages/pdf-structure/src/translate/result'
import {
  deriveBilingualTree,
  deriveTranslatedTree,
} from '../../../../packages/pdf-structure/src/tree/derive'
import type { DocumentTree, InlineRun } from '../../../../packages/pdf-structure/src/tree/types'
import type { StoredTranslation } from '../../../../packages/pdf-structure/src/store/store'

export function mergeReadings(
  tree: DocumentTree,
  readings: ReadonlyMap<string, { payload: unknown }>,
): DocumentTree {
  return tree.variant === 'original' && readings.size > 0
    ? applyStoredAiReadings(tree, readings)
    : tree
}

export function applyTranslation(
  tree: DocumentTree,
  translation:
    { lang: string; bilingual?: boolean; units?: { key: string; runs: InlineRun[] }[] } | undefined,
  stored: StoredTranslation | null | undefined,
): { tree: DocumentTree; report?: TranslationReport } {
  if (!translation) return { tree }
  const { lang, bilingual } = translation
  if (translation.units) {
    const units = new Map(translation.units.map((unit) => [unit.key, unit.runs]))
    return {
      tree: bilingual
        ? deriveBilingualTree(tree, lang, units)
        : deriveTranslatedTree(tree, lang, units),
    }
  }
  if (stored === undefined) {
    const error = new Error('the stored translation cannot be read here')
    error.name = 'NotAvailableError'
    throw error
  }
  return {
    tree: bilingual
      ? buildBilingualTree(tree, lang, stored)
      : buildTranslatedTree(tree, lang, stored),
    report: translationReport(tree, lang, stored),
  }
}

export { treeToMarkdown, treeToStructuredText }
