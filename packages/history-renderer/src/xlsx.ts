import {
  parseAddress,
  parseRange,
  type RangeBounds,
} from '@seuoffice/xlsx-gateway/domain/cell-address'
import { formulaReads, type FormulaRead } from '@seuoffice/xlsx-gateway/gateway/formula-reads'
import { decodeXlsxEscapes } from '@seuoffice/xlsx-gateway/gateway/xlsx-escapes'
import {
  readBasicWorkbook,
  planCellEditsToXlsx,
  type CellEdit,
} from '@seuoffice/xlsx-gateway/gateway/xlsx-gateway'
import { applyPageSetupState } from '@seuoffice/xlsx-gateway/gateway/xlsx-page-setup'
import { canonicalXmlSpelling } from '@seuoffice/xlsx-gateway/gateway/xlsx-xml-scan'
import JSZip from 'jszip'
import { SaxesParser } from 'saxes'
import {
  checkText,
  decode,
  encode,
  invalid,
  unsupported,
  type Literal,
  type SheetOp,
  type Write,
} from './xlsx-mutations'
import type { ByteHistoryAdapter } from './adapter'

const sheetNs = 'http://schemas.openxmlformats.org/spreadsheetml/2006/main'
const relNs = 'http://schemas.openxmlformats.org/officeDocument/2006/relationships'
const packageNs = 'http://schemas.openxmlformats.org/package/2006/'
const spreadsheet = 'application/vnd.openxmlformats-officedocument.spreadsheetml.'
const workbookPath = 'xl/workbook.xml'
const workbookRels = 'xl/_rels/workbook.xml.rels'
type Sheet = {
  name: string
  path: string
  base: Map<string, Literal>
  edits: Map<string, Literal>
  locked: Set<string>
  ranges: RangeBounds[]
}
type SourceCells = Map<string, number | null | undefined>
type Element = { name: string; attrs: Record<string, string>; children: Element[]; text: string }

const defaultWorkbookView = Object.entries({
  visibility: 'visible',
  minimized: '0',
  showHorizontalScroll: '1',
  showVerticalScroll: '1',
  showSheetTabs: '1',
  tabRatio: '600',
  firstSheet: '0',
  activeTab: '0',
  autoFilterDateGrouping: '1',
})

function parsePart(xml: string, path: string): Element {
  // The gateway patches serialized tags. Do not let a comment, alternate namespace or
  // entity-spelled address mean something different to its scanner and to the XML reader.
  if (xml.includes('<!') || /<[^<>]*&/.test(xml)) unsupported('unsupported XLSX XML spelling')
  const parser = new SaxesParser({ xmlns: false })
  const stack: Element[] = []
  let root: Element | undefined
  let count = 0
  parser.on('error', () => invalid('malformed XLSX XML'))
  parser.on('processinginstruction', () => unsupported('XLSX processing instruction'))
  parser.on('xmldecl', ({ encoding, version }) => {
    if (version !== '1.0' || (encoding && encoding.toLowerCase() !== 'utf-8'))
      unsupported('XLSX encoding')
  })
  parser.on('opentag', ({ name, attributes }) => {
    const attrCount = Object.keys(attributes).length
    if (
      ++count > 40000 ||
      stack.length > 8 ||
      (attrCount > 8 &&
        !(
          path === workbookPath &&
          stack.length === 2 &&
          stack[0]!.name === 'workbook' &&
          stack[1]!.name === 'bookViews' &&
          name === 'workbookView' &&
          attrCount === defaultWorkbookView.length &&
          defaultWorkbookView.every(([key, value]) => attributes[key] === value)
        ))
    )
      unsupported('XLSX XML complexity')
    if (
      !/^[A-Za-z][A-Za-z0-9]*$/.test(name) ||
      (stack.length && Object.hasOwn(attributes, 'xmlns'))
    )
      unsupported('unsupported XLSX namespace')
    const node: Element = { name, attrs: attributes, children: [], text: '' }
    if (stack.length) stack[stack.length - 1]!.children.push(node)
    else root = node
    stack.push(node)
  })
  parser.on('text', (text) => {
    if (stack.length) stack[stack.length - 1]!.text += text
  })
  parser.on('closetag', () => {
    stack.pop()
  })
  parser.write(xml).close()
  return root ?? invalid('missing XLSX XML root')
}

