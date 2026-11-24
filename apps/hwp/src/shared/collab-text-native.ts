import type { HwpDocument } from '@rhwp/core'
import {
  invalidHwpText,
  mapHwpPosition,
  parseHwpPosition,
  parseHwpTextSplice,
  rebaseHwpText,
  sameHwpContainer,
  type HwpPosition,
  type HwpTextSplice,
} from './collab-text'

type ParaMeta = {
  para_shape_id: number
  style_id: number
  column_type: 'None' | 'Section' | 'MultiColumn' | 'Page' | 'Column'
  raw_break_type: number
  numbering_restart: null | 'ContinuePrevious' | { NewStart: number }
  raw_header_extra: number[]
  tab_extended: number[][]
}

type RichAction =
  | {
      action: 'insert'
      text: string
      field: number | null
      afterInline: boolean
      tailSlots?: number
    }
  | { action: 'bodyLocal'; count: number; text: string; field: number | null; tailSlots?: number }
  | { action: 'cellReplace'; count: number; text: string; field: number | null }
  | { action: 'replace'; count: number; text: string }
  | { action: 'delete'; count: number }
  | { action: 'split' | 'splitFlat'; logical: number; meta: ParaMeta | null }
  | { action: 'merge' | 'mergeFlat' }
  | { action: 'flatRange'; endParagraph: number; endOffset: number }
  | { action: 'logicalRange'; logical: number; endParagraph: number; endLogical: number }

export type HwpRichTextOp = { kind: 'nativeText'; at: HwpPosition } & RichAction

type Axis = 'scalar' | 'insert' | 'local' | 'caret' | 'split' | 'toSplit' | 'anchors'
type WriteMode = 'insert' | 'afterInline' | 'bodyLocal' | 'cellReplace'
type NativeText = {
  getCollabTextPosition(s: number, p: number, path: string, offset: number, axis: Axis): string
  writeCollabText(
    s: number,
    p: number,
    path: string,
    offset: number,
    count: number,
    text: string,
    mode: WriteMode,
    field?: number,
  ): string
}

function engine(doc: HwpDocument): HwpDocument & NativeText {
  if (
    !('getCollabTextPosition' in doc) ||
    typeof doc.getCollabTextPosition !== 'function' ||
    !('writeCollabText' in doc) ||
    typeof doc.writeCollabText !== 'function'
  ) {
    throw new Error('The HWP engine does not support field-aware text replay')
  }
  return doc as HwpDocument & NativeText
}

function uint(v: unknown, max = 0x7fffffff): v is number {
  return typeof v === 'number' && Number.isInteger(v) && v >= 0 && v <= max
}
function record(v: unknown): v is Record<string, unknown> {
  return typeof v === 'object' && v !== null && !Array.isArray(v)
}
function keys(v: Record<string, unknown>, names: string[]): boolean {
  return Object.keys(v).length === names.length && names.every((k) => Object.hasOwn(v, k))
}
function text(v: unknown): v is string {
  return typeof v === 'string' && v.length <= 256 * 1024 && !invalidHwpText.test(v)
}
function uintArray(v: unknown, max: number): v is number[] {
  return Array.isArray(v) && v.length <= 4096 && Array.from(v).every((n) => uint(n, max))
}
function meta(v: unknown): v is ParaMeta {
  if (
    !record(v) ||
    !keys(v, [
      'para_shape_id',
      'style_id',
      'column_type',
      'raw_break_type',
      'numbering_restart',
      'raw_header_extra',
      'tab_extended',
    ])
  )
    return false
  const restart = v.numbering_restart
  return (
    uint(v.para_shape_id, 65535) &&
    uint(v.style_id, 255) &&
    uint(v.raw_break_type, 255) &&
    (v.column_type === 'None' ||
      v.column_type === 'Section' ||
      v.column_type === 'MultiColumn' ||
      v.column_type === 'Page' ||
      v.column_type === 'Column') &&
    (restart === null ||
      restart === 'ContinuePrevious' ||
      (record(restart) && keys(restart, ['NewStart']) && uint(restart.NewStart, 0xffffffff))) &&
    uintArray(v.raw_header_extra, 255) &&
    Array.isArray(v.tab_extended) &&
    v.tab_extended.length <= 4096 &&
    Array.from(v.tab_extended).every(
      (row) => Array.isArray(row) && row.length === 7 && uintArray(row, 65535),
    )
  )
}

