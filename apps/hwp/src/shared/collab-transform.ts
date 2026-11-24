import type { HwpDocument } from '@rhwp/core'
import { HistoryError } from '@seuoffice/collab/client'
import {
  applyHwpCommand,
  captureHwpCommandEffect,
  findHwpMemo,
  mapHwpCharShapeIds,
  mapHwpCommand,
  type HwpCommandEffect,
  type HwpCommandOp,
} from './collab-command'
import { applyHwpMemo } from './collab-memo'
import { applyHwpOp, textOp, type HwpOp } from './collab-editor-ops'
import {
  captureHwpParagraphLoss,
  applyHwpStructure,
  type HwpStructureEffect,
} from './collab-structure'
import {
  mapHwpPosition,
  sameHwpContainer,
  type HwpPosition,
  type HwpTextSplice,
} from './collab-text'
import {
  hwpRichTextEffect,
  hwpSplitOffset,
  remapHwpRichText,
  sameHwpInlineBoundary,
} from './collab-text-native'

interface Mirror extends HwpDocument {
  forkCollabDocument(): Mirror
  restoreCollabConfirmed(source: HwpDocument): void
  forkCollabControlTrace(
    section: number,
    paragraph: number,
    path: string,
    last: number,
  ): Mirror | undefined
  readCollabControlTrace(section: number, paragraph: number, path: string, last: number): string
}

type Affinity = 'before' | 'after'
// A control or grid line belongs to the paragraph itself, not to its first cell.
type Target = HwpPosition & {
  object?: { control: number; axis?: 'row' | 'column'; index?: number; gap?: boolean }
}
type MapPoint = (p: Target, affinity: Affinity) => Target | null
type Range = { from: HwpPosition; to: HwpPosition }
type Step = {
  forward: MapPoint
  backward: MapPoint
  insertion?: Range
  created?: Target
  command?: { op: HwpCommandOp; effect: HwpCommandEffect }
}
const para = (p: HwpPosition, depth = p.path.length) =>
  depth ? p.path[depth - 1].cellParaIndex : p.paragraph
const cmp = (a: HwpPosition, b: HwpPosition) => para(a) - para(b) || a.offset - b.offset
const same = (a: HwpPosition, b: HwpPosition) => sameHwpContainer(a, b) && cmp(a, b) === 0

function inPara(p: Target, index: number, offset = p.offset, depth = p.path.length): Target {
  if (para(p, depth) === index) return p.offset === offset ? p : { ...p, offset }
  if (!depth) return { ...p, paragraph: index, offset }
  const path = p.path.slice()
  path[depth - 1] = { ...path[depth - 1], cellParaIndex: index }
  return { ...p, path, offset }
}

function length(doc: HwpDocument, p: HwpPosition) {
  return p.path.length
    ? doc.getCellParagraphLengthByPath(p.section, p.paragraph, JSON.stringify(p.path))
    : doc.getParagraphLength(p.section, p.paragraph)
}

function direction(steps: readonly Step[], backward = false): MapPoint {
  return (p, affinity) => {
    for (let i = 0; i < steps.length; i++) {
      const mapped = backward
        ? steps[steps.length - i - 1].backward(p, affinity)
        : steps[i].forward(p, affinity)
      if (!mapped) return null
      p = mapped
    }
    return p
  }
}