function shape(node: Element, name: string, attrs: string[], children: string[] = []) {
  if (
    node.name !== name ||
    Object.keys(node.attrs).some((key) => !attrs.includes(key)) ||
    node.children.some((child) => !children.includes(child.name)) ||
    (!['t', 'v', 'f', 'definedName'].includes(name) && /\S/.test(node.text))
  )
    unsupported(`unsupported XLSX ${node.name}`)
  return node.attrs
}

function one(node: Element, name: string) {
  const matches = node.children.filter((child) => child.name === name)
  if (matches.length !== 1) return invalid(`expected one XLSX ${name}`)
  return matches[0]!
}

function rootNamespace(
  node: Element,
  name: string,
  namespace: string,
  children: string[],
  workbook = false,
) {
  const attrs = shape(node, name, workbook ? ['xmlns', 'xmlns:r'] : ['xmlns'], children)
  if (attrs.xmlns !== namespace || (attrs['xmlns:r'] !== undefined && attrs['xmlns:r'] !== relNs))
    unsupported('unsupported XLSX namespace')
}

function relationships(node: Element) {
  rootNamespace(node, 'Relationships', `${packageNs}relationships`, ['Relationship'])
  const table = new Map<string, { target: string; type: string }>()
  for (const child of node.children) {
    const { Id, Target, Type } = shape(child, 'Relationship', ['Id', 'Target', 'Type'])
    if (!Id || !/^[A-Za-z_][\w.-]{0,63}$/.test(Id) || !Target || !Type || table.has(Id))
      invalid('invalid XLSX relationship')
    table.set(Id, { target: Target, type: Type })
  }
  return table
}

function checkTypes(node: Element, types: Map<string, string>, paths: readonly string[]) {
  rootNamespace(node, 'Types', `${packageNs}content-types`, ['Default', 'Override'])
  const expected = new Map(types)
  expected.set('xml', 'application/xml')
  expected.set('rels', 'application/vnd.openxmlformats-package.relationships+xml')
  const seen = new Set<string>()
  for (const child of node.children) {
    const key = child.name === 'Default' ? 'Extension' : 'PartName'
    const attrs = shape(child, child.name, [key, 'ContentType'])
    const part = attrs[key]
    if (
      !part ||
      !attrs.ContentType ||
      seen.has(part) ||
      (expected.has(part)
        ? expected.get(part) !== attrs.ContentType
        : child.name === 'Override' && !paths.includes(part.slice(1))) ||
      (child.name === 'Override') !== part.startsWith('/')
    )
      unsupported('unsupported XLSX content type')
    seen.add(part)
    expected.delete(part)
  }
  if (expected.size) invalid('missing XLSX content type')
}

function checkAddress(address: string) {
  // Every Sheets workbook opens with at least this 1000 by 26 grid. Do not invent
  // an editor grid size for writes outside it (univer-sync.ts).
  if (!address || !/^[A-Z][1-9]\d{0,3}$/.test(address))
    return unsupported('outside the XLSX history grid')
  const pos = parseAddress(address)
  if (pos.row >= 1000) unsupported('outside the XLSX history grid')
  return pos
}

