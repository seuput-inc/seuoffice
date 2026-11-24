import type { HwpDocument } from '@rhwp/core'
import {
  parseHwpPosition,
  sameHwpContainer,
  type HwpPosition,
  type HwpTextSplice,
} from './collab-text'

type DeleteRow = {
  kind: 'deleteRow'
  section: number
  paragraph: number
  control: number
  row: number
}
type DeleteParagraph = { kind: 'deleteParagraph'; section: number; paragraph: number }
type CellPicture = {
  kind: 'insertCellPicture'
  at: HwpPosition
  anchor: 'paper' | 'cell'
  data: string
  extension: 'png' | 'jpeg' | 'jpg' | 'gif' | 'bmp' | 'webp'
  description: string
  width: number
  height: number
  naturalWidthPx: number
  naturalHeightPx: number
  offsetXHu: number | null
  offsetYHu: number | null
}
export type HwpStructureOp = DeleteRow | DeleteParagraph | CellPicture

/** Local observations of one successful native mutation. Never accept these maps from the wire. */
export type HwpStructureEffect = { result: string } & (
  | (DeleteRow & { cells: Int32Array })
  | DeleteParagraph
  | (Pick<CellPicture, 'kind' | 'at' | 'anchor'> & { control: number })
)

export const MAX_IMAGE_BYTES = 5 * 1024 * 1024 // Base64 and the enclosing change must fit the 8 MiB frame.
const MAX_INDEX = 0x7fffffff
function integer(n: unknown, min = 0, max = MAX_INDEX): n is number {
  return typeof n === 'number' && Number.isInteger(n) && n >= min && n <= max
}
function fields(obj: object, keys: readonly string[]): boolean {
  return Object.keys(obj).length === keys.length && keys.every((key) => Object.hasOwn(obj, key))
}
function offset(n: unknown): n is number | null {
  return n === null || integer(n, -0x80000000)
}

export function base64(bytes: Uint8Array): string {
  let binary = ''
  for (let i = 0; i < bytes.length; i += 8192)
    binary += String.fromCharCode(...bytes.subarray(i, i + 8192))
  return btoa(binary)
}

