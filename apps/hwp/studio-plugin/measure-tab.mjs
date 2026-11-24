/** Objects tried in one cell paragraph when looking for a table inside it. */
export const NESTED_CONTROLS_TRIED = 3

/** Page positions are in pixels of 75 HWPUNIT each. */
const HWPUNIT_PER_PX = 75
/** Tab stop positions and paragraph margins come from the engine at twice their HWPUNIT. */
const PARA_UNIT_SCALE = 2
/** A tab stop counts as passed when the tab starts this close to it, as the engine lays it out. */
const TAB_STOP_SLACK_PX = 0.5
/** Kinds of tab stop as the engine numbers them. */
const TAB_LEFT = 0
const TAB_RIGHT = 1

/**
 * The table at `ordinal` among the tables of a paragraph, as a path: in a body paragraph
 * (`outer` empty) its objects are listed, in a cell paragraph (`outer` its address) the
 * first few objects are tried, as tablesOf does.
 */
function tableAt(doc, sec, para, outer, ordinal) {
  const tried =
    outer.length === 0
      ? JSON.parse(doc.getControlTextPositions(sec, para)).length
      : NESTED_CONTROLS_TRIED
  let seen = 0
  for (let control = 0; control < tried; control++) {
    const path = [...outer, { controlIndex: control, cellIndex: 0, cellParaIndex: 0 }]
    let dims = null
    try {
      dims = JSON.parse(doc.getTableDimensionsByPath(sec, para, JSON.stringify(path)))
    } catch {
      // not a table
    }
    if (dims && seen++ === ordinal) return { control, cells: dims.cellCount }
  }
  return null
}

/** The paragraph a tab sits in, with the engine calls that read it; null when it is not there. */
function tabParagraph(doc, spot) {
  const { sec, para } = spot
  if (spot.cells.length === 0) {
    return {
      length: doc.getParagraphLength(sec, para),
      text: (length) => doc.getTextRange(sec, para, 0, length),
      rect: (offset) => JSON.parse(doc.getCursorRect(sec, para, offset)),
      props: () => JSON.parse(doc.getParaPropertiesAt(sec, para)),
    }
  }
  let outer = []
  for (const step of spot.cells) {
    const table = tableAt(doc, sec, para, outer, step.table)
    if (!table || step.cell >= table.cells) return null
    outer = [...outer, { controlIndex: table.control, cellIndex: step.cell, cellParaIndex: 0 }]
    const count = doc.getCellParagraphCountByPath(sec, para, JSON.stringify(outer))
    if (step.para >= count) return null
    outer[outer.length - 1].cellParaIndex = step.para
  }
  const json = JSON.stringify(outer)
  const last = outer[outer.length - 1]
  return {
    length: doc.getCellParagraphLengthByPath(sec, para, json),
    text: (length) => doc.getTextInCellByPath(sec, para, json, 0, length),
    rect: (offset) => JSON.parse(doc.getCursorRectByPath(sec, para, json, offset)),
    // the engine reads a cell paragraph's formatting by table and cell number only
    props: () =>
      outer.length === 1
        ? JSON.parse(
            doc.getCellParaPropertiesAt(
              sec,
              para,
              last.controlIndex,
              last.cellIndex,
              last.cellParaIndex,
            ),
          )
        : null,
  }
}

/**
 * The kind and leader of the tab stop a tab goes to: the first stop past where the tab
 * starts, as the engine looks it up; past the last stop, the right edge when the paragraph
 * says so, and otherwise the plain left tab every so often. Tab stops are measured from
 * where the paragraph's margin starts: the start of the first line less its indent.
 */
function tabStopOf(props, firstX, startX) {
  if (!props) return { type: TAB_LEFT, fill: 0 }
  const toPx = (value) => (Number(value) || 0) / PARA_UNIT_SCALE / HWPUNIT_PER_PX
  const indent = toPx(props.indent)
  const origin = firstX - toPx(props.marginLeft) - Math.max(indent, 0)
  const at = startX - origin
  for (const stop of props.tabStops ?? []) {
    if (toPx(stop.position) > at + TAB_STOP_SLACK_PX) {
      return { type: Number(stop.type) || 0, fill: Number(stop.fill) || 0 }
    }
  }
  return props.tabAutoRight ? { type: TAB_RIGHT, fill: 0 } : { type: TAB_LEFT, fill: 0 }
}

/**
 * How wide the editor lays a tab out, in HWPUNIT, and the kind and leader of its stop; null
 * when the tab is not where the file says (another paragraph, another number of tabs), or
 * cannot be measured (it ends a line, its caret positions are not on one line).
 */
export function measureTab(doc, spot) {
  const paragraph = tabParagraph(doc, spot)
  if (!paragraph || paragraph.length <= 0) return null
  const text = paragraph.text(paragraph.length)
  const offsets = []
  for (let i = text.indexOf('\t'); i >= 0; i = text.indexOf('\t', i + 1)) offsets.push(i)
  if (offsets.length !== spot.tabs || spot.tab >= offsets.length) return null
  const offset = offsets[spot.tab]
  const before = paragraph.rect(offset)
  const after = paragraph.rect(offset + 1)
  if (before.pageIndex !== after.pageIndex || Math.abs(before.y - after.y) > 1) return null
  const width = Math.round((after.x - before.x) * HWPUNIT_PER_PX)
  if (!(width > 0)) return null
  const first = paragraph.rect(0)
  const { type, fill } = tabStopOf(paragraph.props(), first.x, before.x)
  return { width, type, fill }
}