// Only scalar arithmetic and scalar reductions have no spill footprint. Even for those,
// writing a precedent would leave the cached result stale without a calculation engine.
const operand = String.raw`(?:(?:'[A-Za-z0-9 ._-]+'|[A-Za-z_][A-Za-z0-9_.]*)!)?\$?[A-Z]{1,3}\$?[1-9]\d{0,6}`
const number = String.raw`(?:\d+(?:\.\d*)?|\.\d+)(?:[Ee][+-]?\d+)?`
const scalar = `(?:${operand}|${number})`
const argument = `(?:${operand}(?::${operand})?|${number})`
const scalarFormula = new RegExp(
  `^(?:[-+]?${scalar}(?:\\s*[-+*/^]\\s*[-+]?${scalar})*|(?:SUM|MIN|MAX|COUNT|AVERAGE)\\(\\s*${argument}(?:\\s*,\\s*${argument})*\\s*\\))$`,
  'i',
)

function literalText(inline: Element): string | undefined {
  shape(inline, inline.name, [], ['t', 'r', 'rPh', 'phoneticPr'])
  if (inline.children.length !== 1 || inline.children[0]!.name !== 't') {
    // Rich and phonetic text stays untouched. The patcher scans serialized tags:
    // never admit a second row, cell or formula hidden inside a run.
    for (const child of inline.children) {
      if (child.name === 'r' || child.name === 'rPh') {
        shape(child, child.name, child.name === 'rPh' ? ['sb', 'eb'] : [], ['rPr', 't'])
        for (const run of child.children) {
          if (run.name === 't') shape(run, 't', ['xml:space'])
          else {
            shape(
              run,
              'rPr',
              [],
              [
                'rFont',
                'charset',
                'family',
                'b',
                'i',
                'strike',
                'outline',
                'shadow',
                'condense',
                'extend',
                'color',
                'sz',
                'u',
                'vertAlign',
                'scheme',
              ],
            )
            for (const prop of run.children)
              shape(prop, prop.name, ['val', 'rgb', 'auto', 'indexed', 'theme', 'tint'])
          }
        }
      } else if (child.name === 't') shape(child, 't', ['xml:space'])
      else shape(child, 'phoneticPr', ['fontId', 'type', 'alignment'])
    }
    return undefined
  }
  const t = inline.children[0]!
  const space = shape(t, 't', ['xml:space'])['xml:space']
  if (space !== undefined && space !== 'default' && space !== 'preserve') return undefined
  let start = 0
  let end = t.text.length
  if (space !== 'preserve') {
    while (start < end && ' \t\r\n'.includes(t.text[start]!)) start++
    while (end > start && ' \t\r\n'.includes(t.text[end - 1]!)) end--
  }
  return checkText(decodeXlsxEscapes(t.text.slice(start, end)).replace(/\r\n?/g, '\n'))
}