export function parseHwpRichText(v: unknown): HwpRichTextOp | null {
  if (!record(v) || v.kind !== 'nativeText') return null
  const at = parseHwpPosition(v.at)
  if (!at) return null
  const { action, count, field, logical, endParagraph, endOffset, endLogical } = v
  const only = (...names: string[]) => keys(v, ['kind', 'at', 'action', ...names])
  const base = { kind: 'nativeText' as const, at }
  const tailSlots = v.tailSlots
  if (
    tailSlots !== undefined &&
    (!uint(tailSlots) ||
      tailSlots === 0 ||
      !uint(at.offset + tailSlots) ||
      (action === 'insert' ? v.afterInline !== false : action !== 'bodyLocal' || count !== 0))
  )
    return null
  const trailing = tailSlots === undefined ? {} : { tailSlots }
  const start = at.path.at(-1)?.cellParaIndex ?? at.paragraph
  switch (action) {
    case 'insert':
      return only('text', 'field', 'afterInline', ...Object.keys(trailing)) &&
        text(v.text) &&
        v.text !== '' &&
        (uint(field) || field === null) &&
        typeof v.afterInline === 'boolean' &&
        !(v.afterInline && at.path.length)
        ? { ...base, action, text: v.text, field, afterInline: v.afterInline, ...trailing }
        : null
    case 'bodyLocal':
    case 'cellReplace':
      return only('count', 'text', 'field', ...Object.keys(trailing)) &&
        uint(count, 8) &&
        text(v.text) &&
        v.text.length <= 16 &&
        [...v.text].length <= 8 &&
        !/[\t\n]/.test(v.text) &&
        (uint(field) || field === null) &&
        (count > 0 || v.text !== '') &&
        (action === 'bodyLocal'
          ? !at.path.length
          : at.path.length === 1 && count > 0 && v.text !== '')
        ? { ...base, action, count, text: v.text, field, ...trailing }
        : null
    case 'replace':
      return only('count', 'text') &&
        !at.path.length &&
        uint(count) &&
        text(v.text) &&
        (count > 0 || v.text !== '')
        ? { ...base, action, count, text: v.text }
        : null
    case 'delete':
      return only('count') && uint(count) && count > 0 ? { ...base, action, count } : null
    case 'split':
    case 'splitFlat':
      return only('logical', 'meta') &&
        uint(logical) &&
        (action !== 'splitFlat' || at.path.length === 1) &&
        (v.meta === null || meta(v.meta))
        ? { ...base, action, logical, meta: v.meta }
        : null
    case 'merge':
    case 'mergeFlat':
      return only() &&
        start > 0 &&
        at.offset === 0 &&
        (action !== 'mergeFlat' || at.path.length === 1)
        ? { ...base, action }
        : null
    case 'flatRange':
      return only('endParagraph', 'endOffset') &&
        at.path.length === 1 &&
        uint(endParagraph) &&
        uint(endOffset) &&
        (endParagraph > start || (endParagraph === start && endOffset > at.offset))
        ? { ...base, action, endParagraph, endOffset }
        : null
    case 'logicalRange':
      return only('logical', 'endParagraph', 'endLogical') &&
        !at.path.length &&
        uint(logical) &&
        uint(endParagraph) &&
        uint(endLogical) &&
        (endParagraph > start || (endParagraph === start && endLogical > logical))
        ? { ...base, action, logical, endParagraph, endLogical }
        : null
    default:
      return null
  }
}

function position(doc: HwpDocument, at: HwpPosition, axis: Axis, offset = at.offset) {
  const value: unknown = JSON.parse(
    engine(doc).getCollabTextPosition(
      at.section,
      at.paragraph,
      JSON.stringify(at.path),
      offset,
      axis,
    ),
  )
  if (
    !record(value) ||
    !uint(value.scalar) ||
    !uint(value.splitBefore) ||
    !uint(value.length) ||
    !(uint(value.field) || value.field === null) ||
    typeof value.afterInline !== 'boolean' ||
    typeof value.simple !== 'boolean'
  ) {
    throw new Error('Invalid native text position')
  }
  if (value.effectOffset !== undefined && !uint(value.effectOffset))
    throw new Error('Invalid split effect')
  if (
    value.anchors !== undefined &&
    (!Array.isArray(value.anchors) || !value.anchors.every((n: unknown) => uint(n)))
  )
    throw new Error('Invalid inline controls')
  return {
    scalar: value.scalar,
    splitBefore: value.splitBefore,
    length: value.length,
    field: value.field,
    afterInline: value.afterInline,
    simple: value.simple,
    effectOffset: value.effectOffset ?? value.scalar,
    anchors: value.anchors as number[] | undefined,
  }
}

