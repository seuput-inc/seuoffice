/** Text offsets count Unicode scalars, not Studio logical caret slots beside inline objects. */
export interface HwpPosition {
  section: number
  paragraph: number
  path: { controlIndex: number; cellIndex: number; cellParaIndex: number }[]
  offset: number
}

/** LF in a splice separates paragraphs. Shift+Enter inserts one LF inside a paragraph. */
export type HwpTextSplice =
  | { kind: 'splice'; from: HwpPosition; to: HwpPosition; text: string }
  | { kind: 'lineBreak'; from: HwpPosition; to: HwpPosition; text: '\n' }

/** Local engine observations, keyed by the exact applied splice. Never read from the wire. */
export type HwpControlRelocations = ReadonlyMap<
  HwpTextSplice,
  readonly {
    beforeParagraph: number
    beforeControl: number
    afterParagraph: number
    afterControl: number
  }[]
>

type Affinity = 'before' | 'after'
const MAX_INDEX = 0x7fffffff
const MAX_PATH = 16
const MAX_OPS = 256
const MAX_TEXT = 256 * 1024 // UTF-16 units on the wire, not editor offsets.
export const invalidHwpText = /[^\t\n\u0020-\ud7ff\ue000-\u{10ffff}]/u

function index(n: unknown): n is number {
  return typeof n === 'number' && Number.isInteger(n) && n >= 0 && n <= MAX_INDEX
}

function fields(value: unknown, keys: string[]): value is Record<string, unknown> {
  return (
    typeof value === 'object' &&
    value !== null &&
    !Array.isArray(value) &&
    Object.keys(value).length === keys.length &&
    keys.every((key) => Object.hasOwn(value, key))
  )
}

export function parseHwpPosition(value: unknown): HwpPosition | null {
  if (!fields(value, ['section', 'paragraph', 'path', 'offset'])) return null
  const { section, paragraph, path, offset } = value
  if (
    !index(section) ||
    !index(paragraph) ||
    !index(offset) ||
    !Array.isArray(path) ||
    path.length > MAX_PATH
  )
    return null
  const steps: HwpPosition['path'] = []
  for (const step of path) {
    if (!fields(step, ['controlIndex', 'cellIndex', 'cellParaIndex'])) return null
    const { controlIndex, cellIndex, cellParaIndex } = step
    if (!index(controlIndex) || !index(cellIndex) || !index(cellParaIndex)) return null
    steps.push({ controlIndex, cellIndex, cellParaIndex })
  }
  return { section, paragraph, path: steps, offset }
}

function paragraphAt(p: HwpPosition, depth = p.path.length): number {
  return depth === 0 ? p.paragraph : p.path[depth - 1].cellParaIndex
}

/** Is p in this paragraph list or a descendant of one of its paragraphs? */
function within(p: HwpPosition, list: HwpPosition): boolean {
  if (p.section !== list.section || p.path.length < list.path.length) return false
  return list.path.every(
    (step, i) =>
      paragraphAt(p, i) === paragraphAt(list, i) &&
      p.path[i].controlIndex === step.controlIndex &&
      p.path[i].cellIndex === step.cellIndex,
  )
}

export function sameHwpContainer(a: HwpPosition, b: HwpPosition): boolean {
  return a.path.length === b.path.length && within(a, b)
}

function compare(a: HwpPosition, b: HwpPosition): number {
  return paragraphAt(a) - paragraphAt(b) || a.offset - b.offset
}

export function parseHwpTextSplice(value: unknown): HwpTextSplice | null {
  if (!fields(value, ['kind', 'from', 'to', 'text'])) return null
  const { kind, text } = value
  if (
    (kind !== 'splice' && kind !== 'lineBreak') ||
    typeof text !== 'string' ||
    text.length > MAX_TEXT
  )
    return null
  // CR is normalized by rhwp. A lone surrogate would be replaced on entry to WASM.
  if (invalidHwpText.test(text)) return null
  const from = parseHwpPosition(value.from)
  const to = parseHwpPosition(value.to)
  if (!from || !to || !sameHwpContainer(from, to)) return null
  const order = compare(from, to)
  if (order > 0 || (order === 0 && text === '')) return null
  if (kind === 'lineBreak' && (order !== 0 || text !== '\n')) return null
  const op: HwpTextSplice =
    kind === 'lineBreak' ? { kind, from, to, text: '\n' } : { kind, from, to, text }
  return change(op) ? op : null
}

