import type { HwpDocument } from '@rhwp/core'
import { base64, imageExtension, MAX_IMAGE_BYTES } from './collab-structure'
import { parseHwpPosition, type HwpPosition, type HwpTextSplice } from './collab-text'

type Guard<T> = (v: unknown) => v is T
type Schema = Record<string, Guard<unknown>>
type Props<S extends Schema> = { [K in keyof S]?: S[K] extends Guard<infer V> ? V : never }
function record(v: unknown): v is Record<string, unknown> {
  return (
    v !== null &&
    typeof v === 'object' &&
    !Array.isArray(v) &&
    (Object.getPrototypeOf(v) === Object.prototype || Object.getPrototypeOf(v) === null)
  )
}
function exact(v: Record<string, unknown>, ...names: string[]): boolean {
  return Object.keys(v).length === names.length && names.every((k) => Object.hasOwn(v, k))
}
const int =
  (min: number, max: number): Guard<number> =>
  (v): v is number =>
    typeof v === 'number' && Number.isInteger(v) && v >= min && v <= max
const bool = (v: unknown): v is boolean => typeof v === 'boolean'
const one =
  <T extends string>(...values: T[]): Guard<T> =>
  (v): v is T =>
    values.some((s) => s === v)
const string =
  (max: number): Guard<string> =>
  (v): v is string =>
    typeof v === 'string' && v.length <= max && !/(?![\t\n])[\p{Cc}\p{Cs}]/u.test(v)
const index = int(0, 0x7fffffff),
  id = int(0, 0xffffffff),
  hu = int(-0x1000000, 0x1000000),
  size = int(1, 0x1000000)
const i8 = int(-128, 127),
  u8 = int(0, 255),
  i16 = int(-32768, 32767),
  u16 = int(0, 65535)
const color = (v: unknown): v is string => typeof v === 'string' && /^#[\da-f]{6}$/i.test(v)
const array =
  <T>(check: Guard<T>, count: number): Guard<T[]> =>
  (v): v is T[] =>
    Array.isArray(v) && v.length === count && Array.from(v).every(check)
const chars = {
  bold: bool,
  italic: bool,
  underline: bool,
  strikethrough: bool,
  subscript: bool,
  superscript: bool,
  emboss: bool,
  engrave: bool,
  kerning: bool,
  fontSize: int(1, 163800),
  textColor: color,
  shadeColor: color,
  underlineColor: color,
  strikeColor: color,
  shadowColor: color,
  underlineType: one('None', 'Bottom', 'Top'),
  outlineType: u8,
  shadowType: u8,
  shadowOffsetX: i8,
  shadowOffsetY: i8,
  emphasisDot: u8,
  underlineShape: u8,
  strikeShape: u8,
  ratios: array(u8, 7),
  spacings: array(i8, 7),
  relativeSizes: array(u8, 7),
  charOffsets: array(i8, 7),
}
type Tab = { position: number; type: number; fill: number }
const tab = (v: unknown): v is Tab =>
  record(v) &&
  exact(v, 'position', 'type', 'fill') &&
  int(0, 0x1000000)(v.position) &&
  int(0, 3)(v.type) &&
  u8(v.fill)
const paras = {
  alignment: one('left', 'right', 'center', 'justify', 'distribute', 'split', 'division'),
  lineSpacing: hu,
  lineSpacingType: one('Percent', 'Fixed', 'SpaceOnly', 'Minimum'),
  indent: hu,
  marginLeft: hu,
  marginRight: hu,
  spacingBefore: hu,
  spacingAfter: hu,
  headType: one('None', 'Outline', 'Number', 'Bullet'),
  paraLevel: int(0, 9),
  widowOrphan: bool,
  keepWithNext: bool,
  keepLines: bool,
  pageBreakBefore: bool,
  fontLineHeight: bool,
  singleLine: bool,
  autoSpaceKrEn: bool,
  autoSpaceKrNum: bool,
  verticalAlign: int(0, 3),
  englishBreakUnit: int(0, 2),
  koreanBreakUnit: int(0, 1),
  borderConnect: bool,
  borderIgnoreMargin: bool,
  borderSpacing: array(i16, 4),
  tabAutoLeft: bool,
  tabAutoRight: bool,
  tabStops: (v: unknown): v is Tab[] =>
    Array.isArray(v) && v.length <= 128 && Array.from(v).every(tab),
}
const pictures = {
  width: size,
  height: size,
  treatAsChar: bool,
  vertRelTo: one('Paper', 'Page', 'Para'),
  horzRelTo: one('Paper', 'Page', 'Column', 'Para'),
  vertAlign: one('Top', 'Center', 'Bottom'),
  horzAlign: one('Left', 'Center', 'Right'),
  textWrap: one('Square', 'Tight', 'Through', 'TopAndBottom', 'BehindText', 'InFrontOfText'),
  restrictInPage: bool,
  allowOverlap: bool,
  sizeProtect: bool,
  vertOffset: hu,
  horzOffset: hu,
  brightness: int(-100, 100),
  contrast: int(-100, 100),
  transparency: int(0, 100),
  effect: one('RealPic', 'GrayScale', 'BlackWhite', 'Pattern8x8'),
  rotationAngle: i16,
  horzFlip: bool,
  vertFlip: bool,
  cropLeft: hu,
  cropRight: hu,
  cropTop: hu,
  cropBottom: hu,
  paddingTop: i16,
  paddingBottom: i16,
  paddingRight: i16,
  paddingLeft: i16,
  outerMarginLeft: i16,
  outerMarginTop: i16,
  outerMarginRight: i16,
  outerMarginBottom: i16,
  borderColor: id,
  borderWidth: int(0, 65535),
  description: string(4096),
  hasCaption: bool,
  captionDirection: one('Left', 'Right', 'Top', 'Bottom'),
  captionVertAlign: one('Top', 'Center', 'Bottom'),
  captionWidth: size,
  captionSpacing: i16,
  captionIncludeMargin: bool,
}
const forms = { value: int(-0x80000000, 0x7fffffff), text: string(65536), caption: string(65536) }
function props<S extends Schema>(v: unknown, schema: S): v is Props<S> {
  if (!record(v)) return false
  const entries = Object.entries(v)
  return (
    entries.length > 0 &&
    entries.every(([k, value]) => Object.hasOwn(schema, k) && schema[k](value))
  )
}