type Relocation = [number, number, number, number]
function textStep(
  op: HwpTextSplice,
  rows: Relocation[] | undefined,
  lost: (p: HwpPosition) => boolean,
): Step {
  const { from, to } = op,
    end = mapHwpPosition(from, [op])
  if (!end) throw new RangeError('HWP splice overflows its container')
  function edge(oldEnd: HwpPosition, newEnd: HwpPosition, reverse: boolean): MapPoint {
    const depth = from.path.length,
      first = para(from),
      last = para(oldEnd),
      next = para(newEnd)
    const k = reverse ? 2 : 0,
      j = 2 - k
    const ordinals = rows && new Map<number, Map<number, Relocation>>()
    if (ordinals && rows)
      for (const row of rows) {
        let controls = ordinals.get(row[k])
        if (!controls) {
          controls = new Map()
          ordinals.set(row[k], controls)
        }
        if (controls.has(row[k + 1])) throw new Error('Ambiguous HWP control relocation')
        controls.set(row[k + 1], row)
      }
    return (p, affinity) => {
      if (p.path.length < depth) return p
      const host = p.path.length === depth ? p : { ...p, path: p.path.slice(0, depth) }
      if (!sameHwpContainer(host, from)) return p
      const index = para(host)
      if (index < first) return p
      if (index > last) return inPara(p, index + next - last, p.offset, depth)
      const child = p.path[depth],
        control = child?.controlIndex ?? p.object?.control
      if (control !== undefined) {
        if (!ordinals) return p
        const found = ordinals.get(index)?.get(control)
        if (!found) return null
        const at = inPara(p, found[j], p.offset, depth)
        if (found[j + 1] === control) return at
        if (!child) return { ...at, object: { ...p.object, control: found[j + 1] } }
        const path = at.path.slice()
        path[depth] = { ...child, controlIndex: found[j + 1] }
        return { ...at, path }
      }
      if (!reverse && lost(p)) return null
      if (cmp(p, from) < 0) return p
      if (cmp(p, oldEnd) > 0) return inPara(p, next, newEnd.offset + p.offset - oldEnd.offset)
      if (cmp(p, from) !== 0 && cmp(p, oldEnd) === 0) return newEnd
      return affinity === 'after' ? newEnd : from
    }
  }
  return {
    forward: edge(to, end, false),
    backward: edge(end, to, true),
    insertion: op.text ? { from, to: end } : undefined,
  }
}

function controlInsertion(host: HwpPosition, ordinal: number): Step {
  function edge(reverse: boolean): MapPoint {
    return (p) => {
      const depth = host.path.length
      if (p.path.length < depth) return p
      const parent = p.path.length === depth ? p : { ...p, path: p.path.slice(0, depth) }
      if (!sameHwpContainer(parent, host) || para(parent) !== para(host)) return p
      const child = p.path[depth],
        control = child?.controlIndex ?? p.object?.control
      if (control === undefined || control < ordinal) return p
      if (reverse && control === ordinal) return null
      const index = control + (reverse ? -1 : 1)
      if (!child) return { ...p, object: { ...p.object, control: index } }
      const path = p.path.slice()
      path[depth] = { ...child, controlIndex: index }
      return { ...p, path }
    }
  }
  return {
    forward: edge(false),
    backward: edge(true),
    created: { ...host, object: { control: ordinal } },
  }
}

function structureStep(effect: HwpStructureEffect): Step {
  if (effect.kind === 'insertCellPicture') {
    const host = effect.anchor === 'cell' ? effect.at : { ...effect.at, path: [] }
    return controlInsertion(host, effect.control)
  }
  if (effect.kind === 'deleteParagraph') {
    const { section, paragraph } = effect
    return {
      forward: (p) =>
        p.section !== section || p.paragraph < paragraph
          ? p
          : p.paragraph === paragraph
            ? null
            : { ...p, paragraph: p.paragraph - 1 },
      backward: (p) =>
        p.section !== section || p.paragraph < paragraph ? p : { ...p, paragraph: p.paragraph + 1 },
    }
  }
  return gridStep(
    effect.section,
    effect.paragraph,
    effect.control,
    'row',
    effect.row,
    -1,
    effect.cells,
  )
}

function gridBoundary(index: number, pivot: number, delta: number, affinity: Affinity): number {
  return index < pivot || (index === pivot && (delta < 0 || affinity === 'before'))
    ? index
    : index + delta
}