export function hwpSplitOffset(doc: HwpDocument, at: HwpPosition): number {
  return position(doc, at, 'toSplit').splitBefore
}

export function writeHwpText(
  doc: HwpDocument,
  at: HwpPosition,
  text: string,
  mode: WriteMode = 'insert',
  count = 0,
  field: number | null = null,
): string {
  return engine(doc).writeCollabText(
    at.section,
    at.paragraph,
    JSON.stringify(at.path),
    at.offset,
    count,
    text,
    mode,
    field ?? undefined,
  )
}

function inParagraph(at: HwpPosition, paragraph: number, offset: number): HwpPosition {
  if (!at.path.length) return { ...at, paragraph, offset }
  const path = at.path.slice()
  path[path.length - 1] = { ...path[path.length - 1], cellParaIndex: paragraph }
  return { ...at, path, offset }
}

export function applyHwpRichText(doc: HwpDocument, op: HwpRichTextOp): void {
  const { at } = op
  const { section: s, paragraph: p, path, offset } = at
  const cell = path.at(-1)
  const pathJson = JSON.stringify(path)
  const info = position(doc, at, 'scalar')
  if ('count' in op && op.count > info.length - offset)
    throw new RangeError('Text range exceeds paragraph')
  const tailSlots = 'tailSlots' in op ? op.tailSlots : undefined
  if (tailSlots && offset !== info.length)
    throw new RangeError('Trailing control slot is no longer at paragraph end')
  const writeAt = tailSlots ? { ...at, offset: offset + tailSlots } : at
  let result: string
  switch (op.action) {
    case 'insert':
      result = writeHwpText(
        doc,
        writeAt,
        op.text,
        op.afterInline ? 'afterInline' : 'insert',
        0,
        op.field,
      )
      break
    case 'bodyLocal':
    case 'cellReplace':
      result = writeHwpText(doc, writeAt, op.text, op.action, op.count, op.field)
      break
    case 'replace':
      result = doc.replaceText(s, p, offset, op.count, op.text)
      break
    case 'delete':
      result = cell
        ? doc.deleteTextInCellByPath(s, p, pathJson, offset, op.count)
        : doc.deleteText(s, p, offset, op.count)
      break
    case 'split':
    case 'splitFlat': {
      if (position(doc, at, 'split', op.logical).scalar !== offset)
        throw new RangeError('Split slot no longer matches its paragraph')
      const restored = op.meta === null ? undefined : JSON.stringify(op.meta)
      result =
        op.action === 'splitFlat' && cell
          ? doc.splitParagraphInCell(
              s,
              p,
              cell.controlIndex,
              cell.cellIndex,
              cell.cellParaIndex,
              op.logical,
              restored,
            )
          : cell
            ? doc.splitParagraphInCellByPath(s, p, pathJson, op.logical, restored)
            : doc.splitParagraph(s, p, op.logical, restored)
      break
    }
    case 'merge':
    case 'mergeFlat':
      result =
        op.action === 'mergeFlat' && cell
          ? doc.mergeParagraphInCell(s, p, cell.controlIndex, cell.cellIndex, cell.cellParaIndex)
          : cell
            ? doc.mergeParagraphInCellByPath(s, p, pathJson)
            : doc.mergeParagraph(s, p)
      break
    case 'flatRange':
      if (!cell) throw new RangeError('Flat range needs a cell')
      position(doc, inParagraph(at, op.endParagraph, op.endOffset), 'scalar')
      result = doc.deleteRangeInCell(
        s,
        p,
        cell.controlIndex,
        cell.cellIndex,
        cell.cellParaIndex,
        offset,
        op.endParagraph,
        op.endOffset,
      )
      break
    case 'logicalRange':
      if (position(doc, at, 'caret', op.logical).scalar !== offset)
        throw new RangeError('Caret slot no longer matches its paragraph')
      result = doc.deleteRangeLogical(s, p, op.logical, op.endParagraph, op.endLogical)
      break
  }
  if (JSON.parse(result)?.ok !== true) throw new Error('HWP rejected the native text operation')
}