type Fragment = { text: string; runs: [number, number][]; spaces: number[] }
const encoder = new TextEncoder()
function fragment(v: unknown): v is Fragment {
  if (
    !record(v) ||
    !exact(v, 'text', 'runs', 'spaces') ||
    !string(262144)(v.text) ||
    !v.text ||
    encoder.encode(v.text).length > 262144 ||
    !Array.isArray(v.runs) ||
    v.runs.length > 512 ||
    !Array.isArray(v.spaces) ||
    v.spaces.length > v.text.length
  )
    return false
  let count = 0,
    space = 0,
    breaks = 0
  for (const c of v.text) {
    if (c === '\n' && ++breaks > 127) return false
    if (v.spaces[space] === count) {
      if (c !== '\u00a0') return false
      space++
    }
    count++
  }
  if (space !== v.spaces.length || count * v.runs.length > 2_000_000) return false
  let previous = -1
  for (const run of v.runs) {
    if (
      !Array.isArray(run) ||
      run.length !== 2 ||
      !int(previous + 1, count - 1)(run[0]) ||
      !id(run[1])
    )
      return false
    previous = run[0]
  }
  return true
}

type ImageExtension = 'png' | 'jpg' | 'jpeg' | 'gif' | 'bmp' | 'webp'
type Addressed = { at: HwpPosition } & (
  | { action: 'characters'; end: number; props: Props<typeof chars> }
  | { action: 'paragraph'; props: Props<typeof paras> }
  | { action: 'picture'; control: number; props: Props<typeof pictures> }
  | {
      action: 'image'
      control: number
      data: string
      extension: ImageExtension
      widthPx: number
      heightPx: number
    }
  | { action: 'form'; control: number; props: Props<typeof forms> }
  | {
      action: 'table'
      rows: number
      cols: number
      inline: boolean
      slotsBefore: number
      widths: number[] | null
      heights: number[] | null
    }
  | {
      action: 'grid'
      control: number
      axis: 'row' | 'column'
      index: number
      side: 'before' | 'after' | 'delete'
    }
  | { action: 'pasteText'; fragment: Fragment; afterInline: boolean; field: number | null }
)
export type HwpCommandOp = { kind: 'command' } & (
  | Addressed
  | { action: 'memoText'; id: number; before: string; text: string }
  | { action: 'memoRemove'; id: number; before: string }
)

export function parseHwpCommand(v: unknown): HwpCommandOp | null {
  if (!record(v) || v.kind !== 'command') return null
  const { kind, action, control } = v
  const only = (...names: string[]) => exact(v, 'kind', 'action', ...names)
  if (action === 'memoRemove' || action === 'memoText') {
    if (!id(v.id) || !string(100000)(v.before)) return null
    if (action === 'memoRemove')
      return only('id', 'before') ? { kind, action, id: v.id, before: v.before } : null
    return only('id', 'before', 'text') && string(100000)(v.text)
      ? { kind, action, id: v.id, before: v.before, text: v.text }
      : null
  }
  const at = parseHwpPosition(v.at)
  if (!at) return null
  switch (action) {
    case 'characters':
      return only('at', 'end', 'props') &&
        index(v.end) &&
        v.end >= at.offset &&
        props(v.props, chars)
        ? { kind, action, at, end: v.end, props: v.props }
        : null
    case 'paragraph':
      return only('at', 'props') && !at.offset && props(v.props, paras)
        ? { kind, action, at, props: v.props }
        : null
    case 'form':
      return only('at', 'control', 'props') &&
        !at.offset &&
        at.path.length <= 1 &&
        index(control) &&
        props(v.props, forms)
        ? { kind, action, at, control, props: v.props }
        : null
    case 'picture':
      return only('at', 'control', 'props') &&
        !at.offset &&
        index(control) &&
        props(v.props, pictures)
        ? { kind, action, at, control, props: v.props }
        : null
    case 'image': {
      const { data, extension, widthPx, heightPx } = v
      return only('at', 'control', 'data', 'extension', 'widthPx', 'heightPx') &&
        !at.offset &&
        index(control) &&
        int(1, 65535)(widthPx) &&
        int(1, 65535)(heightPx) &&
        widthPx * heightPx <= 40_000_000 &&
        typeof data === 'string' &&
        imageExtension(extension, data)
        ? { kind, action, at, control, data, extension, widthPx, heightPx }
        : null
    }
    case 'table': {
      const { rows, cols, inline, slotsBefore, widths, heights } = v
      const extents = (a: unknown, n: number): a is number[] | null =>
        a === null || (array(size, n)(a) && a.reduce((sum, x) => sum + x, 0) <= 0x7fffffff)
      return only('at', 'rows', 'cols', 'inline', 'slotsBefore', 'widths', 'heights') &&
        int(1, 4096)(rows) &&
        int(1, 256)(cols) &&
        rows * cols <= 4096 &&
        bool(inline) &&
        int(0, 4096)(slotsBefore) &&
        (!inline || at.path.length > 0 || !slotsBefore) &&
        extents(widths, cols) &&
        extents(heights, rows)
        ? { kind, action, at, rows, cols, inline, slotsBefore, widths, heights }
        : null
    }
    case 'grid': {
      const { axis, side } = v
      return only('at', 'control', 'axis', 'index', 'side') &&
        !at.offset &&
        !at.path.length &&
        index(control) &&
        u16(v.index) &&
        (axis === 'row' || axis === 'column') &&
        (side === 'before' || side === 'after' || (axis === 'column' && side === 'delete'))
        ? { kind, action, at, control, axis, index: v.index, side }
        : null
    }
    case 'pasteText':
      return only('at', 'fragment', 'afterInline', 'field') &&
        bool(v.afterInline) &&
        (v.field === null || index(v.field)) &&
        (!at.path.length || (!v.afterInline && v.field === null)) &&
        fragment(v.fragment)
        ? { kind, action, at, fragment: v.fragment, afterInline: v.afterInline, field: v.field }
        : null
    default:
      return null
  }
}