// Inspect the encoded image's dimensions, not just the sender's naturalWidthPx/naturalHeightPx.
export function imageExtension(ext: unknown, data: string): ext is CellPicture['extension'] {
  if (
    data.length < 16 ||
    data.length > 4 * Math.ceil(MAX_IMAGE_BYTES / 3) ||
    data.length % 4 !== 0 ||
    /[^A-Za-z0-9+/=]/.test(data)
  )
    return false
  const pad = data.endsWith('==') ? 2 : data.endsWith('=') ? 1 : 0
  const length = (data.length / 4) * 3 - pad
  if (length > MAX_IMAGE_BYTES || data.indexOf('=') !== (pad ? data.length - pad : -1)) return false
  if (pad && btoa(atob(data.slice(-4))) !== data.slice(-4)) return false
  const head = atob(data.slice(0, 48))
  const bytes = Uint8Array.from(head, (c) => c.charCodeAt(0))
  const view = new DataView(bytes.buffer)
  let w: number, h: number
  switch (ext) {
    case 'png':
      if (length < 45 || !head.startsWith('\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR')) return false
      w = view.getUint32(16)
      h = view.getUint32(20)
      break
    case 'gif':
      if (length < 14 || (!head.startsWith('GIF87a') && !head.startsWith('GIF89a'))) return false
      w = view.getUint16(6, true)
      h = view.getUint16(8, true)
      break
    case 'bmp': {
      if (length < 26 || !head.startsWith('BM')) return false
      const dib = view.getUint32(14, true)
      if (dib !== 12 && (dib < 40 || dib > length - 14)) return false
      w = dib === 12 ? view.getUint16(18, true) : view.getInt32(18, true)
      h = dib === 12 ? view.getUint16(20, true) : Math.abs(view.getInt32(22, true))
      break
    }
    case 'webp': {
      if (
        length < 25 ||
        !head.startsWith('RIFF') ||
        head.slice(8, 12) !== 'WEBP' ||
        view.getUint32(4, true) !== length - 8
      )
        return false
      const chunk = head.slice(12, 16)
      if (chunk === 'VP8X' && length >= 30) {
        w = 1 + bytes[24] + (bytes[25] << 8) + (bytes[26] << 16)
        h = 1 + bytes[27] + (bytes[28] << 8) + (bytes[29] << 16)
      } else if (chunk === 'VP8L' && bytes[20] === 0x2f) {
        const bits = view.getUint32(21, true)
        w = (bits & 0x3fff) + 1
        h = ((bits >>> 14) & 0x3fff) + 1
      } else if (chunk === 'VP8 ' && length >= 30 && head.slice(23, 26) === '\x9d\x01\x2a') {
        w = view.getUint16(26, true) & 0x3fff
        h = view.getUint16(28, true) & 0x3fff
      } else return false
      break
    }
    case 'jpg':
    case 'jpeg': {
      if (!head.startsWith('\xff\xd8\xff')) return false
      const jpeg = atob(data)
      let i = 2
      for (;;) {
        if (i + 4 > length || jpeg.charCodeAt(i++) !== 0xff) return false
        while (jpeg.charCodeAt(i) === 0xff) i++
        const marker = jpeg.charCodeAt(i++)
        if (marker === 0xda || marker === 0xd9 || i + 2 > length) return false
        if (marker === 1 || (marker >= 0xd0 && marker <= 0xd7)) continue
        const size = (jpeg.charCodeAt(i) << 8) + jpeg.charCodeAt(i + 1)
        if (size < 2 || size > length - i) return false
        if (
          marker >= 0xc0 &&
          marker <= 0xcf &&
          marker !== 0xc4 &&
          marker !== 0xc8 &&
          marker !== 0xcc
        ) {
          if (size < 8) return false
          h = (jpeg.charCodeAt(i + 3) << 8) + jpeg.charCodeAt(i + 4)
          w = (jpeg.charCodeAt(i + 5) << 8) + jpeg.charCodeAt(i + 6)
          break
        }
        i += size
      }
      break
    }
    default:
      return false
  }
  return w > 0 && h > 0 && w <= 65535 && h <= 65535 && w * h <= 40_000_000
}

export function parseHwpStructure(value: unknown): HwpStructureOp | null {
  if (typeof value !== 'object' || !value || Array.isArray(value)) return null
  const v = value as Record<string, unknown>
  const { kind, section, paragraph, control, row } = v
  if (kind === 'deleteRow') {
    return fields(v, ['kind', 'section', 'paragraph', 'control', 'row']) &&
      integer(section) &&
      integer(paragraph) &&
      integer(control) &&
      integer(row, 0, 0xffff)
      ? { kind, section, paragraph, control, row }
      : null
  }
  if (kind === 'deleteParagraph') {
    return fields(v, ['kind', 'section', 'paragraph']) && integer(section) && integer(paragraph)
      ? { kind, section, paragraph }
      : null
  }
  if (
    kind !== 'insertCellPicture' ||
    !fields(v, [
      'kind',
      'at',
      'anchor',
      'data',
      'extension',
      'description',
      'width',
      'height',
      'naturalWidthPx',
      'naturalHeightPx',
      'offsetXHu',
      'offsetYHu',
    ])
  )
    return null
  const {
    anchor,
    data,
    extension,
    description,
    width,
    height,
    naturalWidthPx,
    naturalHeightPx,
    offsetXHu,
    offsetYHu,
  } = v
  const at = parseHwpPosition(v.at)
  if (
    !at ||
    (anchor !== 'cell' && anchor !== 'paper') ||
    (anchor === 'cell' && !at.path.length) ||
    typeof data !== 'string' ||
    typeof description !== 'string' ||
    description.length > 4096 ||
    /[\u0000-\u0008\u000b\u000c\u000e-\u001f\ud800-\udfff]/u.test(description) ||
    !integer(width, 1, 0x1000000) ||
    !integer(height, 1, 0x1000000) ||
    !integer(naturalWidthPx, 1, 65535) ||
    !integer(naturalHeightPx, 1, 65535) ||
    naturalWidthPx * naturalHeightPx > 40_000_000 ||
    !offset(offsetXHu) ||
    !offset(offsetYHu) ||
    !imageExtension(extension, data)
  )
    return null
  return {
    kind,
    at,
    anchor,
    data,
    extension,
    description,
    width,
    height,
    naturalWidthPx,
    naturalHeightPx,
    offsetXHu,
    offsetYHu,
  }
}