const methods = {
  insertText: 'insert',
  insertTextLogical: 'logicalInsert',
  insertTextInCell: 'insert',
  insertTextInCellDeferredPagination: 'insert',
  insertTextInCellByPath: 'insert',
  replaceBodyTextLocal: 'bodyLocal',
  replaceTextInCellDeferredPagination: 'cellReplace',
  replaceText: 'replace',
  deleteText: 'delete',
  deleteTextInCell: 'delete',
  deleteTextInCellDeferredPagination: 'delete',
  deleteTextInCellByPath: 'delete',
  splitParagraph: 'split',
  splitParagraphInCell: 'splitFlat',
  splitParagraphInCellByPath: 'split',
  mergeParagraph: 'merge',
  mergeParagraphInCell: 'mergeFlat',
  mergeParagraphInCellByPath: 'merge',
  deleteRange: 'range',
  deleteRangeInCell: 'flatRange',
  deleteRangeInCellByPath: 'range',
  deleteRangeLogical: 'logicalRange',
} as const

/** Capture the operands before Studio calls WASM. The operation is publishable only if that call succeeds. */
export function captureHwpText(
  doc: HwpDocument,
  method: string,
  args: readonly unknown[],
): HwpTextSplice | HwpRichTextOp | null {
  if (!Object.hasOwn(methods, method)) return null
  const action = methods[method as keyof typeof methods]
  const [section, paragraph] = args
  if (!uint(section) || !uint(paragraph)) return null
  let path: unknown = []
  let i = 2
  if (method.endsWith('ByPath')) {
    const raw = args[i++]
    if (typeof raw !== 'string' || raw.length > 4096) return null
    try {
      path = JSON.parse(raw)
    } catch {
      return null
    }
    // The native path reader accepts decimal digits and uses the first matching key.
    if (!Array.isArray(path) || !path.length || JSON.stringify(path) !== raw.replace(/\s/g, ''))
      return null
  } else if (method.includes('InCell')) {
    path = [{ controlIndex: args[i++], cellIndex: args[i++], cellParaIndex: args[i++] }]
  }
  let at = parseHwpPosition({ section, paragraph, path, offset: 0 })
  if (!at) return null
  const rich = (data: Record<string, unknown>) =>
    parseHwpRichText({ kind: 'nativeText', at, ...data })
  const splice = (to: HwpPosition, text: string, kind = 'splice') =>
    parseHwpTextSplice({ kind, from: at, to, text })
  if (action === 'merge' || action === 'mergeFlat') {
    if (args.length !== i) return null
    position(doc, at, 'scalar')
    return rich({ action })
  }
  if (action === 'range' && at.path.length) {
    const start = args[i++]
    if (!uint(start)) return null
    at = inParagraph(at, start, 0)
  }
  const offset = args[i++]
  if (!uint(offset)) return null
  const axis =
    action === 'split' || action === 'splitFlat'
      ? 'split'
      : action === 'logicalInsert' || action === 'logicalRange'
        ? 'caret'
        : action === 'bodyLocal'
          ? 'local'
          : 'scalar'
  let info
  try {
    info = position(doc, at, axis, offset)
  } catch (error) {
    // Ordinary scalar calls also work on engines that predate trailing-slot operands.
    if (action !== 'insert') throw error
    info = position(doc, at, 'insert', offset)
  }
  at = { ...at, offset: info.scalar }
  if (action === 'range' || action === 'flatRange' || action === 'logicalRange') {
    const endParagraph = args[i++],
      end = args[i++]
    if (args.length !== i || !uint(endParagraph) || !uint(end)) return null
    const to = inParagraph(at, endParagraph, end)
    position(doc, to, action === 'logicalRange' ? 'caret' : 'scalar')
    return action === 'logicalRange'
      ? rich({ action, logical: offset, endParagraph, endLogical: end })
      : action === 'flatRange'
        ? rich({ action, endParagraph, endOffset: end })
        : splice(to, '')
  }
  if (action === 'split' || action === 'splitFlat') {
    let restored: unknown = null
    if (args.length > i + 1) return null
    const raw = args[i]
    if (raw !== undefined && raw !== null) {
      if (typeof raw !== 'string' || raw.length > 256 * 1024) return null
      try {
        restored = JSON.parse(raw)
      } catch {
        return null
      }
      if (!meta(restored)) return null
    }
    if (action === 'split' && restored === null && info.simple && info.splitBefore === offset)
      return splice(at, '\n')
    return rich({ action, logical: offset, meta: restored })
  }
  const count = action === 'insert' || action === 'logicalInsert' ? 0 : args[i++]
  if (!uint(count) || count > info.length - at.offset) return null
  if (action === 'delete') return args.length === i ? rich({ action, count }) : null
  const raw = args[i++]
  if (args.length !== i || typeof raw !== 'string') return null
  const value = raw.replace(/\r\n?/g, '\n')
  if (!text(value)) return null
  if (
    (action === 'insert' ||
      action === 'logicalInsert' ||
      (action === 'bodyLocal' &&
        count === 0 &&
        raw.length <= 16 &&
        [...raw].length <= 8 &&
        !/[\r\t\n]/.test(raw))) &&
    info.simple &&
    info.field === null &&
    !info.afterInline &&
    (value === '\n' || !value.includes('\n'))
  ) {
    return splice(at, value, value === '\n' ? 'lineBreak' : 'splice')
  }
  const trailing =
    (action === 'bodyLocal' || action === 'insert') && offset > info.length
      ? { tailSlots: offset - info.length }
      : {}
  if (action === 'insert' || action === 'logicalInsert')
    return rich({
      action: 'insert',
      text: value,
      field: info.field,
      afterInline: info.afterInline,
      ...trailing,
    })
  return action === 'replace'
    ? rich({ action, text: value, count })
    : rich({ action, text: value, count, field: info.field, ...trailing })
}

