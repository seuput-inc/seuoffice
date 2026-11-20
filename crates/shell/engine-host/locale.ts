import { shortDatePatternForSystemLocale } from '../../../packages/xlsx-gateway/src/shared/short-date'

export function shortDatePattern(input: unknown): string {
  if (typeof input !== 'string') throw new TypeError('invalid system locale')
  return shortDatePatternForSystemLocale(input)
}