function gridStep(
  section: number,
  paragraph: number,
  control: number,
  axis: 'row' | 'column',
  index: number,
  delta: number,
  cells: Int32Array,
): Step {
  function edge(reverse: boolean): MapPoint {
    return (p, affinity) => {
      if (p.section !== section || p.paragraph !== paragraph) return p
      const cell = p.path[0]
      if (!cell) {
        const object = p.object
        if (
          object?.control !== control ||
          object.axis !== axis ||
          object.index === undefined ||
          object.index < index
        )
          return p
        const d = reverse ? -delta : delta
        if (object.gap)
          return {
            ...p,
            object: { ...object, index: gridBoundary(object.index, index, d, affinity) },
          }
        return d < 0 && object.index === index
          ? null
          : { ...p, object: { ...object, index: object.index + d } }
      }
      if (cell.controlIndex !== control || cell.cellIndex === 65534) return p
      const i = reverse ? cells.indexOf(cell.cellIndex) : cells[cell.cellIndex]
      if (i === undefined || i < 0) return null
      if (i === cell.cellIndex) return p
      const path = p.path.slice()
      path[0] = { ...cell, cellIndex: i }
      return { ...p, path }
    }
  }
  return { forward: edge(false), backward: edge(true) }
}

function projection(doc: HwpDocument, op: HwpOp): HwpTextSplice | null {
  if (textOp(op)) return op
  if (op.kind === 'nativeText') return hwpRichTextEffect(doc, op)
  if (op.kind === 'restoreText')
    return { kind: 'splice', from: op.at, to: op.at, text: op.text.replaceAll('\n', ' ') }
  return null
}

function objectPoint(p: Target): HwpPosition {
  return p.object
    ? {
        section: p.section,
        paragraph: p.paragraph,
        offset: 0,
        path: [...p.path, { controlIndex: p.object.control, cellIndex: 0, cellParaIndex: 0 }],
      }
    : p
}

function withObject(p: HwpPosition, object: Target['object']): Target {
  if (!object) return p
  const child = p.path.at(-1)
  if (!child) throw new Error('HWP control lost its parent path')
  return {
    ...p,
    path: p.path.slice(0, -1),
    offset: 0,
    object: { ...object, control: child.controlIndex },
  }
}

function commandStep(doc: Mirror, op: HwpCommandOp): Step {
  const finish = captureHwpCommandEffect(doc, op),
    effect = finish(applyHwpCommand(doc, op))
  if (effect.change.type === 'identity') return { forward: (p) => p, backward: (p) => p }
  function edge(reverse: boolean): MapPoint {
    return (p, affinity) => {
      const object = p.object
      if (
        effect.change.type === 'caption' &&
        object?.control === effect.change.control &&
        p.path.length === effect.change.at.path.length &&
        sameHwpContainer(p, effect.change.at) &&
        para(p) === para(effect.change.at)
      )
        return p
      if (
        op.action === 'grid' &&
        object?.control === op.control &&
        !p.path.length &&
        p.section === op.at.section &&
        p.paragraph === op.at.paragraph
      ) {
        if (!object.axis || object.index === undefined) return p
        if (object.gap && effect.change.type === 'grid' && object.axis === op.axis) {
          const delta = (op.side === 'delete') === reverse ? 1 : -1
          return {
            ...p,
            object: {
              ...object,
              index: gridBoundary(object.index, effect.change.pivot, delta, affinity),
            },
          }
        }
        const index = reverse
          ? effect.unmapGrid(p, op.control, object.axis, object.index)
          : effect.mapGrid(p, op.control, object.axis, object.index)
        return index === null ? null : { ...p, object: { ...object, index } }
      }
      const at = objectPoint(p)
      const mapped = reverse ? effect.unmapPosition(at, affinity) : effect.mapPosition(at, affinity)
      return mapped && withObject(mapped, object)
    }
  }
  const insertion =
    effect.change.type === 'paste'
      ? {
          from: effect.change.at,
          to: { ...effect.change.at, offset: effect.change.at.offset + effect.change.count },
        }
      : undefined
  return { command: { op, effect }, forward: edge(false), backward: edge(true), insertion }
}