// Studio uses JSON.stringify. Native legacy readers search the first key; JSON.parse alone would accept a different last duplicate.
function nativeJson(raw: unknown, max = 262144): unknown {
  if (typeof raw !== 'string' || raw.length > max) return undefined
  try {
    const v: unknown = JSON.parse(raw)
    return JSON.stringify(v) === raw ? v : undefined
  } catch {
    return undefined
  }
}

function memoBefore(doc: HwpDocument, memo: number): string | null {
  const found = (
    JSON.parse(doc.getMemoList()) as { id: number; editable: boolean; text: string }[]
  ).filter((m) => m.id === memo)
  return found.length === 1 && found[0].editable && string(100000)(found[0].text)
    ? found[0].text
    : null
}
type ClipboardEngine = {
  getCollabClipboardText(): string
  getCollabTextPosition(
    s: number,
    p: number,
    path: string,
    offset: number,
    axis: 'scalar' | 'caret',
  ): string
  pasteCollabClipboardText(
    s: number,
    p: number,
    path: string,
    offset: number,
    afterInline: boolean,
    field: number | undefined,
    fragment: string,
  ): string
}
function clipboardEngine(doc: HwpDocument): HwpDocument & ClipboardEngine {
  if (
    !('getCollabClipboardText' in doc) ||
    typeof doc.getCollabClipboardText !== 'function' ||
    !('getCollabTextPosition' in doc) ||
    typeof doc.getCollabTextPosition !== 'function' ||
    !('pasteCollabClipboardText' in doc) ||
    typeof doc.pasteCollabClipboardText !== 'function'
  )
    throw new Error('Missing HWP collaborative clipboard codec')
  return doc as HwpDocument & ClipboardEngine
}

export function findHwpMemo(
  doc: HwpDocument,
  memo: number,
): { at: HwpPosition; control: number; text: string } | null {
  const text = memoBefore(doc, memo)
  if (text === null) return null
  if (!('getCollabMemoAnchor' in doc) || typeof doc.getCollabMemoAnchor !== 'function')
    throw new Error('Missing HWP memo identity query')
  const v = JSON.parse(doc.getCollabMemoAnchor(memo))
  const at = parseHwpPosition({
    section: v.section,
    paragraph: v.paragraph,
    offset: 0,
    path: v.path.map(([controlIndex, cellIndex, cellParaIndex]: number[]) => ({
      controlIndex,
      cellIndex,
      cellParaIndex,
    })),
  })
  if (!at || !index(v.control)) throw new Error('Invalid HWP memo identity')
  return { at, control: v.control, text }
}

function tableSplit(doc: HwpDocument, at: HwpPosition, logical: number): [number, boolean[]] {
  if (!('getCollabTableSplit' in doc) || typeof doc.getCollabTableSplit !== 'function')
    throw new Error('Missing HWP split identity query')
  const value: unknown = JSON.parse(
    doc.getCollabTableSplit(at.section, at.paragraph, JSON.stringify(at.path), logical),
  )
  if (
    !Array.isArray(value) ||
    !index(value[0]) ||
    !Array.isArray(value[1]) ||
    value[1].length > 4096 ||
    !value[1].every(bool)
  )
    throw new Error('Invalid HWP table split')
  return [value[0], value[1]]
}