function checkTarget(doc: HwpDocument, op: HwpStructureOp): void {
  if (op.kind === 'insertCellPicture') {
    const { section, paragraph, path, offset } = op.at
    if (path.length) {
      const json = JSON.stringify(path)
      doc.getCellInfoByPath(section, paragraph, json)
      if (offset > doc.getCellParagraphLengthByPath(section, paragraph, json))
        throw new RangeError('Picture outside cell text')
    } else if (
      paragraph >= doc.getParagraphCount(section) ||
      offset > doc.getParagraphLength(section, paragraph)
    ) {
      throw new RangeError('Picture outside body text')
    }
  } else if (op.kind === 'deleteParagraph') {
    const count = doc.getParagraphCount(op.section)
    if (count <= 1 || op.paragraph >= count)
      throw new RangeError('Cannot delete this HWP paragraph')
  } else {
    const { rowCount } = JSON.parse(doc.getTableDimensions(op.section, op.paragraph, op.control))
    if (!integer(rowCount, 2, 65535) || op.row >= rowCount)
      throw new RangeError('Cannot delete this HWP row')
  }
}

/** Inspect a Studio call BEFORE execution. A null result does not authorize invoking the unknown method. */
export function captureHwpStructure(
  doc: HwpDocument,
  method: string,
  args: readonly unknown[],
): HwpStructureOp | null {
  let value: unknown
  const [section, paragraph] = args
  if (method === 'deleteTableRow' && args.length === 4) {
    value = { kind: 'deleteRow', section, paragraph, control: args[2], row: args[3] }
  } else if (method === 'deleteParagraph' && args.length === 2) {
    value = { kind: 'deleteParagraph', section, paragraph }
  } else if (
    (method === 'insertPictureInCell' || method === 'insertPicture') &&
    args.length >= 11 &&
    args.length <= 13
  ) {
    const [
      ,
      ,
      atOffset,
      json,
      bytes,
      width,
      height,
      naturalWidthPx,
      naturalHeightPx,
      extension,
      description,
      x,
      y,
    ] = args
    if (
      typeof json !== 'string' ||
      json.length > 4096 ||
      !ArrayBuffer.isView(bytes) ||
      Object.prototype.toString.call(bytes) !== '[object Uint8Array]' ||
      !bytes.byteLength ||
      bytes.byteLength > MAX_IMAGE_BYTES
    )
      return null
    let path: unknown
    try {
      path = json === '' ? [] : JSON.parse(json)
    } catch {
      return null
    }
    value = {
      kind: 'insertCellPicture',
      at: { section, paragraph, path, offset: atOffset },
      anchor: method === 'insertPictureInCell' ? 'cell' : 'paper',
      data: base64(new Uint8Array(bytes.buffer, bytes.byteOffset, bytes.byteLength)),
      width,
      height,
      naturalWidthPx,
      naturalHeightPx,
      extension,
      description,
      offsetXHu: x ?? null,
      offsetYHu: y ?? null,
    }
  } else return null
  const op = parseHwpStructure(value)
  if (op) checkTarget(doc, op)
  return op
}

function picture(doc: HwpDocument, op: CellPicture): string {
  const { section, paragraph, path, offset } = op.at
  const raw = atob(op.data)
  const bytes = new Uint8Array(raw.length)
  for (let i = 0; i < raw.length; i++) bytes[i] = raw.charCodeAt(i)
  const args: Parameters<HwpDocument['insertPictureInCell']> = [
    section,
    paragraph,
    offset,
    JSON.stringify(path),
    bytes,
    op.width,
    op.height,
    op.naturalWidthPx,
    op.naturalHeightPx,
    op.extension,
    op.description,
    op.offsetXHu,
    op.offsetYHu,
  ]
  return op.anchor === 'cell' ? doc.insertPictureInCell(...args) : doc.insertPicture(...args)
}