function inspectSheet(
  root: Element,
  sheet: Sheet,
  shared: readonly (string | undefined)[],
  reads: FormulaRead[],
  styles: number,
) {
  const attrs = shape(
    root,
    'worksheet',
    ['xmlns', 'xmlns:r'],
    [
      'sheetPr',
      'dimension',
      'sheetData',
      'mergeCells',
      'cols',
      'sheetViews',
      'sheetFormatPr',
      'pageMargins',
    ],
  )
  if (attrs.xmlns !== sheetNs || (attrs['xmlns:r'] !== undefined && attrs['xmlns:r'] !== relNs))
    unsupported('unsupported XLSX worksheet namespace')
  const count = new Set<string>()
  let dimension: RangeBounds | undefined
  sheet.base.set('showGridlines', 1)
  sheet.base.set('rightToLeft', 0)
  for (const section of root.children) {
    if (count.has(section.name)) invalid('duplicate XLSX worksheet section')
    count.add(section.name)
    switch (section.name) {
      case 'sheetPr': {
        shape(section, 'sheetPr', [], ['outlinePr', 'pageSetUpPr'])
        const outline = shape(one(section, 'outlinePr'), 'outlinePr', [
          'summaryBelow',
          'summaryRight',
        ])
        if (outline.summaryBelow !== '1' || outline.summaryRight !== '1')
          unsupported('unsupported XLSX outline properties')
        shape(one(section, 'pageSetUpPr'), 'pageSetUpPr', [])
        break
      }
      case 'dimension': {
        const { ref } = shape(section, 'dimension', ['ref'])
        if (!ref || !/^[A-Z][1-9]\d{0,3}(?::[A-Z][1-9]\d{0,3})?$/.test(ref))
          return unsupported('unsupported XLSX dimension')
        const [start, end = start!] = ref.split(':').map(checkAddress)
        if (start!.row > end.row || start!.column > end.column) invalid('reversed XLSX dimension')
        dimension = parseRange(ref)
        break
      }
      case 'mergeCells':
        shape(section, 'mergeCells', ['count'], ['mergeCell'])
        if (section.children.length > 1024) unsupported('too many XLSX merge ranges')
        for (const merge of section.children) {
          const { ref } = shape(merge, 'mergeCell', ['ref'])
          if (!ref || !/^[A-Z][1-9]\d{0,3}:[A-Z][1-9]\d{0,3}$/.test(ref))
            return unsupported('unsupported XLSX merge')
          const [start, end] = ref.split(':').map(checkAddress)
          if (start!.row > end!.row || start!.column > end!.column) invalid('reversed XLSX merge')
          sheet.ranges.push(parseRange(ref))
        }
        break
      case 'sheetViews': {
        shape(section, 'sheetViews', [], ['sheetView'])
        if (section.children.length !== 1) unsupported('multiple XLSX sheet views')
        const view = one(section, 'sheetView')
        shape(
          view,
          'sheetView',
          [
            'workbookViewId',
            'rightToLeft',
            'showGridLines',
            'zoomScale',
            'zoomScaleNormal',
            'tabSelected',
            'showZeros',
            'showRowColHeaders',
          ],
          ['pane', 'selection'],
        )
        if (view.attrs.workbookViewId !== '0') unsupported('unsupported XLSX view id')
        for (const child of view.children)
          shape(
            child,
            child.name,
            child.name === 'pane'
              ? ['xSplit', 'ySplit', 'topLeftCell', 'activePane', 'state']
              : ['pane', 'activeCell', 'activeCellId', 'sqref'],
          )
        for (const [key, attr] of [
          ['showGridlines', 'showGridLines'],
          ['rightToLeft', 'rightToLeft'],
        ] as const) {
          const flag = view.attrs[attr]
          if (flag === undefined) continue
          if (!['0', '1', 'false', 'true'].includes(flag)) invalid('invalid XLSX view flag')
          sheet.base.set(key, Number(flag === '1' || flag === 'true'))
        }
        break
      }
      case 'cols':
        shape(section, 'cols', [], ['col'])
        for (const col of section.children) {
          const { min, max } = shape(col, 'col', [
            'min',
            'max',
            'width',
            'customWidth',
            'style',
            'hidden',
            'bestFit',
            'outlineLevel',
          ])
          if (!/^[1-9]\d*$/.test(min) || !/^[1-9]\d*$/.test(max) || +min > +max || +max > 26)
            unsupported('outside the XLSX history columns')
        }
        break
      case 'sheetFormatPr':
        shape(section, 'sheetFormatPr', [
          'defaultRowHeight',
          'defaultColWidth',
          'baseColWidth',
          'customHeight',
          'zeroHeight',
          'outlineLevelRow',
          'outlineLevelCol',
        ])
        break
      case 'pageMargins':
        shape(section, 'pageMargins', ['left', 'right', 'top', 'bottom', 'header', 'footer'])
        break
    }
  }
  const data = one(root, 'sheetData')
  shape(data, 'sheetData', [], ['row'])
  let lastRow = 0
  // An undefined entry is a non-numeric cell; numbers are checked against the gateway reader.
  const addresses: SourceCells = new Map()
  for (const row of data.children) {
    const { r } = shape(
      row,
      'row',
      ['r', 'spans', 's', 'customFormat', 'ht', 'customHeight', 'hidden', 'outlineLevel'],
      ['c'],
    )
    if (!r || !/^[1-9]\d{0,3}$/.test(r) || +r <= lastRow || +r > 1000)
      unsupported('unordered or out-of-grid XLSX row')
    lastRow = +r
    let lastColumn = -1
    for (const cell of row.children) {
      const { r: address, s, t } = shape(cell, 'c', ['r', 't', 's'], ['is', 'v', 'f'])
      const pos = checkAddress(address)
      if (
        pos.row + 1 !== lastRow ||
        pos.column <= lastColumn ||
        (dimension &&
          (pos.row < dimension.startRow ||
            pos.row > dimension.endRow ||
            pos.column < dimension.startColumn ||
            pos.column > dimension.endColumn))
      )
        invalid('inconsistent XLSX cell address')
      addresses.set(address, undefined)
      lastColumn = pos.column
      if (s !== undefined && (!/^\d+$/.test(s) || +s >= styles)) invalid('invalid XLSX cell style')
      const formulas = cell.children.filter((item) => item.name === 'f')
      if (formulas.length) {
        const formula = one(cell, 'f')
        if (cell.children.some((child) => child.name === 'is') || cell.children.length > 2)
          invalid('invalid XLSX formula cell')
        shape(formula, 'f', [])
        for (const child of cell.children) if (child.name === 'v') shape(child, 'v', [])
        if (formula.text.length > 8192 || !scalarFormula.test(formula.text))
          unsupported('formula may spill or require recalculation')
        const parsed = formulaReads(formula.text)
        if (parsed.unknown || parsed.names.length || reads.length + parsed.reads.length > 1024)
          unsupported('unknown XLSX formula precedents')
        for (const read of parsed.reads) reads.push({ ...read, sheet: read.sheet ?? sheet.name })
        sheet.locked.add(address)
      } else if (t === 'inlineStr') {
        if (cell.children.length !== 1) invalid('invalid XLSX inline string')
        const inline = one(cell, 'is')
        const value = literalText(inline)
        if (value === undefined) sheet.locked.add(address)
        else sheet.base.set(address, value)
      } else if (cell.children.length) {
        if (cell.children.length !== 1) invalid('invalid XLSX cell contents')
        const v = one(cell, 'v')
        shape(v, 'v', [])
        if (t === 's') {
          if (!/^(?:0|[1-9]\d*)$/.test(v.text) || +v.text >= shared.length)
            invalid('invalid XLSX shared string index')
          const value = shared[+v.text]
          if (value === undefined) sheet.locked.add(address)
          else sheet.base.set(address, value)
        } else if (t === undefined || t === 'n') {
          if (
            !/^[+-]?(?:\d+(?:\.\d*)?|\.\d+)(?:[eE][+-]?\d+)?$/.test(v.text) ||
            !Number.isFinite(+v.text)
          )
            invalid('invalid XLSX number')
          addresses.set(address, +v.text)
        } else sheet.locked.add(address)
      } else if (t !== undefined && t !== 'n') sheet.locked.add(address)
      else addresses.set(address, null)
    }
  }
  return addresses
}