/** Inspect before native dispatch. Only publish after the native call succeeds. Unknown inputs do not authorize dispatch. */
export function captureHwpCommand(
  doc: HwpDocument,
  method: string,
  args: readonly unknown[],
): HwpCommandOp | null {
  const op = (obj: Record<string, unknown>) => parseHwpCommand({ kind: 'command', ...obj })
  if (method === 'setMemoText' || method === 'removeMemo') {
    const [memo, text] = args
    if (!id(memo) || args.length !== (method === 'setMemoText' ? 2 : 1)) return null
    const before = memoBefore(doc, memo)
    return method === 'setMemoText'
      ? op({ action: 'memoText', id: memo, before, text })
      : op({ action: 'memoRemove', id: memo, before })
  }
  let [section, paragraph] = args
  const at = (path: unknown = [], offset: unknown = 0) => ({ section, paragraph, path, offset })
  if (
    method === 'createTableEx' ||
    method === 'createTableInCellByPath' ||
    method === 'createTable'
  ) {
    const table = (
      path: unknown,
      logical: unknown,
      rows: unknown,
      cols: unknown,
      inline: unknown,
      widths: unknown,
      heights: unknown,
    ) => {
      const pos = parseHwpPosition(at(path, logical))
      if (!pos) return null
      const offset =
        !pos.path.length && inline === true ? pos.offset : tableSplit(doc, pos, pos.offset)[0]
      return op({
        action: 'table',
        at: { ...pos, offset },
        slotsBefore: pos.offset - offset,
        rows,
        cols,
        inline,
        widths,
        heights,
      })
    }
    if (method === 'createTable')
      return args.length === 5 ? table([], args[2], args[3], args[4], false, null, null) : null
    const nested = method === 'createTableInCellByPath'
    const options = nativeJson(args[nested ? 4 : 0])
    if (
      args.length !== (nested ? 5 : 1) ||
      !record(options) ||
      Object.keys(options).some(
        (k) =>
          ![
            'rowCount',
            'colCount',
            'treatAsChar',
            'colWidths',
            'rowHeights',
            ...(nested ? [] : ['sectionIdx', 'paraIdx', 'charOffset']),
          ].includes(k),
      )
    )
      return null
    if (Object.values(options).some((v) => v === null)) return null
    const path = nested ? nativeJson(args[2], 4096) : []
    if (nested && (!Array.isArray(path) || !path.length)) return null
    if (!nested) {
      section = options.sectionIdx ?? 0
      paragraph = options.paraIdx ?? 0
    }
    return table(
      path,
      nested ? args[3] : (options.charOffset ?? 0),
      options.rowCount ?? 2,
      options.colCount ?? 2,
      options.treatAsChar ?? nested,
      options.colWidths ?? null,
      options.rowHeights ?? null,
    )
  }
  if (
    method === 'insertTableRow' ||
    method === 'insertTableColumn' ||
    method === 'deleteTableColumn'
  ) {
    const remove = method === 'deleteTableColumn'
    if (args.length !== (remove ? 4 : 5) || (!remove && !bool(args[4]))) return null
    return op({
      action: 'grid',
      at: at(),
      control: args[2],
      axis: method === 'insertTableRow' ? 'row' : 'column',
      index: args[3],
      side: remove ? 'delete' : args[4] ? 'after' : 'before',
    })
  }
  if (method === 'assignPictureImage') {
    const [, , raw, control, bytes, widthPx, heightPx, extension] = args
    if (
      args.length !== 8 ||
      !ArrayBuffer.isView(bytes) ||
      Object.prototype.toString.call(bytes) !== '[object Uint8Array]' ||
      bytes.byteLength > MAX_IMAGE_BYTES
    )
      return null
    return op({
      action: 'image',
      at: at(raw === '' ? [] : nativeJson(raw, 4096)),
      control,
      data: base64(new Uint8Array(bytes.buffer, bytes.byteOffset, bytes.byteLength)),
      widthPx,
      heightPx,
      extension,
    })
  }
  const nested = method.endsWith('ByPath'),
    flat = method.endsWith('InCell')
  const path = nested
    ? nativeJson(args[2], 4096)
    : flat
      ? [{ controlIndex: args[2], cellIndex: args[3], cellParaIndex: args[4] }]
      : []
  if (nested && (!Array.isArray(path) || !path.length)) return null
  const i = nested ? 3 : flat ? 5 : 2
  switch (method) {
    case 'applyCharFormat':
    case 'applyCharFormatInCell':
    case 'applyCharFormatInCellByPath':
      return args.length === i + 3
        ? op({
            action: 'characters',
            at: at(path, args[i]),
            end: args[i + 1],
            props: nativeJson(args[i + 2]),
          })
        : null
    case 'applyParaFormat':
    case 'applyParaFormatInCell':
    case 'applyParaFormatInCellByPath':
      return args.length === i + 1
        ? op({ action: 'paragraph', at: at(path), props: nativeJson(args[i]) })
        : null
    case 'setPictureProperties':
    case 'setCellPicturePropertiesByPath':
    case 'setFormValue':
    case 'setFormValueInCell':
      return args.length === i + 2
        ? op({
            action: method.startsWith('setForm') ? 'form' : 'picture',
            at: at(path),
            control: args[i],
            props: nativeJson(args[i + 1]),
          })
        : null
    case 'pasteInternal':
    case 'pasteInternalTextLogical':
    case 'pasteInternalInCell':
    case 'pasteInternalInCellByPath': {
      if (
        args.length !== i + 1 ||
        !doc.clipboardIsSingleText() ||
        (nested && Array.isArray(path) && path.length < 2)
      )
        return null
      const pos = parseHwpPosition(at(path, args[i]))
      if (!pos) return null
      const native = clipboardEngine(doc)
      const loc = JSON.parse(
        native.getCollabTextPosition(
          pos.section,
          pos.paragraph,
          JSON.stringify(pos.path),
          pos.offset,
          method === 'pasteInternalTextLogical' ? 'caret' : 'scalar',
        ),
      )
      return op({
        action: 'pasteText',
        at: { ...pos, offset: loc.scalar },
        fragment: JSON.parse(native.getCollabClipboardText()),
        afterInline: loc.afterInline,
        field: pos.path.length ? null : loc.field,
      })
    }
    default:
      return null
  }
}

