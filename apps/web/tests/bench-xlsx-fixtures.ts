// Synthetic workbooks for the xlsx view benchmark (bench-xlsx.spec.ts), written
// the way openpyxl writes them (inline strings, numbers, formulas without a
// cached value) so they match the shapes a host reported: five sizes from 200
// cells to 8 million, one with styles, merged cells, formulas and a frozen
// pane, and a small one whose italic cells check the fonts they are drawn
// with. Deterministic: the same name always gives the same bytes.
import { once } from 'node:events'
import { createWriteStream, existsSync, mkdirSync, renameSync } from 'node:fs'
import { dirname } from 'node:path'
import { crc32, createDeflateRaw } from 'node:zlib'

export type FixtureName = 'small' | 'medium' | 'large' | 'styled10' | 'xl' | 'italic'

interface SheetSpec {
  rows: number
  cols: number
  /** a column index (0-based) holds text when this says so; else a number */
  text(col: number): boolean
  styled: 'none' | 'medium' | 'styled10' | 'italic'
}

interface WorkbookSpec {
  sheets: number
  sheet: SheetSpec
}

const SPECS: Record<FixtureName, WorkbookSpec> = {
  small: { sheets: 1, sheet: { rows: 200, cols: 10, text: (c) => c % 3 === 0, styled: 'none' } },
  medium: {
    sheets: 5,
    sheet: { rows: 5_000, cols: 30, text: (c) => c % 3 === 0, styled: 'medium' },
  },
  large: { sheets: 1, sheet: { rows: 50_000, cols: 40, text: (c) => c % 3 === 0, styled: 'none' } },
  styled10: {
    sheets: 10,
    sheet: { rows: 10_000, cols: 30, text: (c) => c % 3 === 0, styled: 'styled10' },
  },
  xl: { sheets: 1, sheet: { rows: 200_000, cols: 40, text: (c) => c % 8 === 0, styled: 'none' } },
  // column A in an italic cell style, column D in a bold italic rich-text run
  italic: { sheets: 1, sheet: { rows: 200, cols: 10, text: (c) => c % 3 === 0, styled: 'italic' } },
}

/** the rows of each workbook's sheets: its last row's first cell reads "항목 <rows>-0" */
export const FIXTURE_ROWS = Object.fromEntries(
  Object.entries(SPECS).map(([name, spec]) => [name, spec.sheet.rows]),
) as Record<FixtureName, number>

/** mulberry32: a small seeded generator, so every run writes the same numbers */
function random(seed: number): () => number {
  let state = seed >>> 0
  return () => {
    state = (state + 0x6d2b79f5) >>> 0
    let t = state
    t = Math.imul(t ^ (t >>> 15), t | 1)
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61)
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296
  }
}

function column(index: number): string {
  let name = ''
  for (let n = index + 1; n > 0; n = Math.floor((n - 1) / 26)) {
    name = String.fromCharCode(65 + ((n - 1) % 26)) + name
  }
  return name
}

// cellXfs: 0 plain, 1 header (bold, blue fill, centred), then every mix of
// alternate fill (+1), thin top/bottom border (+2) and #,##0.00 (+4) from 2 on
const STYLES =
  '<styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">' +
  '<numFmts count="1"><numFmt numFmtId="164" formatCode="#,##0.00"/></numFmts>' +
  '<fonts count="2"><font><name val="Calibri"/><family val="2"/><color theme="1"/><sz val="11"/><scheme val="minor"/></font><font><b val="1"/></font></fonts>' +
  '<fills count="4"><fill><patternFill/></fill><fill><patternFill patternType="gray125"/></fill>' +
  '<fill><patternFill patternType="solid"><fgColor rgb="00DDEBF7"/></patternFill></fill>' +
  '<fill><patternFill patternType="solid"><fgColor rgb="00F2F2F2"/></patternFill></fill></fills>' +
  '<borders count="2"><border><left/><right/><top/><bottom/><diagonal/></border>' +
  '<border><top style="thin"><color rgb="00999999"/></top><bottom style="thin"><color rgb="00999999"/></bottom></border></borders>' +
  '<cellStyleXfs count="1"><xf numFmtId="0" fontId="0" fillId="0" borderId="0"/></cellStyleXfs>' +
  '<cellXfs count="10"><xf numFmtId="0" fontId="0" fillId="0" borderId="0" xfId="0"/>' +
  '<xf numFmtId="0" fontId="1" fillId="2" borderId="0" xfId="0" applyFont="1" applyFill="1" applyAlignment="1"><alignment horizontal="center"/></xf>' +
  Array.from({ length: 8 }, (_, mix) => {
    const fill = mix & 1 ? 3 : 0
    const border = mix & 2 ? 1 : 0
    const format = mix & 4 ? 164 : 0
    return `<xf numFmtId="${format}" fontId="0" fillId="${fill}" borderId="${border}" xfId="0" applyNumberFormat="${mix & 4 ? 1 : 0}" applyFill="${mix & 1}" applyBorder="${mix & 2 ? 1 : 0}"/>`
  }).join('') +
  '</cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles></styleSheet>'