/** The shared archive gate admits these bytes before a JSZip member is read. */
export async function openXlsx(bytes: Uint8Array): Promise<ByteHistoryAdapter<SheetOp>> {
  const zip = await JSZip.loadAsync(bytes)
  const paths = Object.keys(zip.files).filter((path) => !zip.files[path]!.dir)
  const textParts = new Map<string, string>()
  const decoder = new TextDecoder('utf-8', { fatal: true })
  async function text(path: string) {
    const file = zip.file(path) ?? invalid('missing XLSX part')
    return decoder.decode(await file.async('uint8array'))
  }
  async function part(path: string) {
    const xml = await text(path)
    const root = parsePart(xml, path)
    textParts.set(path, canonicalXmlSpelling(xml))
    return root
  }
  const packageRels = [...relationships(await part('_rels/.rels')).values()]
  const office = packageRels.filter((rel) => rel.type === `${relNs}/officeDocument`)
  if (office.length !== 1 || office[0]!.target !== workbookPath) unsupported('not an XLSX workbook')
  for (const rel of packageRels) {
    if (rel === office[0]) continue
    if (
      !/^docProps\/[\w-]+\.xml$/.test(rel.target) ||
      ![
        `${relNs}/extended-properties`,
        `${relNs}/custom-properties`,
        `${packageNs}relationships/metadata/core-properties`,
      ].includes(rel.type)
    )
      unsupported('unsupported XLSX package relationship')
    if (!zip.file(rel.target)) invalid('missing XLSX document properties')
  }
  const workbook = await part(workbookPath)
  rootNamespace(
    workbook,
    'workbook',
    sheetNs,
    ['sheets', 'calcPr', 'definedNames', 'bookViews', 'workbookPr', 'workbookProtection'],
    true,
  )
  if (workbook.children.some((child) => child.name === 'workbookProtection'))
    shape(one(workbook, 'workbookProtection'), 'workbookProtection', [])
  if (workbook.children.some((child) => child.name === 'calcPr')) {
    shape(one(workbook, 'calcPr'), 'calcPr', [
      'calcId',
      'calcMode',
      'fullCalcOnLoad',
      'forceFullCalc',
    ])
  }
  if (workbook.children.some((child) => child.name === 'definedNames')) {
    const defined = one(workbook, 'definedNames')
    shape(defined, 'definedNames', [], ['definedName'])
    for (const name of defined.children)
      shape(name, 'definedName', ['name', 'localSheetId', 'hidden'])
  }
  if (workbook.children.some((child) => child.name === 'workbookPr')) {
    shape(one(workbook, 'workbookPr'), 'workbookPr', ['date1904', 'defaultThemeVersion'])
  }
  if (workbook.children.some((child) => child.name === 'bookViews')) {
    const views = one(workbook, 'bookViews')
    shape(views, 'bookViews', [], ['workbookView'])
    const view = one(views, 'workbookView')
    shape(
      view,
      'workbookView',
      Object.keys(view.attrs).length === defaultWorkbookView.length
        ? defaultWorkbookView.map(([key]) => key)
        : [
            'activeTab',
            'firstSheet',
            'xWindow',
            'yWindow',
            'windowWidth',
            'windowHeight',
            'visibility',
          ],
    )
  }
  const list = one(workbook, 'sheets')
  shape(list, 'sheets', [], ['sheet'])
  const rels = relationships(await part(workbookRels))
  const types = new Map([[`/${workbookPath}`, `${spreadsheet}sheet.main+xml`]])
  const names = new Map<string, Sheet>()
  const sheets = new Map<string, Sheet>()
  for (const item of list.children) {
    const {
      name,
      sheetId,
      state,
      'r:id': rid,
      'xmlns:r': localNs,
    } = shape(item, 'sheet', ['name', 'sheetId', 'r:id', 'state', 'xmlns:r'])
    if (
      (localNs ?? workbook.attrs['xmlns:r']) !== relNs ||
      (state !== undefined && state !== 'visible')
    )
      unsupported('unsupported XLSX sheet namespace or visibility')
    if (
      !name ||
      !/^[\p{L}\p{N} ._-]{1,31}$/u.test(name) ||
      !sheetId ||
      !/^[1-9]\d{0,9}$/.test(sheetId) ||
      +sheetId > 4294967295
    )
      unsupported('unsupported XLSX sheet identity')
    const id = `sheet-${sheetId}`
    const rel = rels.get(rid) ?? invalid('unresolved XLSX worksheet')
    if (
      rel.type !== `${relNs}/worksheet` ||
      !/^(?:\/xl\/)?worksheets\/[\w-]+\.xml$/.test(rel.target)
    )
      unsupported('unsupported XLSX worksheet target')
    const path = rel.target.startsWith('/') ? rel.target.slice(1) : `xl/${rel.target}`
    if (names.has(name.toLowerCase()) || sheets.has(id) || types.has(`/${path}`))
      invalid('ambiguous XLSX sheet identity')
    const sheet: Sheet = {
      name,
      path,
      base: new Map(),
      edits: new Map(),
      locked: new Set(),
      ranges: [],
    }
    names.set(name.toLowerCase(), sheet)
    rels.delete(rid)
    types.set(`/${path}`, `${spreadsheet}worksheet+xml`)
    sheets.set(id, sheet)
  }
  if (!sheets.size) invalid('empty XLSX workbook')
  let styles = 1
  let shared: (string | undefined)[] = []
  for (const { target, type } of rels.values()) {
    const isStyle = target === 'styles.xml' && type === `${relNs}/styles`
    const isTheme = target === 'theme/theme1.xml' && type === `${relNs}/theme`
    const isStrings = target === 'sharedStrings.xml' && type === `${relNs}/sharedStrings`
    if (!isStyle && !isTheme && !isStrings) unsupported('unsupported XLSX workbook relationship')
    const path = `xl/${target}`
    if (types.has(`/${path}`) || !zip.file(path))
      invalid('missing or duplicate XLSX relationship target')
    if (isStyle) {
      const root = await part(path)
      if (root.name !== 'styleSheet' || root.attrs.xmlns !== sheetNs)
        unsupported('unsupported XLSX stylesheet')
      const formats = one(root, 'cellXfs')
      shape(formats, 'cellXfs', ['count'], ['xf'])
      styles = formats.children.length
    } else if (isStrings) {
      const root = await part(path)
      if (shape(root, 'sst', ['xmlns', 'count', 'uniqueCount'], ['si']).xmlns !== sheetNs)
        unsupported('unsupported XLSX shared strings')
      shared = root.children.map(literalText)
    }
    types.set(
      `/${path}`,
      isStyle
        ? `${spreadsheet}styles+xml`
        : isStrings
          ? `${spreadsheet}sharedStrings+xml`
          : 'application/vnd.openxmlformats-officedocument.theme+xml',
    )
  }
  checkTypes(await part('[Content_Types].xml'), types, paths)
  // These conventional names are opened by the reader/planner even without a relationship.
  if (
    ['xl/sharedStrings.xml', 'xl/styles.xml', 'xl/calcChain.xml'].some(
      (p) => zip.file(p) && !types.has(`/${p}`),
    )
  )
    unsupported('unexpected implicit XLSX part')
  const reads: FormulaRead[] = []
  const addresses = new Map<string, SourceCells>()
  for (const [id, sheet] of sheets)
    addresses.set(id, inspectSheet(await part(sheet.path), sheet, shared, reads, styles))
  for (const range of reads) {
    const source =
      names.get(range.sheet!.toLowerCase()) ?? invalid('unresolved XLSX formula reference')
    source.ranges.push(range)
  }
  const { snapshot } = await readBasicWorkbook(
    Buffer.from(bytes.buffer, bytes.byteOffset, bytes.byteLength),
  )
  if (snapshot.sheets.length !== sheets.size) unsupported('XLSX reader cannot resolve every sheet')
  for (const tab of snapshot.sheets) {
    const sheet = sheets.get(tab.id)
    if (!sheet || sheet.name !== tab.name) return unsupported('ambiguous XLSX reader sheet')
    const expected = addresses.get(tab.id)!
    const cells = Object.entries(tab.cells)
    if (cells.length !== expected.size) unsupported('XLSX reader cannot resolve every cell')
    for (const [key, cell] of cells) {
      if (!expected.has(key)) unsupported('ambiguous XLSX reader address')
      if (sheet.locked.has(key) || sheet.base.has(key)) continue
      if (
        cell.formula !== undefined ||
        (cell.value !== null && typeof cell.value !== 'number') ||
        cell.value !== expected.get(key)
      )
        return unsupported('XLSX reader literal mismatch')
      sheet.base.set(key, cell.value)
    }
  }
  function writable({ sheet: id, key }: Write) {
    const sheet = sheets.get(id) ?? invalid('unknown Sheets subUnitId')
    if (key === 'showGridlines' || key === 'rightToLeft') return sheet
    const { row, column } = parseAddress(key)
    if (
      sheet.locked.has(key) ||
      sheet.ranges.some(
        (area) =>
          row >= area.startRow &&
          row <= area.endRow &&
          column >= area.startColumn &&
          column <= area.endColumn,
      )
    )
      unsupported('cannot edit a formula, its precedent, a merged or a nonliteral XLSX cell')
    return sheet
  }
  return {
    decode,
    encode,
    integrate(remote, local) {
      if (local.length) invalid('unexpected pending Sheets edits')
      for (const op of remote) {
        for (const write of op.writes) {
          const sheet = writable(write)
          // Restoring a file literal also restores its original XML (shared string, style, etc.).
          if (write.value === (sheet.base.get(write.key) ?? null)) sheet.edits.delete(write.key)
          else sheet.edits.set(write.key, write.value)
        }
      }
      return []
    },
    captureUndo(op) {
      const targets = new Map<string, { before: Write; last: Literal }>()
      for (const write of op.writes) {
        const sheet = writable(write)
        const key = `${write.sheet}/${write.key}`
        const target = targets.get(key)
        if (target) target.last = write.value
        else {
          const value = sheet.edits.has(write.key)
            ? sheet.edits.get(write.key)!
            : (sheet.base.get(write.key) ?? null)
          targets.set(key, { before: { ...write, value }, last: write.value })
        }
      }
      const entries = [...targets.values()]
      if (entries.every(({ before, last }) => before.value === last))
        return unsupported('not an invertible Sheets replacement')
      return (later) =>
        later.some((op) => op.writes.some((w) => targets.has(`${w.sheet}/${w.key}`)))
          ? null
          : [{ id: crypto.randomUUID(), writes: entries.map(({ before }) => before) }]
    },
    async produceFile() {
      const edits: CellEdit[] = []
      const views = new Map<
        string,
        { sheetName: string; showGridlines?: boolean; rightToLeft?: boolean }
      >()
      for (const sheet of sheets.values()) {
        for (const [key, value] of sheet.edits) {
          if (key === 'showGridlines' || key === 'rightToLeft') {
            const view = views.get(sheet.path) ?? { sheetName: sheet.name }
            view[key] = value === 1
            views.set(sheet.path, view)
          } else
            edits.push({
              sheetName: sheet.name,
              ...parseAddress(key),
              writeValue: true,
              cell: { value },
            })
        }
      }
      if (!edits.length && !views.size) return { fileName: 'history.xlsx', bytes }
      const plan = await planCellEditsToXlsx(
        {
          paths: async () => paths,
          has: async (path) => zip.file(path) !== null,
          readText: async (path) =>
            textParts.get(path) ?? unsupported('unexpected XLSX planner read'),
        },
        edits,
      )
      if (plan.addedEntries.length || plan.removedEntries.length)
        unsupported('unexpected XLSX part change')
      const replacements = new Map(plan.replaced)
      for (const [path, view] of views)
        replacements.set(
          path,
          applyPageSetupState(replacements.get(path) ?? textParts.get(path)!, view),
        )
      for (const [path, content] of replacements) {
        const original = zip.file(path) ?? invalid('missing XLSX replacement part')
        zip.file(path, content, {
          createFolders: false,
          date: original.date,
          comment: original.comment,
          unixPermissions: original.unixPermissions,
          dosPermissions: original.dosPermissions,
        })
      }
      return {
        fileName: 'history.xlsx',
        bytes: await zip.generateAsync({
          type: 'uint8array',
          compression: 'DEFLATE',
          platform: zip.files[workbookPath]!.unixPermissions != null ? 'UNIX' : 'DOS',
        }),
      }
    },
  }
}
