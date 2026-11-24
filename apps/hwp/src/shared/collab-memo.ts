import type { HwpDocument } from '@rhwp/core'
import { parseHwpPosition, type HwpPosition } from './collab-text'

export type HwpMemoInsert = {
  kind: 'insertMemo'
  at: HwpPosition
  end: number
  author: string
  text: string
  unixMs: number
  offsetMinutesEast: number
}

function integer(n: unknown, min: number, max: number): n is number {
  return typeof n === 'number' && Number.isInteger(n) && n >= min && n <= max
}

export function parseHwpMemo(value: unknown): HwpMemoInsert | null {
  if (!value || typeof value !== 'object') return null
  const keys = ['kind', 'at', 'end', 'author', 'text', 'unixMs', 'offsetMinutesEast']
  if (Object.keys(value).length !== keys.length || keys.some((key) => !Object.hasOwn(value, key)))
    return null
  const {
    kind,
    at: position,
    end,
    author,
    text,
    unixMs,
    offsetMinutesEast,
  } = value as Record<string, unknown>
  const at = parseHwpPosition(position)
  if (
    kind !== 'insertMemo' ||
    !at ||
    !integer(end, at.offset, 0x7fffffff) ||
    typeof author !== 'string' ||
    author.length > 64 ||
    typeof text !== 'string' ||
    text.length > 100000 ||
    /[\ud800-\udfff]/u.test(author) ||
    /[\ud800-\udfff]/u.test(text) ||
    !integer(unixMs, 0, 253402214399999) ||
    !integer(offsetMinutesEast, -1439, 1439)
  )
    return null
  return { kind, at, end, author, text, unixMs, offsetMinutesEast }
}

/** Sample once, before making the local call. Receivers never read either clock. */
export function captureHwpMemo(
  method: string,
  args: readonly unknown[],
  time: Date,
): HwpMemoInsert | null {
  if (method !== 'insertMemo' && method !== 'insertMemoByPath') return null
  const nested = method === 'insertMemoByPath'
  const i = nested ? 3 : 2
  const raw = args[i + 2]
  if (args.length !== i + 3 || typeof raw !== 'string' || (nested && typeof args[2] !== 'string'))
    return null
  const input = JSON.parse(raw)
  if (
    !input ||
    typeof input !== 'object' ||
    Object.keys(input).some((key) => key !== 'author' && key !== 'text')
  )
    return null
  const path = nested ? JSON.parse(String(args[2])) : []
  if (nested && !path?.length) return null
  return parseHwpMemo({
    kind: 'insertMemo',
    at: { section: args[0], paragraph: args[1], path, offset: args[i] },
    end: args[i + 1],
    author: input.author ?? '',
    text: input.text ?? '',
    unixMs: time.getTime(),
    offsetMinutesEast: -time.getTimezoneOffset(),
  })
}

export function applyHwpMemo(doc: HwpDocument, op: HwpMemoInsert): string {
  const { at, end, author, text, unixMs, offsetMinutesEast } = op
  const json = JSON.stringify({ author, text, authoredAt: { unixMs, offsetMinutesEast } })
  return at.path.length
    ? doc.insertMemoByPath(at.section, at.paragraph, JSON.stringify(at.path), at.offset, end, json)
    : doc.insertMemo(at.section, at.paragraph, at.offset, end, json)
}