export function applyHwpCommand(doc: HwpDocument, op: HwpCommandOp): string {
  let result: string
  if (op.action === 'memoText' || op.action === 'memoRemove') {
    if (memoBefore(doc, op.id) !== op.before) throw new Error('HWP memo changed before replay')
    result = op.action === 'memoText' ? doc.setMemoText(op.id, op.text) : doc.removeMemo(op.id)
  } else {
    const { section: s, paragraph: p, path, offset } = op.at
    const json = JSON.stringify(path)
    if (s >= doc.getSectionCount() || p >= doc.getParagraphCount(s))
      throw new RangeError('Missing HWP command target')
    if (op.action === 'characters' || op.action === 'table') {
      const cell = path[0]
      const length =
        op.action === 'characters' && path.length === 1
          ? doc.getCellParagraphLength(s, p, cell.controlIndex, cell.cellIndex, cell.cellParaIndex)
          : path.length
            ? doc.getCellParagraphLengthByPath(s, p, json)
            : doc.getParagraphLength(s, p)
      if ((op.action === 'characters' ? op.end : offset) > length)
        throw new RangeError('HWP command outside paragraph')
    }
    switch (op.action) {
      case 'characters':
        result = path.length
          ? doc.applyCharFormatInCellByPath(s, p, json, offset, op.end, JSON.stringify(op.props))
          : doc.applyCharFormat(s, p, offset, op.end, JSON.stringify(op.props))
        break
      case 'paragraph':
        result = path.length
          ? doc.applyParaFormatInCellByPath(s, p, json, JSON.stringify(op.props))
          : doc.applyParaFormat(s, p, JSON.stringify(op.props))
        break
      case 'picture':
        result = path.length
          ? doc.setCellPicturePropertiesByPath(s, p, json, op.control, JSON.stringify(op.props))
          : doc.setPictureProperties(s, p, op.control, JSON.stringify(op.props))
        break
      case 'image':
        result = doc.assignPictureImage(
          s,
          p,
          json,
          op.control,
          Uint8Array.from(atob(op.data), (c) => c.charCodeAt(0)),
          op.widthPx,
          op.heightPx,
          op.extension,
        )
        break
      case 'form': {
        const cell = path[0]
        result = cell
          ? doc.setFormValueInCell(
              s,
              p,
              cell.controlIndex,
              cell.cellIndex,
              cell.cellParaIndex,
              op.control,
              JSON.stringify(op.props),
            )
          : doc.setFormValue(s, p, op.control, JSON.stringify(op.props))
        break
      }
      case 'table': {
        const options = {
          rowCount: op.rows,
          colCount: op.cols,
          treatAsChar: op.inline,
          colWidths: op.widths ?? undefined,
          rowHeights: op.heights ?? undefined,
        }
        const logical = offset + op.slotsBefore
        if ((path.length || !op.inline) && tableSplit(doc, op.at, logical)[0] !== offset)
          throw new Error('HWP table split slot no longer matches its text')
        result = path.length
          ? doc.createTableInCellByPath(s, p, json, logical, JSON.stringify(options))
          : doc.createTableEx(
              JSON.stringify({ sectionIdx: s, paraIdx: p, charOffset: logical, ...options }),
            )
        break
      }
      case 'grid': {
        const { rowCount, colCount, cellCount } = JSON.parse(
          doc.getTableDimensions(s, p, op.control),
        )
        const rows = op.axis === 'row',
          count = rows ? rowCount : colCount
        if (
          !int(1, 65535)(rowCount) ||
          !int(1, 65535)(colCount) ||
          !int(1, 4096)(cellCount) ||
          rowCount * colCount > 4096 ||
          op.index >= count ||
          (op.side === 'delete' ? count < 2 : (count + 1) * (rows ? colCount : rowCount) > 4096)
        )
          throw new RangeError('Invalid HWP grid edit')
        result =
          op.side === 'delete'
            ? doc.deleteTableColumn(s, p, op.control, op.index)
            : rows
              ? doc.insertTableRow(s, p, op.control, op.index, op.side === 'after')
              : doc.insertTableColumn(s, p, op.control, op.index, op.side === 'after')
        break
      }
      case 'pasteText':
        result = clipboardEngine(doc).pasteCollabClipboardText(
          s,
          p,
          json,
          offset,
          op.afterInline,
          op.field ?? undefined,
          JSON.stringify(op.fragment),
        )
        break
    }
  }
  if (JSON.parse(result)?.ok !== true) throw new Error('HWP rejected the command')
  return result
}

/** Mapping changes coordinates only. The caller owns range-overlap, resource and competing grid conflicts. */
export function mapHwpCommand(
  op: HwpCommandOp,
  point: (at: HwpPosition, affinity: 'before' | 'after') => HwpPosition | null,
  control: (host: HwpPosition, ordinal: number) => { at: HwpPosition; control: number } | null,
): HwpCommandOp | null {
  if (!('at' in op)) return op
  if ('control' in op) {
    const next = control(op.at, op.control)
    return next && { ...op, at: { ...next.at, offset: 0 }, control: next.control }
  }
  const at = point(op.at, 'after')
  if (!at) return null
  if (op.action === 'characters') {
    const end = point({ ...op.at, offset: op.end }, op.end === op.at.offset ? 'after' : 'before')
    if (
      !end ||
      at.section !== end.section ||
      at.paragraph !== end.paragraph ||
      JSON.stringify(at.path) !== JSON.stringify(end.path) ||
      end.offset < at.offset
    )
      return null
    return { ...op, at, end: end.offset }
  }
  if (op.action === 'pasteText' && op.field !== null) {
    const target = control(op.at, op.field)
    if (
      !target ||
      target.at.section !== at.section ||
      target.at.paragraph !== at.paragraph ||
      JSON.stringify(target.at.path) !== JSON.stringify(at.path)
    )
      return null
    return { ...op, at, field: target.control }
  }
  return { ...op, at: op.action === 'paragraph' ? { ...at, offset: 0 } : at }
}

/** Coordinate effects only. A pasted LF is a line break inside its paragraph, never a paragraph split. */
export function hwpCommandTextEffect(op: HwpCommandOp): HwpTextSplice[] {
  if (op.action !== 'pasteText') return []
  const effects: HwpTextSplice[] = []
  let at = op.at
  for (const part of op.fragment.text.split(/(\n)/)) {
    if (!part) continue
    effects.push(
      part === '\n'
        ? { kind: 'lineBreak', from: at, to: at, text: part }
        : { kind: 'splice', from: at, to: at, text: part },
    )
    at = { ...at, offset: at.offset + [...part].length }
  }
  return effects
}

