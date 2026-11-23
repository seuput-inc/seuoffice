import { HistoryError, type Json } from '@seuoffice/collab/client'
import { formatAddress, parseAddress } from '@seuoffice/xlsx-gateway/domain/cell-address'
import { MINIMAL_STYLESHEET_XML } from '@seuoffice/xlsx-gateway/gateway/xlsx-default-styles'
import { DEFAULT_THEME_XML } from '@seuoffice/xlsx-gateway/gateway/xlsx-default-theme'
import { decodeXlsxEscapes } from '@seuoffice/xlsx-gateway/gateway/xlsx-escapes'
import { planCellEditsToXlsx, type CellEdit } from '@seuoffice/xlsx-gateway/gateway/xlsx-gateway'
import { canonicalXmlSpelling } from '@seuoffice/xlsx-gateway/gateway/xlsx-xml-scan'
import JSZip from 'jszip'
import { SaxesParser } from 'saxes'
import { checkWireMutation } from '../../../apps/sheets/src/web/collab-wire-check'
import type { ByteHistoryAdapter } from './adapter'

const sheetNs = 'http://schemas.openxmlformats.org/spreadsheetml/2006/main'
const relNs = 'http://schemas.openxmlformats.org/officeDocument/2006/relationships'
const packageNs = 'http://schemas.openxmlformats.org/package/2006/'
const spreadsheet = 'application/vnd.openxmlformats-officedocument.spreadsheetml.'
const setValues = 'sheet.mutation.set-range-values'
const workbookPath = 'xl/workbook.xml'
const workbookRels = 'xl/_rels/workbook.xml.rels'
const invalid = (detail: string): never => {
  throw new HistoryError('invalid_history', detail)
}
const unsupported = (detail: string): never => {
  throw new HistoryError('unsupported', detail)
}

type Literal = string | number | null
type CellWrite = {
  id: string
  sheet: string
  row: number
  column: number
  address: string
  value: Literal
}
type Sheet = { name: string; cells: Map<string, Literal>; edits: Map<string, CellEdit> }
type Element = { name: string; attrs: Record<string, string>; children: Element[]; text: string }

