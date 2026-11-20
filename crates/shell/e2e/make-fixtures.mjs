#!/usr/bin/env node
// Builds the fixture folder of the real-app tests (gate=e2e): small documents the repo already
// carries, copied under the names the scenarios use, plus the few the repo has no small copy of
// (a large workbook, a multi-page PDF, a mind map, a Markdown and an HTML file).
//
//   node crates/shell/e2e/make-fixtures.mjs <dest>
//
// The workflow copies <dest> once per scenario into that scenario's own work folder, so a scenario
// that saves over its file never touches the pristine set.

import { copyFileSync, mkdirSync, readFileSync, statSync, writeFileSync } from 'node:fs'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import JSZip from 'jszip'
import { PDFDocument, StandardFonts } from 'pdf-lib'

const root = resolve(dirname(fileURLToPath(import.meta.url)), '../../..')
const from = (...parts) => join(root, ...parts)

/** Documents copied as they are: [name in the fixture folder, repo path]. */
const COPIES = [
  ['docs.docx', 'fixtures/generated/simple.docx'],
  ['docs-ime.docx', 'fixtures/generated/simple.docx'],
  ['docs-paste.docx', 'fixtures/generated/simple.docx'],
  ['docs-quit.docx', 'fixtures/generated/simple.docx'],
  ['sheets.xlsx', 'apps/shell/build/shell-new/blank.xlsx'],
  ['sheets-ime.xlsx', 'apps/shell/build/shell-new/blank.xlsx'],
  ['slides.pptx', 'packages/pptx-engine/tests/fixtures/01_standard_business.pptx'],
  ['hwp.hwp', 'apps/hwp/src/renderer/assets/blank.hwp'],
  ['hwpx.hwpx', 'apps/hwp/tests/fixtures/page-number-pages.hwpx'],
  ['diagram.drawio', 'e2e/assets/diagram/sample.drawio'],
]

export const BIG_ROWS = 50000
export const BIG_COLUMNS = 10

const MARKDOWN = '# SeuOffice e2e\n\nA paragraph the test types into.\n'
const HTML =
  '<!doctype html>\n<html>\n<head><meta charset="utf-8"><title>SeuOffice e2e</title></head>\n' +
  '<body><h1>SeuOffice e2e</h1><p>A paragraph the test types into.</p></body>\n</html>\n'

const escapeXml = (text) =>
  text.replaceAll('&', '&amp;').replaceAll('<', '&lt;').replaceAll('>', '&gt;')

/** The blank workbook of the app with its one sheet replaced by BIG_ROWS x BIG_COLUMNS cells. */
export async function bigWorkbook(templatePath, rows = BIG_ROWS, columns = BIG_COLUMNS) {
  const zip = await JSZip.loadAsync(readFileSync(templatePath))
  const letter = (column) => String.fromCharCode(65 + column)
  const cell = (column, row, value) =>
    typeof value === 'number'
      ? `<c r="${letter(column)}${row}"><v>${value}</v></c>`
      : `<c r="${letter(column)}${row}" t="inlineStr"><is><t>${escapeXml(value)}</t></is></c>`
  const parts = [
    '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>',
    '<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">',
    `<dimension ref="A1:${letter(columns - 1)}${rows + 1}"/><sheetData>`,
  ]
  const header = []
  for (let column = 0; column < columns; column += 1) header.push(cell(column, 1, `col${column}`))
  parts.push(`<row r="1">${header.join('')}</row>`)
  for (let row = 2; row <= rows + 1; row += 1) {
    const cells = [cell(0, row, `row ${row}`)]
    for (let column = 1; column < columns; column += 1) cells.push(cell(column, row, row * column))
    parts.push(`<row r="${row}">${cells.join('')}</row>`)
  }
  parts.push('</sheetData></worksheet>')
  zip.file('xl/worksheets/sheet1.xml', parts.join(''))
  return zip.generateAsync({ type: 'nodebuffer', compression: 'DEFLATE' })
}

/** A PDF of `pages` pages; page n says `SEU-E2E-PDF page n`. */
export async function multiPagePdf(pages = 5) {
  const pdf = await PDFDocument.create()
  const font = await pdf.embedFont(StandardFonts.Helvetica)
  for (let page = 1; page <= pages; page += 1) {
    pdf.addPage([595, 842]).drawText(`SEU-E2E-PDF page ${page}`, { x: 72, y: 760, size: 24, font })
  }
  return Buffer.from(await pdf.save({ useObjectStreams: false }))
}

/** A one-sheet mind map with a central topic and two branches, in the format's own layout. */
export async function mindMap() {
  const topic = (title, children) => ({
    id: `t-${title.replaceAll(' ', '-')}`,
    class: 'topic',
    title,
    ...(children ? { children: { attached: children } } : {}),
  })
  const sheets = [
    {
      id: 'sheet-e2e',
      class: 'sheet',
      title: 'Sheet 1',
      rootTopic: topic('SeuOffice e2e', [topic('Branch one'), topic('Branch two')]),
    },
  ]
  const zip = new JSZip()
  zip.file('content.json', JSON.stringify(sheets))
  zip.file('metadata.json', JSON.stringify({ creator: { name: 'seuoffice-e2e', version: '1' } }))
  zip.file(
    'manifest.json',
    JSON.stringify({ 'file-entries': { 'content.json': {}, 'metadata.json': {} } }),
  )
  return zip.generateAsync({ type: 'nodebuffer', compression: 'DEFLATE' })
}

export async function makeFixtures(dest) {
  mkdirSync(dest, { recursive: true })
  const made = []
  const put = (name, content) => {
    writeFileSync(join(dest, name), content)
    made.push(name)
  }
  for (const [name, source] of COPIES) {
    copyFileSync(from(source), join(dest, name))
    made.push(name)
  }
  put('big.xlsx', await bigWorkbook(from('apps/shell/build/shell-new/blank.xlsx')))
  put('multipage.pdf', await multiPagePdf())
  put('mindmap.xmind', await mindMap())
  put('markdown.md', MARKDOWN)
  put('html.html', HTML)
  return made.map((name) => `${name} (${statSync(join(dest, name)).size} bytes)`)
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const dest = process.argv[2]
  if (!dest) {
    console.error('usage: make-fixtures.mjs <dest>')
    process.exit(64)
  }
  for (const line of await makeFixtures(resolve(dest))) console.log(line)
}