type Axis = 'row' | 'column'
type Affinity = 'before' | 'after'
type TableOp = Extract<HwpCommandOp, { action: 'table' }>
type GridOp = Extract<HwpCommandOp, { action: 'grid' }>
type Cell = { row: number; col: number; rowSpan: number; colSpan: number }
type Change =
  | { type: 'identity' }
  | { type: 'paste'; at: HwpPosition; count: number }
  | { type: 'memo' | 'caption'; at: HwpPosition; control: number; added: boolean }
  | {
      type: 'grid'
      op: GridOp
      pivot: number
      forward: Int32Array
      reverse: Int32Array
      cells: Cell[]
      byOrigin: Map<number, number>
    }
  | {
      type: 'table'
      op: TableOp
      host: number
      table: number
      ci: number
      delta: number
      split: boolean
      left: number[]
      right: number[]
      ordinals: number[]
    }

export interface HwpCommandEffect {
  change: Change
  mapPosition(at: HwpPosition, affinity: Affinity): HwpPosition | null
  unmapPosition(at: HwpPosition, affinity: Affinity): HwpPosition | null
  mapGrid(host: HwpPosition, control: number, axis: Axis, index: number): number | null
  unmapGrid(host: HwpPosition, control: number, axis: Axis, index: number): number | null
  mapCreatedPosition(
    at: HwpPosition,
    counterpart: HwpCommandEffect,
    mapPerpendicular?: (i: number) => number | null,
  ): HwpPosition | null
  mapCreatedGrid(
    host: HwpPosition,
    control: number,
    axis: Axis,
    index: number,
    counterpart: HwpCommandEffect,
  ): number | null
}

const paraAt = (p: HwpPosition, depth: number) =>
  depth ? p.path[depth - 1].cellParaIndex : p.paragraph