function parsePart(xml: string): Element {
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
    if (++count > 40000 || stack.length > 8 || Object.keys(attributes).length > 8)
      unsupported('XLSX XML complexity')
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
    (name !== 't' && name !== 'v' && /\S/.test(node.text))
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
  if (attrs.xmlns !== namespace || (workbook && attrs['xmlns:r'] !== relNs))
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

function checkTypes(node: Element, types: Map<string, string>) {
  rootNamespace(node, 'Types', `${packageNs}content-types`, ['Default', 'Override'])
  const expected = new Map(types)
  expected.set('xml', 'application/xml')
  expected.set('rels', 'application/vnd.openxmlformats-package.relationships+xml')
  for (const child of node.children) {
    const key = child.name === 'Default' ? 'Extension' : 'PartName'
    const attrs = shape(child, child.name, [key, 'ContentType'])
    const part = attrs[key]
    if (
      !part ||
      !attrs.ContentType ||
      expected.get(part) !== attrs.ContentType ||
      (child.name === 'Override') !== part.startsWith('/')
    )
      unsupported('unsupported XLSX content type')
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

function checkText(text: string) {
  // Do not emit lossy UTF-8 or depend on the engine's CR/CRLF normalization.
  if (text.length > 32767 || /\r|\p{Cs}/u.test(text)) unsupported('unsupported XLSX literal text')
  return text
}

function readCells(node: Element) {
  rootNamespace(node, 'worksheet', sheetNs, ['dimension', 'sheetData'])
  let dimension: { row: number; column: number } | undefined
  if (node.children.some((child) => child.name === 'dimension')) {
    if (node.children[0]?.name !== 'dimension') invalid('misplaced XLSX dimension')
    const { ref } = shape(one(node, 'dimension'), 'dimension', ['ref'])
    if (!ref || !/^A1(?::[A-Z][1-9]\d{0,3})?$/.test(ref)) unsupported('unsupported XLSX dimension')
    dimension = checkAddress(ref.split(':').at(-1)!)
  }
  const data = one(node, 'sheetData')
  shape(data, 'sheetData', [], ['row'])
  const cells = new Map<string, Literal>()
  let lastRow = 0
  for (const row of data.children) {
    const { r } = shape(row, 'row', ['r'], ['c'])
    if (!r || !/^[1-9]\d{0,3}$/.test(r) || +r <= lastRow || +r > 1000)
      unsupported('unordered or out-of-grid XLSX row')
    lastRow = +r
    let lastColumn = -1
    for (const cell of row.children) {
      const { r: address, t } = shape(cell, 'c', ['r', 't'], ['is', 'v'])
      const pos = checkAddress(address)
      if (
        pos.row + 1 !== lastRow ||
        pos.column <= lastColumn ||
        (dimension && (pos.row > dimension.row || pos.column > dimension.column))
      )
        invalid('inconsistent XLSX cell address')
      lastColumn = pos.column
      let value: Literal = null
      if (t === 'inlineStr') {
        if (cell.children.length !== 1) invalid('invalid inline string')
        const inline = one(cell, 'is')
        shape(inline, 'is', [], ['t'])
        const text = one(inline, 't')
        const space = shape(text, 't', ['xml:space'])['xml:space']
        if (space !== undefined && space !== 'preserve') unsupported('unsupported XLSX whitespace')
        let start = 0
        let end = text.text.length
        if (space !== 'preserve') {
          while (start < end && ' \t\r\n'.includes(text.text[start]!)) start++
          while (end > start && ' \t\r\n'.includes(text.text[end - 1]!)) end--
        }
        value = checkText(decodeXlsxEscapes(text.text.slice(start, end)).replace(/\r\n?/g, '\n'))
      } else if (t === undefined || t === 'n') {
        if (cell.children.length) {
          if (cell.children.length !== 1) invalid('invalid XLSX numeric cell')
          const number = one(cell, 'v')
          shape(number, 'v', [])
          if (
            !/^[+-]?(?:\d+(?:\.\d*)?|\.\d+)(?:[eE][+-]?\d+)?$/.test(number.text) ||
            !Number.isFinite(+number.text)
          )
            invalid('invalid XLSX number')
          value = +number.text
        }
      } else unsupported('unsupported XLSX cell type')
      cells.set(address, value)
    }
  }
  return cells
}

function record(value: Json | undefined): value is Record<string, Json> {
  return !!value && typeof value === 'object' && !Array.isArray(value)
}

function entry(value: Json | undefined) {
  if (!record(value)) return unsupported('expected one XLSX cell')
  const entries = Object.entries(value)
  if (entries.length !== 1) return unsupported('expected one XLSX cell')
  return entries[0]!
}

function decode(json: Json): CellWrite {
  if (!record(json) || typeof json.id !== 'string' || !Array.isArray(json.m))
    return invalid('invalid Sheets op')
  if (Object.keys(json).some((key) => key !== 'id' && key !== 'm') || json.m.length !== 1)
    return unsupported('expected one Sheets mutation')
  const m = json.m[0]
  if (!Array.isArray(m) || m.length !== 2) return invalid('invalid Sheets mutation')
  const [kind, params] = m
  if (typeof kind !== 'string' || !record(params)) return invalid('invalid Sheets mutation')
  if (kind !== setValues) return unsupported('not a Sheets literal write')
  if (Object.keys(params).some((key) => !['subUnitId', 'trigger', 'cellValue'].includes(key)))
    return unsupported('unexpected Sheets write parameter')
  checkWireMutation(kind, params)
  const { trigger, subUnitId, cellValue, options } = params
  if (
    typeof subUnitId !== 'string' ||
    options !== undefined ||
    (trigger !== undefined && trigger !== 'sheet.command.set-range-values')
  )
    return unsupported('not a direct Sheets write')
  const [row, columns] = entry(cellValue)
  const [column, cell] = entry(columns)
  if (
    !record(cell) ||
    Object.entries(cell).some(
      ([key, value]) =>
        key !== 'v' && key !== 't' && !(value === null && ['f', 'si', 'p'].includes(key)),
    )
  )
    return unsupported('not a plain Sheets cell')
  const { v, t } = cell
  if (typeof v === 'string') {
    // Without an explicit STRING, Univer infers numbers, booleans or FORCE_STRING.
    if (
      t !== 1 &&
      (t !== undefined ||
        (v.trim() !== '' && !Number.isNaN(Number(v))) ||
        /^(true|false)$/i.test(v))
    )
      return unsupported('ambiguous Sheets text type')
    checkText(v)
  } else if (typeof v === 'number') {
    if (t !== undefined && t !== 2) return unsupported('unsupported Sheets numeric type')
  } else if (v !== null || t !== undefined) return unsupported('unsupported Sheets value')
  if (+row >= 1000 || +column >= 26) return unsupported('outside the XLSX history grid')
  return {
    id: json.id,
    sheet: subUnitId,
    row: +row,
    column: +column,
    address: formatAddress(+row, +column),
    value: v,
  }
}

/** The shared archive gate admits these bytes before a JSZip member is read. */
export async function openXlsx(bytes: Uint8Array): Promise<ByteHistoryAdapter<CellWrite>> {
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
    const root = parsePart(xml)
    textParts.set(path, canonicalXmlSpelling(xml))
    return root
  }
  const packageRels = [...relationships(await part('_rels/.rels')).values()]
  if (
    packageRels.length !== 1 ||
    packageRels[0]!.type !== `${relNs}/officeDocument` ||
    packageRels[0]!.target !== workbookPath
  )
    unsupported('not a plain XLSX package')
  const workbook = await part(workbookPath)
  rootNamespace(workbook, 'workbook', sheetNs, ['sheets', 'calcPr'], true)
  if (workbook.children.some((child) => child.name === 'calcPr')) {
    if (workbook.children.at(-1)?.name !== 'calcPr')
      invalid('misplaced XLSX calculation properties')
    if (shape(one(workbook, 'calcPr'), 'calcPr', ['fullCalcOnLoad']).fullCalcOnLoad !== '1')
      unsupported('unsupported XLSX calculation properties')
  }
  const list = one(workbook, 'sheets')
  shape(list, 'sheets', [], ['sheet'])
  const rels = relationships(await part(workbookRels))
  const types = new Map([[`/${workbookPath}`, `${spreadsheet}sheet.main+xml`]])
  const names = new Set<string>()
  const sheets = new Map<string, Sheet>()
  for (const item of list.children) {
    const { name, sheetId, 'r:id': rid } = shape(item, 'sheet', ['name', 'sheetId', 'r:id'])
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
    if (rel.type !== `${relNs}/worksheet` || !/^worksheets\/[\w-]+\.xml$/.test(rel.target))
      unsupported('unsupported XLSX worksheet target')
    const path = `xl/${rel.target}`
    if (names.has(name.toLowerCase()) || sheets.has(id) || types.has(`/${path}`))
      invalid('ambiguous XLSX sheet identity')
    names.add(name.toLowerCase())
    rels.delete(rid)
    types.set(`/${path}`, `${spreadsheet}worksheet+xml`)
    sheets.set(id, { name, cells: readCells(await part(path)), edits: new Map() })
  }
  if (!sheets.size) invalid('empty XLSX workbook')
  for (const { target, type } of rels.values()) {
    const isStyle = target === 'styles.xml' && type === `${relNs}/styles`
    const isTheme = target === 'theme/theme1.xml' && type === `${relNs}/theme`
    if (!isStyle && !isTheme) unsupported('unsupported XLSX relationship')
    const key = `/xl/${target}`
    if (
      types.has(key) ||
      (await text(key.slice(1))) !== (isStyle ? MINIMAL_STYLESHEET_XML : DEFAULT_THEME_XML)
    )
      unsupported('not the default XLSX theme or styles')
    types.set(
      key,
      isStyle
        ? `${spreadsheet}styles+xml`
        : 'application/vnd.openxmlformats-officedocument.theme+xml',
    )
  }
  checkTypes(await part('[Content_Types].xml'), types)
  if (paths.some((path) => !textParts.has(path) && !types.has(`/${path}`)))
    unsupported('unsupported XLSX part')
  const sheetFor = (op: CellWrite) => sheets.get(op.sheet) ?? invalid('unknown Sheets subUnitId')
  return {
    decode,
    encode({ id, sheet, row, column, value }): Json {
      const cell: Json =
        value === null ? { v: null } : { v: value, t: typeof value === 'string' ? 1 : 2 }
      return {
        id,
        m: [[setValues, { subUnitId: sheet, cellValue: { [row]: { [column]: cell } } }]],
      }
    },
    integrate(remote, local) {
      if (local.length) invalid('unexpected pending Sheets edits')
      for (const op of remote) {
        const sheet = sheetFor(op)
        sheet.cells.set(op.address, op.value)
        sheet.edits.set(op.address, {
          sheetName: sheet.name,
          row: op.row,
          column: op.column,
          writeValue: true,
          cell: { value: op.value },
        })
      }
      return []
    },
    captureUndo(op) {
      const value = sheetFor(op).cells.get(op.address) ?? null
      if (op.value === null || op.value === value)
        return unsupported('not an invertible Sheets literal replacement')
      return (later) =>
        later.some((next) => next.sheet === op.sheet && next.address === op.address)
          ? null
          : [{ ...op, id: crypto.randomUUID(), value }]
    },
    async produceFile() {
      const edits = [...sheets.values()].flatMap((sheet) => [...sheet.edits.values()])
      if (!edits.length) return { fileName: 'history.xlsx', bytes }
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
      for (const [path, content] of plan.replaced) {
        const original = zip.file(path) ?? invalid('missing XLSX replacement part')
        zip.file(path, content, { createFolders: false, date: original.date })
      }
      return {
        fileName: 'history.xlsx',
        bytes: await zip.generateAsync({ type: 'uint8array', compression: 'DEFLATE' }),
      }
    },
  }
}