/** STYLES plus font 2 (italic Calibri) and cellXfs 10 using it */
const ITALIC_STYLES = STYLES.replace('<fonts count="2">', '<fonts count="3">')
  .replace('</fonts>', '<font><i val="1"/><name val="Calibri"/><sz val="11"/></font></fonts>')
  .replace('<cellXfs count="10">', '<cellXfs count="11">')
  .replace(
    '</cellXfs>',
    '<xf numFmtId="0" fontId="2" fillId="0" borderId="0" xfId="0" applyFont="1"/></cellXfs>',
  )

function cellStyle(spec: SheetSpec, row: number, col: number): number {
  if (spec.styled === 'italic') return row > 1 && col === 0 ? 10 : 0
  if (spec.styled === 'none') return 0
  if (row === 1) return 1
  let mix = 0
  if (spec.styled === 'styled10' && row % 2 === 0 && col < 7) mix |= 1
  if (spec.styled === 'styled10' ? row % 10 === 0 : (row - 2) % 10 === 0) mix |= 2
  if (col >= 1 && col <= 4) mix |= 4
  return mix === 0 ? 0 : 2 + mix
}

function formula(spec: SheetSpec, row: number, col: number): string | null {
  const last = spec.cols - 1
  if (spec.styled === 'medium') {
    if (col === last) return `SUM(B${row}:E${row})`
    if (col === last - 1) return `IF(B${row}&gt;500,"높음","낮음")`
  }
  if (spec.styled === 'styled10') {
    if (col === last) return `SUM(B${row}:F${row})`
    if (col === last - 1) return `IF(B${row}&gt;500,"높음","낮음")`
    if (col === last - 2) {
      return row % 50 === 0 ? `VLOOKUP(A${row},A$2:B$${spec.rows},2,FALSE)` : `B${row}*1.1`
    }
  }
  return null
}

function rowXml(spec: SheetSpec, row: number, next: () => number): string {
  let xml = `<row r="${row}">`
  for (let col = 0; col < spec.cols; col++) {
    const ref = column(col) + row
    const style = cellStyle(spec, row, col)
    const s = style === 0 ? '' : ` s="${style}"`
    if (row === 1) {
      xml += `<c r="${ref}"${s} t="inlineStr"><is><t>열${col + 1}</t></is></c>`
      continue
    }
    const f = formula(spec, row, col)
    if (f !== null) xml += `<c r="${ref}"${s}><f>${f}</f><v></v></c>`
    else if (spec.styled === 'italic' && col === 3)
      xml += `<c r="${ref}"${s} t="inlineStr"><is><r><rPr><b/><i/></rPr><t>항목 ${row}-${col}</t></r></is></c>`
    else if (spec.text(col))
      xml += `<c r="${ref}"${s} t="inlineStr"><is><t>항목 ${row}-${col}</t></is></c>`
    else xml += `<c r="${ref}"${s} t="n"><v>${Math.round(next() * 100_000) / 100}</v></c>`
  }
  return xml + '</row>'
}

function* sheetXml(spec: SheetSpec, next: () => number): Generator<string> {
  const pane =
    spec.styled === 'styled10'
      ? '<pane xSplit="1" ySplit="1" topLeftCell="B2" activePane="bottomRight" state="frozen"/><selection pane="bottomRight" activeCell="B2" sqref="B2"/>'
      : '<selection activeCell="A1" sqref="A1"/>'
  yield '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>' +
    '<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">' +
    `<dimension ref="A1:${column(spec.cols - 1)}${spec.rows}"/>` +
    `<sheetViews><sheetView workbookViewId="0">${pane}</sheetView></sheetViews>` +
    '<sheetFormatPr baseColWidth="8" defaultRowHeight="15"/><sheetData>'
  let chunk = ''
  for (let row = 1; row <= spec.rows; row++) {
    chunk += rowXml(spec, row, next)
    if (row % 500 === 0) {
      yield chunk
      chunk = ''
    }
  }
  chunk += '</sheetData>'
  if (spec.styled === 'styled10') {
    const merges: string[] = []
    for (let row = 2; row <= spec.rows; row += 25)
      merges.push(`<mergeCell ref="A${row}:B${row + 1}"/>`)
    chunk += `<mergeCells count="${merges.length}">${merges.join('')}</mergeCells>`
  }
  yield chunk +
    '<pageMargins left="0.75" right="0.75" top="1" bottom="1" header="0.5" footer="0.5"/></worksheet>'
}