export type HwpCaretOperand = { at: HwpPosition; slot: number }

/** Studio counts inline objects in body carets but not in cell text carets. */
export function captureHwpCaret(doc: HwpDocument, logicalAt: HwpPosition): HwpCaretOperand {
  const at = parseHwpPosition(logicalAt)
  if (!at) throw new RangeError('Invalid caret')
  if (at.path.length) {
    position(doc, at, 'scalar')
    return { at, slot: 0 }
  }
  const { scalar } = position(doc, at, 'caret')
  const slot = at.offset - doc.textToLogicalOffset(at.section, at.paragraph, scalar)
  if (!uint(slot)) throw new RangeError('Invalid inline control slot')
  return { at: { ...at, offset: scalar }, slot }
}

export function restoreHwpCaret(doc: HwpDocument, caret: HwpCaretOperand): HwpPosition | null {
  const { at, slot } = caret
  if (!uint(slot)) return null
  try {
    position(doc, at, 'scalar')
    if (at.path.length) return slot === 0 ? at : null
    const offset = doc.textToLogicalOffset(at.section, at.paragraph, at.offset) + slot
    return position(doc, at, 'caret', offset).scalar === at.offset ? { ...at, offset } : null
  } catch {
    return null
  }
}

export function richTextFragment(op: HwpRichTextOp): string {
  return 'text' in op ? op.text : op.action === 'split' || op.action === 'splitFlat' ? '\n' : ''
}

/** Describes scalar movement only. Never apply this projection in place of its native edit. */
export function hwpRichTextEffect(doc: HwpDocument, op: HwpRichTextOp): HwpTextSplice | null {
  let from = op.at,
    to = from,
    text = richTextFragment(op)
  switch (op.action) {
    case 'merge':
    case 'mergeFlat': {
      const prev = inParagraph(from, (from.path.at(-1)?.cellParaIndex ?? from.paragraph) - 1, 0)
      from = { ...prev, offset: position(doc, prev, 'scalar').length }
      break
    }
    case 'flatRange':
      to = inParagraph(from, op.endParagraph, op.endOffset)
      break
    case 'logicalRange': {
      const end = inParagraph(from, op.endParagraph, op.endLogical)
      to = captureHwpCaret(doc, end).at
      if (!samePoint(captureHwpCaret(doc, { ...from, offset: op.logical }).at, from)) return null
      // Removing only an inline object moves no scalar. This is not an empty operation on the wire.
      if (samePoint(from, to)) return { kind: 'splice', from, to, text: '' }
      break
    }
    case 'split':
    case 'splitFlat': {
      const { effectOffset } = position(doc, from, 'split', op.logical)
      from = to = { ...from, offset: effectOffset }
      break
    }
    default:
      if ('count' in op) to = { ...from, offset: from.offset + op.count }
      // Embedded LF has width one. A splice LF, unlike the native string, would create a paragraph.
      if (text.includes('\n')) text = text.replaceAll('\n', ' ')
  }
  return parseHwpTextSplice({ kind: 'splice', from, to, text })
}