function observe(doc: Mirror, op: HwpOp, shape = projection(doc, op)): Step {
  if (op.kind === 'deleteRow' || op.kind === 'deleteParagraph' || op.kind === 'insertCellPicture')
    return structureStep(applyHwpStructure(doc, op))
  if (op.kind === 'command') return commandStep(doc, op)
  if (op.kind === 'insertMemo') {
    const reply = JSON.parse(applyHwpMemo(doc, op))
    if (reply.ok !== true) throw new Error('HWP refused the memo')
    const m = findHwpMemo(doc, reply.id)
    if (!m) throw new Error('HWP did not return the inserted memo')
    return controlInsertion(m.at, m.control)
  }
  if (!shape) throw new Error('Missing native HWP text projection')
  const lost = captureHwpParagraphLoss(doc, shape)
  let rows: Relocation[] | undefined
  // Insertion without a paragraph split cannot reorder pre-existing controls.
  if (
    !same(shape.from, shape.to) ||
    (shape.kind === 'splice' && shape.text.includes('\n')) ||
    (op.kind === 'nativeText' && op.action === 'logicalRange')
  ) {
    const end = mapHwpPosition(shape.from, [shape])!
    const { section, paragraph, path } = shape.from,
      json = JSON.stringify(path)
    const trace = doc.forkCollabControlTrace(section, paragraph, json, para(shape.to))
    if (trace) {
      try {
        applyHwpOp(trace, op)
        rows = JSON.parse(trace.readCollabControlTrace(section, paragraph, json, para(end)))
      } finally {
        trace.free()
      }
    } else rows = []
  }
  applyHwpOp(doc, op)
  return textStep(shape, rows, lost)
}

// Split at changes of origin, not at every scalar in a long selected paragraph.
function ranges(doc: HwpDocument, range: Range, map: MapPoint, cuts: HwpPosition[]): Range[] {
  const result: Range[] = []
  function include(a: HwpPosition, b: HwpPosition) {
    const from = map(a, 'after'),
      to = map(b, 'before')
    if (!from || !to || !sameHwpContainer(from, to) || cmp(from, to) >= 0) return
    const prev = result.at(-1)
    if (prev && same(prev.to, from)) prev.to = to
    else result.push({ from, to })
  }
  const inside = cuts
    .filter(
      (p) => sameHwpContainer(p, range.from) && cmp(p, range.from) > 0 && cmp(p, range.to) < 0,
    )
    .sort(cmp)
  let j = 0
  for (let i = para(range.from); i <= para(range.to); i++) {
    let p = inPara(range.from, i, i === para(range.from) ? range.from.offset : 0)
    const last = i === para(range.to) ? range.to.offset : length(doc, p)
    while (j < inside.length && para(inside[j]) === i) {
      const next = inside[j++]
      if (next.offset > p.offset) include(p, next)
      p = next
    }
    if (p.offset < last) {
      const end = { ...p, offset: last }
      include(p, end)
      p = end
    }
    if (i < para(range.to)) include(p, inPara(p, i + 1, 0))
  }
  return result
}

function fragments(
  doc: HwpDocument,
  op: HwpTextSplice,
  map: MapPoint,
  cuts: HwpPosition[],
): HwpTextSplice[] | null {
  const anchor = map(op.from, 'after')
  if (same(op.from, op.to)) return anchor && [{ ...op, from: anchor, to: anchor }]
  const pieces = ranges(doc, op, map, cuts)
  if (!anchor && (!pieces.length || op.text)) return null
  if (pieces.length === 1 && anchor && same(anchor, pieces[0].from))
    return [{ ...op, ...pieces[0] }]
  const result: HwpTextSplice[] = []
  for (const piece of pieces.reverse()) result.push({ kind: 'splice', ...piece, text: '' })
  if (op.text && anchor) {
    const at = mapHwpPosition(anchor, result)!
    result.push({ ...op, from: at, to: at })
  }
  return result
}

function addCut(cuts: HwpPosition[], p: HwpPosition | null) {
  if (p && !cuts.some((c) => same(c, p))) cuts.push(p)
}

function boundaries(op: HwpOp, shape: HwpTextSplice | null) {
  return shape
    ? [shape.from, shape.to]
    : op.kind === 'command' && (op.action === 'pasteText' || op.action === 'table')
      ? [op.at]
      : []
}