function packageParts({ sheets, sheet }: WorkbookSpec): Record<string, string> {
  const ids = Array.from({ length: sheets }, (_, i) => i + 1)
  return {
    '[Content_Types].xml':
      '<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">' +
      '<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/>' +
      '<Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>' +
      '<Override PartName="/xl/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml"/>' +
      ids
        .map(
          (i) =>
            `<Override PartName="/xl/worksheets/sheet${i}.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>`,
        )
        .join('') +
      '</Types>',
    '_rels/.rels':
      '<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">' +
      '<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>',
    'xl/workbook.xml':
      '<?xml version="1.0" encoding="UTF-8" standalone="yes"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets>' +
      ids.map((i) => `<sheet name="시트${i}" sheetId="${i}" r:id="rId${i}"/>`).join('') +
      '</sheets></workbook>',
    'xl/_rels/workbook.xml.rels':
      '<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">' +
      ids
        .map(
          (i) =>
            `<Relationship Id="rId${i}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet${i}.xml"/>`,
        )
        .join('') +
      `<Relationship Id="rId${sheets + 1}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/></Relationships>`,
    'xl/styles.xml': sheet.styled === 'italic' ? ITALIC_STYLES : STYLES,
  }
}

/** One zip entry: its name, and its text in pieces */
type Entry = [name: string, text: Iterable<string>]

/**
 * A zip of `entries`, deflated by zlib as Python's zipfile does (fflate's deflate is some 10%
 * larger, which pushes the 8-million-cell workbook over the embed's limit). Sizes and CRC follow
 * each entry in a data descriptor, so nothing is held in memory but one piece of text.
 */
async function writeZip(path: string, entries: Iterable<Entry>): Promise<void> {
  const out = createWriteStream(path)
  let offset = 0
  const write = async (bytes: Uint8Array): Promise<void> => {
    offset += bytes.length
    if (!out.write(bytes)) await once(out, 'drain')
  }
  const central: Buffer[] = []
  for (const [name, text] of entries) {
    const nameBytes = Buffer.from(name)
    const local = Buffer.alloc(30)
    local.writeUInt32LE(0x04034b50, 0)
    local.writeUInt16LE(20, 4)
    local.writeUInt16LE(0x0808, 6) // data descriptor follows; UTF-8 names
    local.writeUInt16LE(8, 8) // deflate
    local.writeUInt16LE(nameBytes.length, 26)
    const start = offset
    await write(local)
    await write(nameBytes)
    let crc = 0
    let size = 0
    let compressed = 0
    const deflate = createDeflateRaw({ level: 6 })
    const drained = (async () => {
      for await (const piece of deflate as AsyncIterable<Buffer>) {
        compressed += piece.length
        await write(piece)
      }
    })()
    for (const piece of text) {
      const bytes = Buffer.from(piece)
      crc = crc32(bytes, crc)
      size += bytes.length
      if (!deflate.write(bytes)) await once(deflate, 'drain')
    }
    deflate.end()
    await drained
    const descriptor = Buffer.alloc(16)
    descriptor.writeUInt32LE(0x08074b50, 0)
    descriptor.writeUInt32LE(crc, 4)
    descriptor.writeUInt32LE(compressed, 8)
    descriptor.writeUInt32LE(size, 12)
    await write(descriptor)
    const record = Buffer.alloc(46)
    record.writeUInt32LE(0x02014b50, 0)
    record.writeUInt16LE(20, 4)
    record.writeUInt16LE(20, 6)
    record.writeUInt16LE(0x0808, 8)
    record.writeUInt16LE(8, 10)
    record.writeUInt32LE(crc, 16)
    record.writeUInt32LE(compressed, 20)
    record.writeUInt32LE(size, 24)
    record.writeUInt16LE(nameBytes.length, 28)
    record.writeUInt32LE(start, 42)
    central.push(record, nameBytes)
  }
  const directory = Buffer.concat(central)
  const end = Buffer.alloc(22)
  end.writeUInt32LE(0x06054b50, 0)
  end.writeUInt16LE(central.length / 2, 8)
  end.writeUInt16LE(central.length / 2, 10)
  end.writeUInt32LE(directory.length, 12)
  end.writeUInt32LE(offset, 16)
  await write(directory)
  await write(end)
  out.end()
  await once(out, 'finish')
}

/** Writes the named workbook to `path` unless it is already there; resolves to `path` */
export async function benchFixture(name: FixtureName, path: string): Promise<string> {
  if (existsSync(path)) return path
  mkdirSync(dirname(path), { recursive: true })
  const spec = SPECS[name]
  const next = random(Object.keys(SPECS).indexOf(name) + 1)
  const entries: Entry[] = Object.entries(packageParts(spec)).map(([part, text]) => [part, [text]])
  for (let sheet = 1; sheet <= spec.sheets; sheet++) {
    // generated as the zip writes it: the sheets share one sequence of numbers, in order
    entries.push([
      `xl/worksheets/sheet${sheet}.xml`,
      { [Symbol.iterator]: () => sheetXml(spec.sheet, next) },
    ])
  }
  const partial = `${path}.partial`
  await writeZip(partial, entries)
  renameSync(partial, path)
  return path
}