type EndpointMap = (position: HwpPosition, affinity: 'before' | 'after') => HwpPosition | null
type ControlMap = (
  host: HwpPosition,
  control: number,
) => { at: HwpPosition; control: number } | null

function sameParagraph(a: HwpPosition, b: HwpPosition): boolean {
  return (
    sameHwpContainer(a, b) &&
    (a.path.at(-1)?.cellParaIndex ?? a.paragraph) === (b.path.at(-1)?.cellParaIndex ?? b.paragraph)
  )
}

function samePoint(a: HwpPosition | null, b: HwpPosition): boolean {
  return a !== null && a.offset === b.offset && sameParagraph(a, b)
}

function textControlMap(map: EndpointMap): ControlMap {
  return (host, controlIndex) => {
    const child = map(
      {
        ...host,
        offset: 0,
        path: [...host.path, { controlIndex, cellIndex: 0, cellParaIndex: 0 }],
      },
      'after',
    )
    const step = child?.path.at(-1)
    if (!child || !step || child.path.length !== host.path.length + 1) return null
    return { at: { ...child, path: child.path.slice(0, -1) }, control: step.controlIndex }
  }
}

/** All objects at this scalar must survive in the same order, including the ones to the right of the caret. */
export function sameHwpInlineBoundary(
  before: HwpDocument,
  after: HwpDocument,
  map: EndpointMap,
  old: HwpPosition,
  at: HwpPosition,
  control: ControlMap = textControlMap(map),
): boolean {
  const a = position(before, old, 'anchors').anchors,
    b = position(after, at, 'anchors').anchors
  return (
    !!a &&
    !!b &&
    a.length === b.length &&
    a.every((ci, i) => {
      const mapped = control(old, ci)
      return mapped !== null && sameParagraph(mapped.at, at) && mapped.control === b[i]
    })
  )
}

function sameRangeControls(
  before: HwpDocument,
  after: HwpDocument,
  control: ControlMap,
  original: HwpTextSplice,
  target: HwpTextSplice,
): boolean {
  const destinations = new Set<string>()
  for (const [doc, range, destination] of [
    [after, target, true],
    [before, original, false],
  ] as const) {
    let budget = 32768
    if (range.to.paragraph - range.from.paragraph > 4096) return false
    for (let p = range.from.paragraph; p <= range.to.paragraph; p++) {
      const positions: unknown = JSON.parse(doc.getControlTextPositions(range.from.section, p))
      if (!Array.isArray(positions) || (budget -= positions.length) < 0) return false
      for (const [controlIndex, offset] of positions.entries()) {
        if (!uint(offset)) return false
        if (
          (p === range.from.paragraph && offset < range.from.offset) ||
          (p === range.to.paragraph && offset > range.to.offset)
        )
          continue
        if (destination) destinations.add(`${p}/${controlIndex}`)
        else {
          const mapped = control(
            { section: original.from.section, paragraph: p, offset, path: [] },
            controlIndex,
          )
          if (
            !mapped ||
            mapped.at.section !== target.from.section ||
            mapped.at.path.length !== 0 ||
            !destinations.delete(`${mapped.at.paragraph}/${mapped.control}`)
          )
            return false
        }
      }
    }
  }
  return destinations.size === 0
}