// Where a confirmed op put new content: a later structural deletion of what holds it must not
// discard it unannounced. Text inserted after a line break lands in a new paragraph instead.
function additions(op: HwpOp, shape: HwpTextSplice | null): HwpPosition[] {
  if (shape) return shape.text && !shape.text.startsWith('\n') ? [shape.from] : []
  if (op.kind === 'insertCellPicture' || op.kind === 'insertMemo') return [op.at]
  if (op.kind !== 'command') return []
  if (op.action === 'pasteText' || op.action === 'table') return [op.at]
  if (op.action === 'grid' && op.side !== 'delete') return [op.at]
  return []
}

/** True when `added` lies in a cell that deleting `index` along `axis` would remove whole. */
function inRemovedCell(
  doc: HwpDocument,
  host: HwpPosition,
  control: number,
  axis: 'row' | 'column',
  index: number,
  added: HwpPosition,
): boolean {
  const depth = host.path.length
  if (
    added.section !== host.section ||
    added.paragraph !== host.paragraph ||
    added.path.length <= depth ||
    added.path[depth].controlIndex !== control ||
    host.path.some((step, i) => {
      const other = added.path[i]
      return (
        other.controlIndex !== step.controlIndex ||
        other.cellIndex !== step.cellIndex ||
        other.cellParaIndex !== step.cellParaIndex
      )
    })
  )
    return false
  const cell = JSON.parse(
    doc.getCellInfoByPath(
      added.section,
      added.paragraph,
      JSON.stringify(added.path.slice(0, depth + 1)),
    ),
  ) as { row: number; col: number; rowSpan: number; colSpan: number }
  return axis === 'row'
    ? cell.row === index && cell.rowSpan === 1
    : cell.col === index && cell.colSpan === 1
}

// The next pending op was written AFTER this local op. Map its old text backwards,
// but carry text inserted by this op directly to its matching replay insertion.
function cross(
  previous: MapPoint,
  local: Step,
  replayed: readonly Step[],
  missingDependency?: () => void,
): MapPoint {
  const forward = direction(replayed),
    source = local.insertion
  const i = replayed.findIndex((s) => s.insertion !== undefined),
    dest = replayed[i]?.insertion
  const tail = dest ? direction(replayed.slice(i + 1)) : undefined
  return (p, affinity) => {
    if (source && !p.object && sameHwpContainer(p, source.from)) {
      const l = cmp(p, source.from),
        r = cmp(p, source.to)
      if (
        (l > 0 || (l === 0 && affinity === 'after')) &&
        (r < 0 || (r === 0 && affinity === 'before'))
      ) {
        if (!dest || !tail) {
          missingDependency?.()
          return null
        }
        const n = para(p) - para(source.from)
        return tail(
          inPara(
            dest.from,
            para(dest.from) + n,
            n ? p.offset : dest.from.offset + p.offset - source.from.offset,
          ),
          affinity,
        )
      }
    }
    if (local.created && replayed[0]?.created) {
      const src = local.created,
        dst = replayed[0].created
      const at = objectPoint(p),
        depth = src.path.length
      if (at.path.length > depth && at.path[depth].controlIndex === src.object!.control) {
        const host = { ...at, path: at.path.slice(0, depth) }
        if (sameHwpContainer(host, src) && para(host) === para(src)) {
          const mapped = {
            ...at,
            section: dst.section,
            paragraph: dst.paragraph,
            path: [
              ...dst.path,
              { ...at.path[depth], controlIndex: dst.object!.control },
              ...at.path.slice(depth + 1),
            ],
          }
          return withObject(mapped, p.object)
        }
      }
    }
    if (local.command && replayed[0]?.command) {
      const { op, effect } = local.command,
        paired = replayed[0].command.effect
      if (op.action === 'grid' && p.object?.axis && !p.object.gap && p.object.index !== undefined) {
        const index = effect.mapCreatedGrid(
          p,
          p.object.control,
          p.object.axis,
          p.object.index,
          paired,
        )
        if (index !== null && replayed[0].command.op.action === 'grid') {
          const next = replayed[0].command.op
          return { ...next.at, object: { control: next.control, axis: p.object.axis, index } }
        }
      }
      if ((!p.object?.gap || op.action === 'table') && op.action !== 'pasteText') {
        const at = effect.mapCreatedPosition(
          objectPoint(p),
          paired,
          op.action !== 'grid'
            ? undefined
            : (index) =>
                previous(
                  {
                    ...op.at,
                    object: {
                      control: op.control,
                      axis: op.axis === 'row' ? 'column' : 'row',
                      index,
                    },
                  },
                  'after',
                )?.object?.index ?? null,
        )
        if (at) return withObject(at, p.object)
      }
    }
    const old = local.backward(p, affinity)
    if (!old) {
      missingDependency?.()
      return null
    }
    const mapped = previous(old, affinity)
    return mapped && forward(mapped, affinity)
  }
}