function withinList(p: HwpPosition, host: HwpPosition): boolean {
  return (
    p.section === host.section &&
    p.path.length >= host.path.length &&
    host.path.every(
      (s, i) =>
        paraAt(p, i) === paraAt(host, i) &&
        p.path[i].controlIndex === s.controlIndex &&
        p.path[i].cellIndex === s.cellIndex,
    )
  )
}
function withinHost(p: HwpPosition, h: HwpPosition): boolean {
  return withinList(p, h) && paraAt(p, h.path.length) === paraAt(h, h.path.length)
}
function inPara(p: HwpPosition, depth: number, n: number, offset = p.offset): HwpPosition {
  if (!depth) return { ...p, paragraph: n, offset }
  const path = p.path.slice()
  path[depth - 1] = { ...path[depth - 1], cellParaIndex: n }
  return { ...p, path, offset }
}
function step(
  p: HwpPosition,
  depth: number,
  patch: Partial<HwpPosition['path'][number]>,
): HwpPosition {
  const path = p.path.slice()
  path[depth] = { ...path[depth], ...patch }
  return { ...p, path }
}
function tablePosition(
  c: Extract<Change, { type: 'table' }>,
  p: HwpPosition,
  affinity: Affinity,
  reverse: boolean,
): HwpPosition | null {
  const { op, host, table, ci, delta, split } = c
  if (!withinList(p, op.at)) return p
  const d = op.at.path.length,
    n = paraAt(p, d),
    child = p.path[d],
    cut = op.at.offset
  if (!delta) return reverse && n === table && child?.controlIndex === ci ? null : p // Body-inline appends a control.
  if (n < host) return p
  if (n > host + (reverse ? delta : 0)) return inPara(p, d, n + (reverse ? -delta : delta))
  if (split) {
    const tail = host + delta
    if (reverse) {
      if (n !== host && n !== tail) return null
      if (n === table) return null
      if (!child) return inPara(p, d, host, p.offset + (n === tail ? cut : 0))
      const ci = (n === tail ? c.right : c.left)[child.controlIndex]
      return ci === undefined ? null : step(inPara(p, d, host), d, { controlIndex: ci })
    }
    if (child) {
      const dest = c.ordinals[child.controlIndex]
      if (dest === undefined) return null
      return step(inPara(p, d, dest < 0 ? tail : host), d, {
        controlIndex: dest < 0 ? ~dest : dest,
      })
    }
    if (host === table || p.offset > cut || (p.offset === cut && affinity === 'after'))
      return inPara(p, d, tail, p.offset - cut)
    return p
  }
  if (delta === 1) {
    // An empty body host was reused; its existing structural controls survive.
    if (reverse && n === host + 1)
      return !child && !p.offset && affinity === 'after' ? inPara(p, d, host) : null
    if (reverse && child?.controlIndex === ci) return null
    return !reverse && !child && affinity === 'after' ? inPara(p, d, host + 1) : p
  }
  const oldAfter = table === host ? host + delta : host
  return reverse ? (n === oldAfter ? inPara(p, d, host) : null) : inPara(p, d, oldAfter)
}
function positionThrough(
  c: Change,
  p: HwpPosition,
  affinity: Affinity,
  reverse: boolean,
): HwpPosition | null {
  if (c.type === 'identity') return p
  if (c.type === 'table') return tablePosition(c, p, affinity, reverse)
  const at = c.type === 'grid' ? c.op.at : c.at
  if (!withinHost(p, at)) return p
  const d = at.path.length,
    child = p.path[d]
  if (c.type === 'paste') {
    if (child || p.offset < at.offset) return p
    if (reverse)
      return p.offset === at.offset
        ? p
        : p.offset < at.offset + c.count
          ? null
          : { ...p, offset: p.offset - c.count }
    return p.offset === at.offset && affinity === 'before'
      ? p
      : { ...p, offset: p.offset + c.count }
  }
  if (!child) return p
  if (c.type === 'grid') {
    if (child.controlIndex !== c.op.control || child.cellIndex === 65534) return p
    const cell = (reverse ? c.reverse : c.forward)[child.cellIndex]
    return cell === undefined || cell < 0 ? null : step(p, d, { cellIndex: cell })
  }
  if (c.type === 'caption')
    return child.controlIndex === c.control && reverse === c.added ? null : p
  if (reverse)
    return child.controlIndex >= c.control
      ? step(p, d, { controlIndex: child.controlIndex + 1 })
      : p
  return child.controlIndex === c.control
    ? null
    : child.controlIndex > c.control
      ? step(p, d, { controlIndex: child.controlIndex - 1 })
      : p
}
function gridThrough(
  c: Change,
  host: HwpPosition,
  control: number,
  axis: Axis,
  i: number,
  reverse: boolean,
): number | null {
  if (
    c.type !== 'grid' ||
    c.op.axis !== axis ||
    host.path.length !== 0 ||
    !withinHost(host, c.op.at) ||
    control !== c.op.control ||
    i < c.pivot
  )
    return i
  const removed = (c.op.side === 'delete') !== reverse
  return removed && i === c.pivot ? null : i + (removed ? -1 : 1)
}
function created(
  c: Change,
  p: HwpPosition,
  target: Change,
  perp?: (i: number) => number | null,
): HwpPosition | null {
  if (
    c.type === 'paste' &&
    target.type === 'paste' &&
    withinHost(p, c.at) &&
    p.path.length === c.at.path.length &&
    p.offset >= c.at.offset &&
    p.offset <= c.at.offset + c.count &&
    target.count === c.count
  ) {
    return { ...target.at, offset: target.at.offset + p.offset - c.at.offset }
  }
  if (c.type === 'table' && target.type === 'table' && withinList(p, c.op.at)) {
    const d = c.op.at.path.length,
      n = paraAt(p, d),
      ch = p.path[d]
    const td = target.op.at.path.length
    if (n === c.table && ch?.controlIndex === c.ci) {
      const base = inPara(target.op.at, td, target.table)
      return {
        ...base,
        offset: p.offset,
        path: [...base.path, { ...ch, controlIndex: target.ci }, ...p.path.slice(d + 1)],
      }
    }
    if (
      !ch &&
      ((c.delta > 0 && n === c.table && target.delta > 0) ||
        (d === 0 && td === 0 && c.delta > 0 && target.delta > 0 && n === c.table + 1))
    ) {
      return inPara(target.op.at, td, target.table + n - c.table, p.offset)
    }
  }
  if (
    c.type === 'grid' &&
    target.type === 'grid' &&
    perp &&
    c.op.axis === target.op.axis &&
    c.op.side !== 'delete' &&
    target.op.side !== 'delete' &&
    withinHost(p, c.op.at)
  ) {
    const child = p.path[0]
    if (child?.controlIndex !== c.op.control || c.reverse[child.cellIndex] !== -1) return null
    const old = c.cells[child.cellIndex],
      row = c.op.axis === 'row',
      other = perp(row ? old.col : old.row)
    if (other === null) return null
    const ci = target.byOrigin.get(
      row ? target.pivot * 65536 + other : other * 65536 + target.pivot,
    )
    if (ci === undefined || target.reverse[ci] !== -1) return null
    const next = target.cells[ci]
    if (old.rowSpan !== next.rowSpan || old.colSpan !== next.colSpan) return null
    return {
      ...target.op.at,
      offset: p.offset,
      path: [{ ...child, controlIndex: target.op.control, cellIndex: ci }, ...p.path.slice(1)],
    }
  }
  if (
    c.type === 'caption' &&
    c.added &&
    target.type === 'caption' &&
    target.added &&
    withinHost(p, c.at) &&
    p.path[c.at.path.length]?.controlIndex === c.control
  ) {
    const suffix = p.path.slice(c.at.path.length)
    return {
      ...target.at,
      offset: p.offset,
      path: [...target.at.path, { ...suffix[0], controlIndex: target.control }, ...suffix.slice(1)],
    }
  }
  return null
}
function effect(change: Change): HwpCommandEffect {
  return {
    change,
    mapPosition: (p, a) => positionThrough(change, p, a, false),
    unmapPosition: (p, a) => positionThrough(change, p, a, true),
    mapGrid: (h, c, a, i) => gridThrough(change, h, c, a, i, false),
    unmapGrid: (h, c, a, i) => gridThrough(change, h, c, a, i, true),
    mapCreatedPosition: (p, target, perp) => created(change, p, target.change, perp),
    mapCreatedGrid: (h, ctrl, axis, i, target) => {
      if (
        change.type === 'grid' &&
        target.change.type === 'grid' &&
        change.op.axis === axis &&
        target.change.op.axis === axis &&
        change.op.side !== 'delete' &&
        target.change.op.side !== 'delete' &&
        h.path.length === 0 &&
        withinHost(h, change.op.at) &&
        ctrl === change.op.control &&
        i === change.pivot
      )
        return target.change.pivot
      return null
    },
  }
}
function gridCells(doc: HwpDocument, op: GridOp): Cell[] {
  const { section, paragraph } = op.at,
    { control } = op
  const { rowCount, colCount, cellCount } = JSON.parse(
    doc.getTableDimensions(section, paragraph, control),
  )
  if (
    !int(1, 4096)(cellCount) ||
    !int(1, 65535)(rowCount) ||
    !int(1, 65535)(colCount) ||
    rowCount * colCount > 4096
  )
    throw new Error('Oversized HWP grid')
  return Array.from({ length: cellCount }, (_, i) => {
    const { row, col, rowSpan, colSpan } = JSON.parse(
      doc.getCellInfo(section, paragraph, control, i),
    )
    if (
      !int(0, rowCount - 1)(row) ||
      !int(0, colCount - 1)(col) ||
      !int(1, rowCount - row)(rowSpan) ||
      !int(1, colCount - col)(colSpan)
    )
      throw new Error('Invalid HWP cell')
    return { row, col, rowSpan, colSpan }
  })
}