function tableCells(
  doc: HwpDocument,
  op: DeleteRow,
  visit: (index: number, row: number, col: number, span: number, cols: number) => void,
): number {
  const { section, paragraph, control } = op
  const { rowCount, colCount, cellCount } = JSON.parse(
    doc.getTableDimensions(section, paragraph, control),
  )
  if (
    !integer(rowCount, 1, 65535) ||
    !integer(colCount, 1, 65535) ||
    !integer(cellCount, 1, 1_000_000)
  )
    throw new Error('Invalid HWP table')
  for (let i = 0; i < cellCount; i++) {
    const { row, col, rowSpan, colSpan } = JSON.parse(
      doc.getCellInfo(section, paragraph, control, i),
    )
    if (
      !integer(row, 0, rowCount - 1) ||
      !integer(col, 0, colCount - 1) ||
      !integer(rowSpan, 1, rowCount - row) ||
      !integer(colSpan, 1, colCount - col)
    )
      throw new Error('Invalid HWP cell bounds')
    visit(i, row, col, rowSpan, colSpan)
  }
  return cellCount
}

function checked(result: string) {
  const reply = JSON.parse(result)
  if (reply?.ok !== true) throw new Error('HWP rejected the structure operation')
  return reply
}

/** The caller owns rollback if native execution or post-mutation observation fails. */
export function applyHwpStructure(
  doc: HwpDocument,
  op: HwpStructureOp,
  call?: () => unknown,
): HwpStructureEffect {
  checkTarget(doc, op)
  const execute = (replay: () => string): string => {
    const result = call ? call() : replay()
    if (typeof result !== 'string') throw new Error('Expected an HWP mutation result')
    return result
  }
  if (op.kind === 'deleteParagraph') {
    const result = execute(() => doc.deleteParagraph(op.section, op.paragraph))
    checked(result)
    return { ...op, result }
  }
  if (op.kind === 'insertCellPicture') {
    const result = execute(() => picture(doc, op))
    const { controlIdx } = checked(result)
    if (!integer(controlIdx)) throw new Error('Missing HWP picture index')
    return { kind: op.kind, at: op.at, anchor: op.anchor, control: controlIdx, result }
  }
  // Table::delete_row retains spanning cells, then sorts by the NEW (row, col). Neither ordinal nor text identifies a survivor.
  const survivors = new Map<number, { index: number; span: number; cols: number }>()
  const count = tableCells(doc, op, (index, row, col, span, cols) => {
    if (row === op.row && span === 1) return
    const key = (row > op.row ? row - 1 : row) * 65536 + col
    if (survivors.has(key)) throw new Error('Ambiguous HWP cell origin')
    survivors.set(key, { index, span: span - Number(row <= op.row && op.row < row + span), cols })
  })
  const result = execute(() => doc.deleteTableRow(op.section, op.paragraph, op.control, op.row))
  checked(result)
  const cells = new Int32Array(count).fill(-1)
  tableCells(doc, op, (index, row, col, span, cols) => {
    const key = row * 65536 + col
    const old = survivors.get(key)
    if (!old || old.span !== span || old.cols !== cols)
      throw new Error('Unexpected HWP cell after row deletion')
    cells[old.index] = index
    survivors.delete(key)
  })
  if (survivors.size) throw new Error('Missing HWP cell after row deletion')
  return { ...op, cells, result }
}