export function parseHwpTextSplices(value: unknown): HwpTextSplice[] | null {
  if (!Array.isArray(value) || value.length > MAX_OPS) return null
  const ops: HwpTextSplice[] = []
  let units = 0
  for (const item of value) {
    if (typeof item?.text !== 'string' || (units += item.text.length) > MAX_TEXT) return null
    const op = parseHwpTextSplice(item)
    if (!op) return null
    ops.push(op)
  }
  return ops
}

function at(
  p: HwpPosition,
  depth: number,
  paragraph: number,
  offset = p.offset,
): HwpPosition | null {
  if (!index(paragraph) || !index(offset)) return null
  if (paragraph === paragraphAt(p, depth)) return offset === p.offset ? p : { ...p, offset }
  if (depth === 0) return { ...p, paragraph, offset }
  const path = p.path.slice()
  path[depth - 1] = { ...path[depth - 1], cellParaIndex: paragraph }
  return { ...p, path, offset }
}

interface Change {
  op: HwpTextSplice
  firstLineLength: number
  /** Position immediately after the inserted content in the new document. */
  end: HwpPosition
}

function change(op: HwpTextSplice): Change | null {
  let paragraph = paragraphAt(op.from)
  let offset = op.from.offset
  let firstLineLength: number | undefined
  // rhwp insert_text_native and insert_text_in_cell_by_path count Rust chars, not UTF-16.
  for (const c of op.text) {
    if (c === '\n' && op.kind === 'splice') {
      firstLineLength ??= offset - op.from.offset
      if (++paragraph > MAX_INDEX) return null
      offset = 0
    } else if (++offset > MAX_INDEX) return null
  }
  const end = at(op.from, op.from.path.length, paragraph, offset)
  return end && { op, end, firstLineLength: firstLineLength ?? offset - op.from.offset }
}

function mapPoint(
  p: HwpPosition,
  { op, end }: Change,
  affinity: Affinity,
  moves?: HwpControlRelocations,
): HwpPosition | null {
  const { from, to } = op
  if (!within(p, from)) return p
  const depth = from.path.length
  const host = paragraphAt(p, depth)
  const first = paragraphAt(from),
    last = paragraphAt(to),
    final = paragraphAt(end)
  if (host < first) return p
  if (host > last) return at(p, depth, host + final - last)
  if (p.path.length > depth) {
    // Text insertion keeps control ordinals. Deletion can remove fields; a split redistributes controls.
    if (compare(from, to) === 0 && final === first) return p
    const control = p.path[depth].controlIndex
    const entries = moves?.get(op)
    const known = entries?.find((m) => m.beforeParagraph === host && m.beforeControl === control)
    if (
      !known ||
      !index(known.afterControl) ||
      known.afterParagraph < first ||
      known.afterParagraph > final
    )
      return null
    if (
      entries?.some(
        (m) =>
          m !== known &&
          ((m.beforeParagraph === host && m.beforeControl === control) ||
            (m.afterParagraph === known.afterParagraph && m.afterControl === known.afterControl)),
      )
    )
      return null
    const mapped = at(p, depth, known.afterParagraph)
    if (!mapped || known.afterControl === control) return mapped
    const path = mapped.path.slice()
    path[depth] = { ...path[depth], controlIndex: known.afterControl }
    return { ...mapped, path }
  }
  if (compare(p, from) < 0) return p
  if (compare(p, to) > 0) return at(p, depth, final, end.offset + p.offset - to.offset)
  if (compare(p, from) !== 0 && compare(p, to) === 0) return end
  return affinity === 'after' ? end : from
}

export function mapHwpPosition(
  position: HwpPosition,
  ops: readonly HwpTextSplice[],
  affinity: Affinity = 'after',
  moves?: HwpControlRelocations,
): HwpPosition | null {
  let p = position
  for (const op of ops) {
    const c = change(op)
    const mapped = c && mapPoint(p, c, affinity, moves)
    if (!mapped) return null
    p = mapped
  }
  return p
}

function deletion(from: HwpPosition, to: HwpPosition): Change {
  return { op: { kind: 'splice', from, to, text: '' }, end: from, firstLineLength: 0 }
}

function atoms(ops: readonly HwpTextSplice[]): Change[][] | null {
  const groups: Change[][] = []
  for (const op of ops) {
    const c = change(op)
    if (!c) return null
    if (!op.text || compare(op.from, op.to) === 0) {
      groups.push([c])
      continue
    }
    // Insert first to preserve the replacement's original anchor when deletions overlap.
    const insert = { ...c, op: { ...op, to: op.from } }
    const right = mapPoint(op.to, insert, 'after')
    if (!right) return null
    groups.push([insert], [deletion(c.end, right)])
  }
  return groups
}