/** Observe exactly one successful native call. Effects stay local and are not valid wire operands. */
export function captureHwpCommandEffect(
  doc: HwpDocument,
  op: HwpCommandOp,
): (result: string) => HwpCommandEffect {
  if (op.action === 'grid') {
    const before = gridCells(doc, op),
      pivot = op.index + Number(op.side === 'after')
    return () => {
      const cells = gridCells(doc, op),
        byOrigin = new Map(cells.map((c, i) => [c.row * 65536 + c.col, i]))
      if (byOrigin.size !== cells.length) throw new Error('Duplicate HWP grid origin')
      const forward = new Int32Array(before.length).fill(-1),
        reverse = new Int32Array(cells.length).fill(-1)
      before.forEach((cell, old) => {
        const r = op.axis === 'row',
          a = r ? cell.row : cell.col,
          span = r ? cell.rowSpan : cell.colSpan,
          removed = op.side === 'delete'
        if (removed && a === pivot && span === 1) return
        const dest = removed ? a - Number(a > pivot) : a + Number(a >= pivot)
        const newSpan =
          span +
          (removed
            ? -Number(a <= pivot && pivot < a + span)
            : Number(a < pivot && pivot < a + span))
        const i = byOrigin.get(r ? dest * 65536 + cell.col : cell.row * 65536 + dest)
        if (
          i === undefined ||
          reverse[i] !== -1 ||
          cells[i].rowSpan !== (r ? newSpan : cell.rowSpan) ||
          cells[i].colSpan !== (r ? cell.colSpan : newSpan)
        )
          throw new Error('HWP grid lost a surviving cell')
        forward[old] = i
        reverse[i] = old
      })
      return effect({ type: 'grid', op, cells, pivot, byOrigin, forward, reverse })
    }
  }
  if (op.action === 'table') {
    const { section, paragraph, path, offset } = op.at,
      d = path.length,
      host = paraAt(op.at, d)
    const count = () =>
      d
        ? doc.getCellParagraphCountByPath(section, paragraph, JSON.stringify(path))
        : doc.getParagraphCount(section)
    const before = count(),
      moves = d || !op.inline ? tableSplit(doc, op.at, offset + op.slotsBefore)[1] : []
    return (result) => {
      const reply = JSON.parse(result),
        table = d ? reply.cellParaIdx : reply.paraIdx,
        ci = reply.controlIdx,
        delta = count() - before
      if (
        !index(ci) ||
        !int(host, host + 1)(table) ||
        (d
          ? delta !== table - host + 1
          : op.inline
            ? delta !== 0 || table !== host
            : !(
                (delta === 1 && table === host) ||
                delta === 2 ||
                (delta === 3 && table === host + 1)
              ))
      )
        throw new Error('Unexpected HWP table placement')
      const split = d > 0 || delta === 3,
        left: number[] = [],
        right: number[] = []
      const ordinals = split
        ? moves.map((toRight, old) => (toRight ? ~(right.push(old) - 1) : left.push(old) - 1))
        : []
      return effect({ type: 'table', op, host, table, ci, delta, split, ordinals, left, right })
    }
  }
  let change: Change = { type: 'identity' }
  if (op.action === 'pasteText')
    change = { type: 'paste', at: op.at, count: [...op.fragment.text].length }
  if (op.action === 'memoRemove') {
    const hit = findHwpMemo(doc, op.id)
    if (!hit) throw new Error('HWP memo was removed')
    change = { type: 'memo', at: hit.at, control: hit.control, added: false }
  }
  if (op.action === 'picture' && op.props.hasCaption !== undefined) {
    const { section: s, paragraph: p, path } = op.at
    const props = JSON.parse(
      path.length
        ? doc.getCellPicturePropertiesByPath(s, p, JSON.stringify(path), op.control)
        : doc.getPictureProperties(s, p, op.control),
    )
    if (!bool(props.hasCaption)) throw new Error('Missing HWP caption state')
    if (props.hasCaption !== op.props.hasCaption)
      change = { type: 'caption', at: op.at, control: op.control, added: op.props.hasCaption }
  }
  return () => effect(change)
}

/** Use before replaying shape references from an optimistic model against the reordered model. */
export function mapHwpCharShapeIds(
  source: HwpDocument,
  target: HwpDocument,
  ids: readonly number[],
): number[] | null {
  if (ids.length > 512 || !Array.from(ids).every(id)) return null
  if (!('getCollabCharShapeMap' in source) || typeof source.getCollabCharShapeMap !== 'function')
    throw new Error('Missing HWP shape identity query')
  const mapped = source.getCollabCharShapeMap(target, Uint32Array.from(ids))
  if (
    !ArrayBuffer.isView(mapped) ||
    Object.prototype.toString.call(mapped) !== '[object Int32Array]'
  )
    throw new Error('Invalid HWP shape identity response')
  const result = Array.from(new Int32Array(mapped.buffer, mapped.byteOffset, mapped.byteLength / 4))
  return result.length === ids.length && result.every(index) ? result : null
}
