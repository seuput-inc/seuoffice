import { mkdtempSync, readFileSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { expect, test, type Page } from '@playwright/test'
import { strToU8, zipSync } from 'fflate'

/**
 * A host asks the page for a picture of its first page (`doc.thumbnail()`) as soon as `opened`
 * settled. Every format answers a PNG of the asked width that shows the page, opened for
 * viewing or for editing. A document that is not open has none: `thumbnail_unsupported`.
 */
const repo = fileURLToPath(new URL('../../..', import.meta.url))
const at = (path: string): string => resolve(repo, path)

const SHEET_NS = 'xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"'
const PACKAGE_RELS = 'http://schemas.openxmlformats.org/package/2006/relationships'
const OFFICE_RELS = 'http://schemas.openxmlformats.org/officeDocument/2006/relationships'
const SHEET_TYPES = 'application/vnd.openxmlformats-officedocument.spreadsheetml'

/** A workbook whose cells A1:H20 are filled red: the grid alone, without its cells, has no red */
function redBlockXlsx(): string {
  const columns = ['A', 'B', 'C', 'D', 'E', 'F', 'G', 'H']
  const rows = Array.from({ length: 20 }, (_, row) => {
    const cells = columns.map((column) => `<c r="${column}${row + 1}" s="1"><v>${row + 1}</v></c>`)
    return `<row r="${row + 1}">${cells.join('')}</row>`
  })
  const xml = (body: string): Uint8Array => strToU8(`<?xml version="1.0" encoding="UTF-8"?>${body}`)
  const zip = zipSync({
    '[Content_Types].xml': xml(
      '<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">' +
        '<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>' +
        '<Default Extension="xml" ContentType="application/xml"/>' +
        `<Override PartName="/xl/workbook.xml" ContentType="${SHEET_TYPES}.sheet.main+xml"/>` +
        `<Override PartName="/xl/worksheets/sheet1.xml" ContentType="${SHEET_TYPES}.worksheet+xml"/>` +
        `<Override PartName="/xl/styles.xml" ContentType="${SHEET_TYPES}.styles+xml"/></Types>`,
    ),
    '_rels/.rels': xml(
      `<Relationships xmlns="${PACKAGE_RELS}"><Relationship Id="rId1" Type="${OFFICE_RELS}/officeDocument" Target="xl/workbook.xml"/></Relationships>`,
    ),
    'xl/workbook.xml': xml(
      `<workbook ${SHEET_NS} xmlns:r="${OFFICE_RELS}"><sheets><sheet name="Red" sheetId="1" r:id="rId1"/></sheets></workbook>`,
    ),
    'xl/_rels/workbook.xml.rels': xml(
      `<Relationships xmlns="${PACKAGE_RELS}">` +
        `<Relationship Id="rId1" Type="${OFFICE_RELS}/worksheet" Target="worksheets/sheet1.xml"/>` +
        `<Relationship Id="rId2" Type="${OFFICE_RELS}/styles" Target="styles.xml"/></Relationships>`,
    ),
    'xl/styles.xml': xml(
      `<styleSheet ${SHEET_NS}><fonts count="1"><font/></fonts>` +
        '<fills count="3"><fill><patternFill patternType="none"/></fill><fill><patternFill patternType="gray125"/></fill>' +
        '<fill><patternFill patternType="solid"><fgColor rgb="FFFF0000"/><bgColor indexed="64"/></patternFill></fill></fills>' +
        '<borders count="1"><border/></borders><cellStyleXfs count="1"><xf/></cellStyleXfs>' +
        '<cellXfs count="2"><xf/><xf fillId="2" applyFill="1"/></cellXfs></styleSheet>',
    ),
    'xl/worksheets/sheet1.xml': xml(
      `<worksheet ${SHEET_NS}><dimension ref="A1:H20"/><sheetData>${rows.join('')}</sheetData></worksheet>`,
    ),
  })
  const file = join(mkdtempSync(join(tmpdir(), 'seuoffice-thumbnail-')), 'red-block.xlsx')
  writeFileSync(file, zip)
  return file
}

interface Format {
  name: string
  file: () => string
  /** the least share of the image the document's red cells take */
  red?: number
}

const FORMATS: Format[] = [
  { name: 'pdf', file: () => at('apps/pdf/tests/fixtures/plain-sample.pdf') },
  {
    name: 'pptx',
    file: () => at('packages/pptx-engine/tests/fixtures/01_standard_business.pptx'),
  },
  { name: 'hwp', file: () => at('apps/hwp/tests/fixtures/format-copy-table.hwp') },
  { name: 'hwpx', file: () => at('apps/hwp/tests/fixtures/generated-form-tables.hwpx') },
  { name: 'docx', file: () => at('fixtures/generated/kitchen-sink.docx') },
  { name: 'xlsx', file: redBlockXlsx, red: 0.05 },
]

const PNG_SIGNATURE = [137, 80, 78, 71, 13, 10, 26, 10]

/** What the host got: the answer's own numbers, and what the browser decodes from its bytes */
interface Shot {
  width: number
  height: number
  /** the bytes the host got */
  png: number[]
  decodedWidth: number
  decodedHeight: number
  /** share of pixels that differ from the most common colour: 0 for a blank image */
  inked: number
  /** share of pixels that are plainly red */
  red: number
  /** every pixel is fully opaque: the page is on paper, not see-through */
  opaque: boolean
}

/** Opens `file` in the launcher and returns once its `opened` settled, without waiting further */
async function open(page: Page, file: string, mode: 'view' | 'edit'): Promise<void> {
  await page.goto('/')
  if (mode === 'edit') await page.check('#edit')
  await page.setInputFiles('#file', {
    name: file.replaceAll('\\', '/').split('/').pop()!,
    mimeType: 'application/octet-stream',
    buffer: readFileSync(file),
  })
  await expect
    .poll(() => page.evaluate(() => window.seuLauncher?.mode ?? null), { timeout: 40_000 })
    .toBe(mode === 'edit' ? 'locked' : 'view')
}

/** `doc.thumbnail()` as the host calls it: the image as the browser reads it, or the refusal's code */
async function thumbnail(page: Page, width?: number): Promise<Shot | { code: string }> {
  return page.evaluate(async (asked) => {
    let image
    try {
      image = await window.seuLauncher!.thumbnail(
        asked === undefined ? undefined : { width: asked },
      )
    } catch (error) {
      // a SeuofficeEmbedError names why; anything else is a broken host and reads as itself
      if (error instanceof Error && 'code' in error && typeof error.code === 'string') {
        return { code: error.code }
      }
      return { code: String(error) }
    }
    const bitmap = await createImageBitmap(
      new Blob([image.bytes as BlobPart], { type: 'image/png' }),
    )
    const canvas = document.createElement('canvas')
    canvas.width = bitmap.width
    canvas.height = bitmap.height
    const context = canvas.getContext('2d')!
    context.drawImage(bitmap, 0, 0)
    const data = context.getImageData(0, 0, bitmap.width, bitmap.height).data
    const counts = new Map<number, number>()
    let opaque = true
    let red = 0
    for (let i = 0; i < data.length; i += 4) {
      const key = (data[i]! << 16) | (data[i + 1]! << 8) | data[i + 2]!
      counts.set(key, (counts.get(key) ?? 0) + 1)
      if (data[i + 3] !== 255) opaque = false
      if (data[i]! > 200 && data[i + 1]! < 90 && data[i + 2]! < 90) red++
    }
    return {
      width: image.width,
      height: image.height,
      png: Array.from(image.bytes),
      decodedWidth: bitmap.width,
      decodedHeight: bitmap.height,
      inked: 1 - Math.max(...counts.values()) / (data.length / 4),
      red: red / (data.length / 4),
      opaque,
    }
  }, width)
}

function expectPage(shot: Shot | { code: string }, width: number): asserts shot is Shot {
  expect(shot).not.toHaveProperty('code')
  const image = shot as Shot
  expect(image.png.slice(0, 8)).toEqual(PNG_SIGNATURE)
  expect(image.width).toBe(width)
  expect(image.decodedWidth).toBe(width)
  expect(image.height).toBeGreaterThan(0)
  expect(image.decodedHeight).toBe(image.height)
  expect(image.opaque).toBe(true)
  // a page with text or lines on it: more than a few stray pixels differ from the paper
  expect(image.inked).toBeGreaterThan(0.002)
}

for (const format of FORMATS) {
  for (const mode of ['view', 'edit'] as const) {
    test(`${format.name} gives its first page right after it opened for ${mode}`, async ({
      page,
    }, testInfo) => {
      const problems: string[] = []
      page.on('pageerror', (error) => problems.push(error.message))
      await open(page, format.file(), mode)
      const shot = await thumbnail(page, 320)
      expectPage(shot, 320)
      if (format.red !== undefined) expect(shot.red).toBeGreaterThan(format.red)
      await testInfo.attach(`${format.name}-${mode}`, {
        body: Buffer.from(shot.png),
        contentType: 'image/png',
      })
      expect(problems).toEqual([])
    })
  }
}

test('a width the host does not name is 480, and one out of range moves to the nearer end', async ({
  page,
}) => {
  await open(page, FORMATS[0]!.file(), 'view')
  const [unnamed, narrow, wide] = [
    await thumbnail(page),
    await thumbnail(page, 10),
    await thumbnail(page, 5000),
  ]
  expectPage(unnamed, 480)
  expectPage(wide, 1024)
  // the same page at every width: as tall for its width, to the pixel rounding
  expectPage(narrow, 64)
  const shape = unnamed.height / unnamed.width
  expect(Math.abs(wide.height / wide.width - shape)).toBeLessThan(0.01)
  expect(Math.abs(narrow.height / narrow.width - shape)).toBeLessThan(0.02)
})

test('a Word page zoomed to fit a narrow screen is still one whole page', async ({ page }) => {
  // the page is refitted to the screen after it opened, while the request waits for pagination
  await page.setViewportSize({ width: 390, height: 844 })
  await open(page, at('fixtures/generated/kitchen-sink.docx'), 'view')
  const shot = await thumbnail(page)
  expectPage(shot, 480)
  // the fixture is A4 portrait
  expect(shot.height / shot.width).toBeCloseTo(297 / 210, 1)
})

test('a document that has not opened yet has no first page', async ({ page }) => {
  await page.goto('/')
  const file = FORMATS[0]!.file()
  await page.setInputFiles('#file', {
    name: 'not-open-yet.pdf',
    mimeType: 'application/octet-stream',
    buffer: readFileSync(file),
  })
  const code = await page.evaluate(async () => {
    for (;;) {
      // the launcher's frame is there, and the page inside it has not answered `opened`
      if (window.seuLauncher!.mode !== null) return 'the document opened before it was asked'
      try {
        await window.seuLauncher!.thumbnail()
        return 'an image'
      } catch (error) {
        if (error instanceof Error && 'code' in error) return String(error.code)
      }
      // the launcher is still reading the file: no document to ask
      await new Promise((resolve) => setTimeout(resolve, 10))
    }
  })
  expect(code).toBe('thumbnail_unsupported')
})

test('closing the document refuses the image it was drawing', async ({ page }) => {
  // a Word page waits for its pagination to settle, so the request is still open when it closes
  await open(page, at('fixtures/generated/kitchen-sink.docx'), 'view')
  const code = await page.evaluate(async () => {
    const asked = window.seuLauncher!.thumbnail().then(
      () => 'an image',
      (error: unknown) =>
        error instanceof Error && 'code' in error ? String(error.code) : String(error),
    )
    // dropping another file on the launcher destroys the document that is open
    const dropped = new DataTransfer()
    dropped.items.add(new File(['not a document'], 'next.txt'))
    document.dispatchEvent(new DragEvent('drop', { dataTransfer: dropped }))
    return asked
  })
  expect(code).toBe('thumbnail_unsupported')
})