function afterOne(
  c: Change,
  other: Change,
  priority: Affinity,
  moves?: HwpControlRelocations,
): Change[] | null {
  const { op } = c
  // Even unchanged endpoints don't identify the same control array after another host edit.
  const observedHostEdited =
    moves?.has(op) &&
    sameHwpContainer(op.from, other.op.from) &&
    paragraphAt(other.op.from) <= paragraphAt(op.to) &&
    paragraphAt(other.op.to) >= paragraphAt(op.from)
  if (op.text) {
    const p = mapPoint(op.from, other, priority, moves)
    if (!p || p.offset + c.firstLineLength > MAX_INDEX) return null
    if (p === op.from && !observedHostEdited) return [c]
    const breaks = paragraphAt(c.end) - paragraphAt(op.from)
    const end = at(
      p,
      p.path.length,
      paragraphAt(p) + breaks,
      breaks ? c.end.offset : p.offset + c.firstLineLength,
    )
    return end && [{ op: { ...op, from: p, to: p }, end, firstLineLength: c.firstLineLength }]
  }
  const from = mapPoint(op.from, other, 'after', moves)
  const to = mapPoint(op.to, other, 'before', moves)
  if (!from || !to || !sameHwpContainer(from, to) || compare(from, to) > 0) return null
  const cut = other.op.from
  if (
    other.op.text &&
    sameHwpContainer(op.from, cut) &&
    compare(op.from, cut) < 0 &&
    compare(cut, op.to) < 0
  ) {
    // Delete the right half first, so neither the left address nor the peer's insertion is consumed.
    return [deletion(other.end, to), deletion(from, cut)]
  }
  if (from === op.from && to === op.to && !observedHostEdited) return [c]
  return compare(from, to) === 0 ? [] : [deletion(from, to)]
}

/** Reconstitute each original replacement when the insertion and deletion still meet. */
function splices(groups: Change[][], original: readonly HwpTextSplice[]): HwpTextSplice[] | null {
  const result: HwpTextSplice[] = []
  let i = 0
  for (const op of original) {
    const group = groups[i++]
    if (op.text && compare(op.from, op.to) !== 0) {
      const insert = group[0]
      const deletes = groups[i++]
      if (
        deletes.length === 1 &&
        sameHwpContainer(insert.end, deletes[0].op.from) &&
        compare(insert.end, deletes[0].op.from) === 0
      ) {
        const to = mapPoint(deletes[0].op.to, deletion(insert.op.from, insert.end), 'after')
        if (!to) return null
        result.push({ ...insert.op, to })
      } else {
        result.push(insert.op)
        for (const c of deletes) result.push(c.op)
      }
    } else {
      for (const c of group) result.push(c.op)
    }
    if (result.length > MAX_OPS) return null
  }
  return result
}

/** Both inputs start at the same revision. Execute `pending` in the result AFTER confirmed, and vice versa. */
export function rebaseHwpText(
  confirmed: readonly HwpTextSplice[],
  pending: readonly HwpTextSplice[],
  moves?: HwpControlRelocations,
): {
  confirmed: HwpTextSplice[]
  pending: HwpTextSplice[]
} | null {
  if (confirmed.length > MAX_OPS || pending.length > MAX_OPS) return null
  const rows = atoms(confirmed),
    columns = atoms(pending)
  if (!rows || !columns) return null
  let budget = 262144
  // Each group stays a single insertion or a sequence of disjoint deletions in descending order.
  // Lower deletions retain their input coordinates after a higher one is applied.
  function after(group: Change[], others: Change[], priority: Affinity): Change[] | null {
    for (const other of others) {
      const next: Change[] = []
      for (const c of group) {
        if (--budget < 0) return null
        const parts = afterOne(c, other, priority, moves)
        if (!parts) return null
        next.push(...parts)
      }
      group = next
    }
    return group
  }
  for (let i = 0; i < rows.length; i++) {
    let row = rows[i]
    for (let j = 0; j < columns.length; j++) {
      const col = columns[j]
      const rowAfter = after(row, col, 'before'),
        colAfter = after(col, row, 'after')
      if (!rowAfter || !colAfter) return null
      row = rowAfter
      columns[j] = colAfter
    }
    rows[i] = row
  }
  const left = splices(rows, confirmed),
    right = splices(columns, pending)
  return left && right ? { confirmed: left, pending: right } : null
}