export interface HwpRebase {
  replay: HwpOp[]
  deleted: HwpOp[]
  retry: HwpOp[]
  mapCaret(at: HwpPosition): HwpPosition | null
}

/** Advance only the confirmed model. Every replay operand is relative to the previous replay operand. */
export function rebaseHwpOps(
  confirmed: HwpDocument,
  remote: readonly HwpOp[],
  pending: readonly HwpOp[],
): HwpRebase {
  const doc = confirmed as Mirror
  const replay: HwpOp[] = [],
    deleted: HwpOp[] = [],
    retry: HwpOp[] = []
  if (!remote.length) return { replay: [...pending], deleted, retry, mapCaret: (p) => p }
  const remoteDoc = doc.forkCollabDocument()
  let local: Mirror | undefined, target: Mirror | undefined
  try {
    const remoteSteps: Step[] = []
    let cuts: HwpPosition[] = []
    // Content the confirmed ops added, kept in the coordinates of the replay target.
    let added: HwpPosition[] = []
    const advance = (step: Step) => {
      const next: HwpPosition[] = []
      for (const p of added) addCut(next, step.forward(p, 'before'))
      added = next
    }
    for (const op of remote) {
      const shape = projection(remoteDoc, op)
      if (pending.length) {
        const reverse = direction(remoteSteps, true)
        for (const p of boundaries(op, shape)) {
          addCut(cuts, reverse(p, 'before'))
          addCut(cuts, reverse(p, 'after'))
        }
      }
      const fresh = pending.length ? additions(op, shape) : []
      const step = observe(remoteDoc, op, shape)
      advance(step)
      for (const p of fresh) addCut(added, step.forward(p, 'before'))
      remoteSteps.push(step)
    }
    let map = direction(remoteSteps)
    let dependsOnRetry = false
    const markDependency = () => {
      dependsOnRetry = true
    }
    if (pending.length) {
      local = doc.forkCollabDocument()
      target = remoteDoc.forkCollabDocument()
    }
    for (const op of pending) {
      dependsOnRetry = false
      const before = local!,
        after = target!,
        shape = projection(before, op)
      const control = (at: HwpPosition, ordinal: number) => {
        const next = map({ ...at, object: { control: ordinal } }, 'after')
        if (!next?.object) return null
        const { object, ...host } = next
        return { at: host, control: object.control }
      }
      let ops: HwpOp[] | null = null,
        conflict = false
      if (shape) {
        const parts = fragments(before, shape, map, cuts)
        if (parts && textOp(op)) ops = parts
        else if (parts && op.kind === 'restoreText') ops = parts.map((p) => ({ ...op, at: p.from }))
        else if (parts && op.kind === 'nativeText') {
          ops = []
          for (const part of parts) {
            const next = remapHwpRichText(op, before, after, map, part, control)
            if (!next) {
              ops = null
              conflict = true
              break
            }
            ops.push(next)
          }
        }
      } else if (op.kind === 'command') {
        if (op.action === 'memoRemove' || op.action === 'memoText') {
          const old = findHwpMemo(before, op.id)
          if (!old) throw new Error('Missing original HWP memo')
          const mapped = map({ ...old.at, object: { control: old.control } }, 'after')
          if (mapped) {
            const candidates = JSON.parse(after.getMemoList()) as { id: number }[]
            const id = candidates.find(({ id }) => {
              const m = findHwpMemo(after, id)
              return m && m.control === mapped.object!.control && same(m.at, mapped)
            })?.id
            if (id === undefined) throw new Error('Missing mapped HWP memo')
            conflict = findHwpMemo(after, id)!.text !== op.before
            if (!conflict) ops = [{ ...op, id }]
          }
        } else if (op.action === 'characters' && op.at.offset !== op.end) {
          const pieces = ranges(
            before,
            { from: op.at, to: { ...op.at, offset: op.end } },
            map,
            cuts,
          )
          if (pieces.length || map(op.at, 'after'))
            ops = pieces.map(({ from, to }) => ({ ...op, at: from, end: to.offset }))
        } else {
          const mapped = mapHwpCommand(op, map, control)
          if (mapped?.action === 'grid' && op.action === 'grid') {
            const index = map(
              { ...op.at, object: { control: op.control, axis: op.axis, index: op.index } },
              'after',
            )?.object?.index
            if (index !== undefined) {
              const dims = JSON.parse(
                after.getTableDimensions(mapped.at.section, mapped.at.paragraph, mapped.control),
              )
              const count = mapped.axis === 'row' ? dims.rowCount : dims.colCount
              conflict =
                mapped.side === 'delete'
                  ? count <= 1 ||
                    added.some((a) =>
                      inRemovedCell(after, mapped.at, mapped.control, mapped.axis, index, a),
                    )
                  : (count + 1) * (mapped.axis === 'row' ? dims.colCount : dims.rowCount) > 4096
              if (!conflict && op.side !== 'delete') {
                const gap = map(
                  {
                    ...op.at,
                    object: {
                      control: op.control,
                      axis: op.axis,
                      index: op.index + Number(op.side === 'after'),
                      gap: true,
                    },
                  },
                  'after',
                )?.object?.index
                if (gap === undefined || gap > count)
                  throw new Error('HWP insertion gap lost its surviving row or column')
                ops =
                  gap === index + Number(op.side === 'after')
                    ? [{ ...mapped, index }]
                    : [
                        {
                          ...mapped,
                          index: gap === count ? count - 1 : gap,
                          side: gap === count ? 'after' : 'before',
                        },
                      ]
              } else if (!conflict) ops = [{ ...mapped, index }]
            }
          } else if (
            (mapped?.action === 'table' && (mapped.at.path.length || !mapped.inline)) ||
            mapped?.action === 'pasteText'
          ) {
            conflict = !sameHwpInlineBoundary(before, after, map, op.at, mapped.at, control)
            if (!conflict && mapped.action === 'table' && op.action === 'table') {
              const slotsBefore =
                hwpSplitOffset(after, mapped.at) -
                mapped.at.offset +
                op.at.offset +
                op.slotsBefore -
                hwpSplitOffset(before, op.at)
              conflict = slotsBefore < 0 || slotsBefore > 4096
              if (!conflict) ops = [{ ...mapped, slotsBefore }]
            } else if (!conflict) ops = [mapped]
          } else if (mapped) ops = [mapped]
        }
      } else if (op.kind === 'deleteRow' || op.kind === 'deleteParagraph') {
        const p = map(
          {
            section: op.section,
            paragraph: op.paragraph,
            path: [],
            offset: 0,
            object:
              op.kind === 'deleteRow'
                ? { control: op.control, axis: 'row', index: op.row }
                : undefined,
          },
          'before',
        )
        if (p) {
          if (op.kind === 'deleteRow') {
            const next = {
              ...op,
              paragraph: p.paragraph,
              control: p.object!.control,
              row: p.object!.index!,
            }
            const host = { section: next.section, paragraph: next.paragraph, path: [], offset: 0 }
            conflict =
              JSON.parse(after.getTableDimensions(next.section, next.paragraph, next.control))
                .rowCount <= 1 ||
              added.some((a) => inRemovedCell(after, host, next.control, 'row', next.row, a))
            if (!conflict) ops = [next]
          } else {
            // A text point can survive a merge even when deleting its current host
            // would also delete another original paragraph's text or table.
            conflict =
              after.getParagraphCount(op.section) <= 1 ||
              p.offset !== 0 ||
              added.some((a) => a.section === p.section && a.paragraph === p.paragraph) ||
              cuts.some((c) => {
                if (c.section !== op.section || c.path.length || c.paragraph === op.paragraph)
                  return false
                if (
                  map(c, 'after')?.paragraph === p.paragraph ||
                  map(c, 'before')?.paragraph === p.paragraph
                )
                  return true
                const controls = JSON.parse(
                  before.getControlTextPositions(c.section, c.paragraph),
                ) as number[]
                return controls.some((_, ci) => control(c, ci)?.at.paragraph === p.paragraph)
              })
            if (!conflict) ops = [{ ...op, paragraph: p.paragraph }]
          }
        }
      } else if (op.kind === 'insertMemo' || op.kind === 'insertCellPicture') {
        const at = map(op.at, 'after')
        if (at && op.kind === 'insertCellPicture') ops = [{ ...op, at }]
        else if (at && op.kind === 'insertMemo') {
          const end = op.end === op.at.offset ? at : map({ ...op.at, offset: op.end }, 'before')
          if (end && sameHwpContainer(at, end) && para(end) === para(at) && end.offset >= at.offset)
            ops = [{ ...op, at, end: end.offset }]
          else conflict = true
        }
      } else throw new Error('Missing HWP operation transform')
      if (ops) {
        for (const [i, next] of ops.entries()) {
          const runs =
            next.kind === 'restoreText'
              ? next.runs
              : next.kind === 'command' && next.action === 'pasteText'
                ? next.fragment.runs
                : undefined
          if (!runs?.length) continue
          const mapped: [number, number][] = []
          for (let start = 0; start < runs.length; start += 512) {
            const chunk = runs.slice(start, start + 512),
              ids = mapHwpCharShapeIds(
                before,
                after,
                chunk.map((r) => r[1]),
              )
            if (!ids) break
            chunk.forEach(([offset], j) => mapped.push([offset, ids[j]]))
          }
          if (mapped.length !== runs.length) {
            conflict = true
            ops = null
            break
          }
          if (next.kind === 'restoreText') ops[i] = { ...next, runs: mapped }
          else if (next.kind === 'command' && next.action === 'pasteText')
            ops[i] = { ...next, fragment: { ...next.fragment, runs: mapped } }
        }
      }
      const localStep = observe(before, op, shape)
      let steps: Step[] = []
      if (ops) {
        try {
          steps = ops.map((next) => observe(after, next))
        } catch (error) {
          const overlap =
            op.kind === 'insertMemo' &&
            typeof error === 'string' &&
            error.includes('memo_overlap') &&
            JSON.parse(error).code === 'memo_overlap'
          if (
            !overlap &&
            !(
              op.kind === 'restoreText' &&
              error instanceof HistoryError &&
              error.code === 'unsupported'
            )
          )
            throw error
          conflict = true
          ops = null
        }
      }
      if (ops) replay.push(...ops)
      else {
        conflict ||= dependsOnRetry
        ;(conflict ? retry : deleted).push(op)
      }
      const nextCuts: HwpPosition[] = []
      for (const p of [...cuts, ...boundaries(op, shape)]) {
        addCut(nextCuts, localStep.forward(p, 'before'))
        addCut(nextCuts, localStep.forward(p, 'after'))
      }
      if (localStep.insertion) {
        addCut(nextCuts, localStep.insertion.from)
        addCut(nextCuts, localStep.insertion.to)
      }
      cuts = nextCuts
      for (const step of steps) advance(step)
      map = cross(map, localStep, steps, conflict ? markDependency : undefined)
    }
    doc.restoreCollabConfirmed(remoteDoc)
    return { replay, deleted, retry, mapCaret: (p) => map(p, 'after') }
  } finally {
    target?.free()
    local?.free()
    remoteDoc.free()
  }
}