/** Reattach native operands to one transformed positional fragment. Null means a boundary, field or atomic range was lost. */
export function remapHwpRichText(
  op: HwpRichTextOp,
  before: HwpDocument,
  after: HwpDocument,
  map: EndpointMap,
  shape: HwpTextSplice,
  control: ControlMap = textControlMap(map),
): HwpRichTextOp | null {
  try {
    // The scalar mapper supplies inline-control identity, not an ordering for raw field ends and title marks.
    if ('tailSlots' in op && op.tailSlots) return null
    const effect = hwpRichTextEffect(before, op)
    if (!effect || shape.text !== effect.text) return null
    const at = shape.from
    const end = shape.to
    const endParagraph = end.path.at(-1)?.cellParaIndex ?? end.paragraph
    if (!sameHwpContainer(at, end)) return null
    const count = end.offset - at.offset
    let next: HwpRichTextOp
    switch (op.action) {
      case 'delete':
        if (!sameParagraph(at, end) || count <= 0 || count > op.count) return null
        next = { ...op, at, count }
        break
      case 'merge':
      case 'mergeFlat':
        if (
          !samePoint(map(op.at, 'before'), end) ||
          end.offset !== 0 ||
          position(after, at, 'scalar').length !== at.offset ||
          (at.path.at(-1)?.cellParaIndex ?? at.paragraph) + 1 !== endParagraph
        )
          return null
        next = { ...op, at: end }
        break
      case 'split':
      case 'splitFlat': {
        const original = map(op.at, 'after')
        // A scalar transform cannot certify that restored raw header bytes and TAB extension rows still belong to the suffix.
        if (op.meta !== null) return null
        if (
          !original ||
          !samePoint(at, end) ||
          !sameHwpInlineBoundary(before, after, map, op.at, original, control)
        )
          return null
        const delta = op.logical - hwpSplitOffset(before, op.at)
        const logical = hwpSplitOffset(after, original) + delta
        if (!uint(logical)) return null
        const info = position(after, original, 'split', logical)
        if (
          info.scalar !== original.offset ||
          !samePoint({ ...original, offset: info.effectOffset }, at)
        )
          return null
        next = { ...op, at: original, logical }
        break
      }
      case 'logicalRange': {
        const left = captureHwpCaret(before, { ...op.at, offset: op.logical })
        const right = captureHwpCaret(before, inParagraph(op.at, op.endParagraph, op.endLogical))
        if (
          !samePoint(map(left.at, 'after'), at) ||
          !samePoint(map(right.at, 'before'), end) ||
          !sameHwpInlineBoundary(before, after, map, left.at, at, control) ||
          !sameHwpInlineBoundary(before, after, map, right.at, end, control) ||
          !sameRangeControls(before, after, control, effect, shape)
        )
          return null
        const l = restoreHwpCaret(after, { at, slot: left.slot }),
          r = restoreHwpCaret(after, { at: end, slot: right.slot })
        if (!l || !r) return null
        next = { ...op, at, logical: l.offset, endParagraph, endLogical: r.offset }
        break
      }
      case 'flatRange':
        next = { ...op, at, endParagraph, endOffset: end.offset }
        break
      default: {
        if (
          !sameParagraph(at, end) ||
          !samePoint(map(op.at, 'after'), at) ||
          ('count' in op
            ? count !== op.count || (count > 0 && !samePoint(map(effect.to, 'before'), end))
            : count !== 0) ||
          !sameHwpInlineBoundary(before, after, map, op.at, at, control)
        )
          return null
        if (op.action === 'replace') {
          next = { ...op, at }
          break
        }
        const mapped = op.field === null ? null : control(op.at, op.field)
        if (op.field !== null && (!mapped || !sameParagraph(mapped.at, at))) return null
        next = { ...op, at, field: mapped?.control ?? null }
      }
    }
    position(after, at, 'scalar')
    position(after, end, 'scalar')
    return parseHwpRichText(next)
  } catch {
    return null
  }
}

/** A single native atomic call cannot be retried as two splices around a concurrent insertion. */
export function mapHwpRichText(
  op: HwpRichTextOp,
  before: HwpDocument,
  after: HwpDocument,
  through: readonly HwpTextSplice[],
): HwpRichTextOp | null {
  if (through.length === 0) return op
  const effect = hwpRichTextEffect(before, op)
  if (!effect) return null
  const map: EndpointMap = (p, affinity) => mapHwpPosition(p, through, affinity)
  if (!effect.text && samePoint(effect.from, effect.to)) {
    const from = map(effect.from, 'after'),
      to = map(effect.to, 'before')
    return from && to && samePoint(from, to)
      ? remapHwpRichText(op, before, after, map, { ...effect, from, to })
      : null
  }
  const pending = rebaseHwpText(through, [effect])?.pending
  if (pending?.length !== 1) return null
  return remapHwpRichText(op, before, after, map, pending[0])
}