/** Null means the original target was deleted, not that another object happens to occupy its index. */
export function mapHwpStructurePosition(
  at: HwpPosition,
  effect: HwpStructureEffect,
): HwpPosition | null {
  if (effect.kind === 'insertCellPicture') {
    const host = effect.at
    const depth = host.path.length
    if (
      effect.anchor === 'paper' ||
      at.section !== host.section ||
      at.paragraph !== host.paragraph ||
      at.path.length <= depth ||
      !host.path.every(
        (a, i) =>
          a.controlIndex === at.path[i].controlIndex &&
          a.cellIndex === at.path[i].cellIndex &&
          a.cellParaIndex === at.path[i].cellParaIndex,
      ) ||
      at.path[depth].controlIndex < effect.control
    )
      return at
    const path = at.path.slice()
    path[depth] = { ...path[depth], controlIndex: path[depth].controlIndex + 1 }
    return { ...at, path }
  }
  if (at.section !== effect.section) return at
  if (effect.kind === 'deleteParagraph') {
    if (at.paragraph === effect.paragraph) return null
    return at.paragraph > effect.paragraph ? { ...at, paragraph: at.paragraph - 1 } : at
  }
  const first = at.path[0]
  if (at.paragraph !== effect.paragraph || first?.controlIndex !== effect.control) return at
  const cell = effect.cells[first.cellIndex]
  if (cell === undefined) throw new RangeError('HWP cell was not in the old table')
  if (cell === -1) return null
  if (cell === first.cellIndex) return at
  const path = at.path.slice()
  path[0] = { ...first, cellIndex: cell }
  return { ...at, path }
}

export function mapHwpStructureOp(
  op: HwpStructureOp,
  effect: HwpStructureEffect,
): HwpStructureOp | null {
  if (op.kind === 'insertCellPicture') {
    const at = mapHwpStructurePosition(op.at, effect)
    return at === op.at ? op : at && { ...op, at }
  }
  if (effect.kind === 'insertCellPicture' || op.section !== effect.section) return op
  if (effect.kind === 'deleteParagraph') {
    if (op.paragraph === effect.paragraph) return null
    return op.paragraph > effect.paragraph ? { ...op, paragraph: op.paragraph - 1 } : op
  }
  if (op.kind !== 'deleteRow' || op.paragraph !== effect.paragraph || op.control !== effect.control)
    return op
  if (op.row === effect.row) return null
  return op.row > effect.row ? { ...op, row: op.row - 1 } : op
}

/** Capture before a splice. A surviving prefix/suffix still accepts typing even when its physical paragraph changed. */
export function captureHwpParagraphLoss(
  doc: HwpDocument,
  op: HwpTextSplice,
): (at: HwpPosition) => boolean {
  const { from, to } = op
  const depth = from.path.length
  const first = from.path.at(-1)?.cellParaIndex ?? from.paragraph
  const last = to.path.at(-1)?.cellParaIndex ?? to.paragraph
  if (first === last) return () => false
  const length = (p: HwpPosition) =>
    depth
      ? doc.getCellParagraphLengthByPath(p.section, p.paragraph, JSON.stringify(p.path))
      : doc.getParagraphLength(p.section, p.paragraph)
  const firstLength = length(from),
    lastLength = length(to)
  if (from.offset > firstLength || to.offset > lastLength)
    throw new RangeError('HWP deletion exceeds its paragraph')
  const boundaryOnly = last === first + 1 && from.offset === firstLength && to.offset === 0
  // A bare merge keeps the first paragraph, even if empty. Removing a whole line up to the next
  // line's start consumes the first line, not an empty successor occupying that boundary.
  const lostFirst = from.offset === 0 && !boundaryOnly
  const lostLast = to.offset === lastLength && (lastLength !== 0 || !lostFirst)
  return (at) => {
    if (at.path.length < depth) return false
    const ancestor = at.path.length === depth ? at : { ...at, path: at.path.slice(0, depth) }
    if (!sameHwpContainer(ancestor, from)) return false
    const para = ancestor.path.at(-1)?.cellParaIndex ?? ancestor.paragraph
    if (para > first && para < last) return true
    // A boundary paragraph's nested control can move to the survivor. Its native relocation decides, not its host's text.
    if (at.path.length > depth) return false
    return (para === first && lostFirst) || (para === last && lostLast)
  }
}
