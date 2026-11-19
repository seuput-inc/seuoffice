import { mkdtempSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { crc32, deflateSync } from 'node:zlib'
import { expect, test, type Page } from '@playwright/test'
import { strToU8, zipSync } from 'fflate'

/**
 * A workbook with a picture floating over its grid opens in the embed with its cells and the
 * picture, for viewing and for editing. A picture whose bytes the package does not hold is left
 * out, and the cells still show.
 */
const SHEET_NS = 'xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"'
const PACKAGE_RELS = 'http://schemas.openxmlformats.org/package/2006/relationships'
const OFFICE_RELS = 'http://schemas.openxmlformats.org/officeDocument/2006/relationships'
const SHEET_TYPES = 'application/vnd.openxmlformats-officedocument.spreadsheetml'
const DRAWING_TYPE = 'application/vnd.openxmlformats-officedocument.drawing+xml'
const DRAWING_NS =
  'xmlns:xdr="http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing" ' +
  'xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"'

/** A PNG of one colour, made here: no outside artwork goes into the fixture */
function solidPng(size: number, [red, green, blue]: [number, number, number]): Uint8Array {
  const chunk = (type: string, data: Buffer): Buffer => {
    const body = Buffer.concat([Buffer.from(type, 'latin1'), data])
    const length = Buffer.alloc(4)
    length.writeUInt32BE(data.length)
    const sum = Buffer.alloc(4)
    sum.writeUInt32BE(crc32(body))
    return Buffer.concat([length, body, sum])
  }
  const header = Buffer.alloc(13)
  header.writeUInt32BE(size, 0)
  header.writeUInt32BE(size, 4)
  // 8 bits a channel, RGB, no interlace
  header.set([8, 2, 0, 0, 0], 8)
  const row = Buffer.concat([
    Buffer.from([0]),
    Buffer.alloc(size * 3, Buffer.from([red, green, blue])),
  ])
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk('IHDR', header),
    chunk('IDAT', deflateSync(Buffer.concat(Array.from({ length: size }, () => row)))),
    chunk('IEND', Buffer.alloc(0)),
  ])
}

/**
 * Cells A1:D10 filled red, and a blue picture floating over F2:J12. Neither colour is in the
 * app's own chrome, so each is on the screen only when its part of the workbook is drawn.
 * `media: false` leaves the picture's bytes out of the package, as a damaged file does.
 */
function pictureXlsx({ media }: { media: boolean }): string {
  const columns = ['A', 'B', 'C', 'D']
  const rows = Array.from({ length: 10 }, (_, row) => {
    const cells = columns.map((column) => `<c r="${column}${row + 1}" s="1"><v>${row + 1}</v></c>`)
    return `<row r="${row + 1}">${cells.join('')}</row>`
  })
  const corner = (name: string, column: number, row: number): string =>
    `<xdr:${name}><xdr:col>${column}</xdr:col><xdr:colOff>0</xdr:colOff>` +
    `<xdr:row>${row}</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:${name}>`
  const xml = (body: string): Uint8Array => strToU8(`<?xml version="1.0" encoding="UTF-8"?>${body}`)
  const zip = zipSync({
    '[Content_Types].xml': xml(
      '<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">' +
        '<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>' +
        '<Default Extension="xml" ContentType="application/xml"/>' +
        '<Default Extension="png" ContentType="image/png"/>' +
        `<Override PartName="/xl/workbook.xml" ContentType="${SHEET_TYPES}.sheet.main+xml"/>` +
        `<Override PartName="/xl/worksheets/sheet1.xml" ContentType="${SHEET_TYPES}.worksheet+xml"/>` +
        `<Override PartName="/xl/styles.xml" ContentType="${SHEET_TYPES}.styles+xml"/>` +
        `<Override PartName="/xl/drawings/drawing1.xml" ContentType="${DRAWING_TYPE}"/></Types>`,
    ),
    '_rels/.rels': xml(
      `<Relationships xmlns="${PACKAGE_RELS}"><Relationship Id="rId1" Type="${OFFICE_RELS}/officeDocument" Target="xl/workbook.xml"/></Relationships>`,
    ),
    'xl/workbook.xml': xml(
      `<workbook ${SHEET_NS} xmlns:r="${OFFICE_RELS}"><sheets><sheet name="Picture" sheetId="1" r:id="rId1"/></sheets></workbook>`,
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
      `<worksheet ${SHEET_NS} xmlns:r="${OFFICE_RELS}"><dimension ref="A1:D10"/>` +
        `<sheetData>${rows.join('')}</sheetData><drawing r:id="rId1"/></worksheet>`,
    ),
    'xl/worksheets/_rels/sheet1.xml.rels': xml(
      `<Relationships xmlns="${PACKAGE_RELS}"><Relationship Id="rId1" Type="${OFFICE_RELS}/drawing" Target="../drawings/drawing1.xml"/></Relationships>`,
    ),
    'xl/drawings/drawing1.xml': xml(
      `<xdr:wsDr ${DRAWING_NS} xmlns:r="${OFFICE_RELS}"><xdr:twoCellAnchor editAs="oneCell">` +
        `${corner('from', 5, 1)}${corner('to', 10, 12)}` +
        '<xdr:pic><xdr:nvPicPr><xdr:cNvPr id="2" name="Picture 1"/><xdr:cNvPicPr/></xdr:nvPicPr>' +
        '<xdr:blipFill><a:blip r:embed="rId1"/><a:stretch><a:fillRect/></a:stretch></xdr:blipFill>' +
        '<xdr:spPr><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></xdr:spPr></xdr:pic>' +
        '<xdr:clientData/></xdr:twoCellAnchor></xdr:wsDr>',
    ),
    'xl/drawings/_rels/drawing1.xml.rels': xml(
      `<Relationships xmlns="${PACKAGE_RELS}"><Relationship Id="rId1" Type="${OFFICE_RELS}/image" Target="../media/image1.png"/></Relationships>`,
    ),
    ...(media ? { 'xl/media/image1.png': solidPng(64, [0, 0, 255]) } : {}),
  })
  const file = join(mkdtempSync(join(tmpdir(), 'seuoffice-picture-')), 'floating-picture.xlsx')
  writeFileSync(file, zip)
  return file
}

interface Watched {
  /** uncaught errors of the launcher and every frame */
  errors: string[]
  warnings: string[]
}

function watch(page: Page): Watched {
  const watched: Watched = { errors: [], warnings: [] }
  page.on('pageerror', (error) => watched.errors.push(error.message))
  page.on('console', (message) => {
    if (message.type() === 'warning') watched.warnings.push(message.text())
  })
  return watched
}

async function open(page: Page, file: string, mode: 'view' | 'edit'): Promise<void> {
  await page.goto('/')
  await page.locator('#edit').setChecked(mode === 'edit')
  await page.setInputFiles('#file', file)
  await expect(page.locator('#status')).not.toContainText('여는 중', { timeout: 30_000 })
  await expect(page.locator('#status')).not.toContainText('오류')
}

/** Shares of the embed's pixels that are plainly red (the cells) and plainly blue (the picture) */
async function shownColours(page: Page): Promise<{ red: number; blue: number }> {
  const png = await page.locator('#viewer iframe').screenshot()
  return page.evaluate(async (base64) => {
    const blob = await (await fetch(`data:image/png;base64,${base64}`)).blob()
    const bitmap = await createImageBitmap(blob)
    const canvas = document.createElement('canvas')
    canvas.width = bitmap.width
    canvas.height = bitmap.height
    const context = canvas.getContext('2d')!
    context.drawImage(bitmap, 0, 0)
    const data = context.getImageData(0, 0, bitmap.width, bitmap.height).data
    let red = 0
    let blue = 0
    for (let i = 0; i < data.length; i += 4) {
      if (data[i]! > 200 && data[i + 1]! < 90 && data[i + 2]! < 90) red++
      if (data[i]! < 90 && data[i + 1]! < 90 && data[i + 2]! > 200) blue++
    }
    return { red: red / (data.length / 4), blue: blue / (data.length / 4) }
  }, png.toString('base64'))
}

/** The app's own "every cell is loaded" line, then the red block of the cells on the screen */
async function expectCells(page: Page): Promise<void> {
  const frame = page.frameLocator('#viewer iframe')
  await expect(frame.locator('footer.status-bar')).toContainText('모두 로드', { timeout: 30_000 })
  await expect
    .poll(async () => (await shownColours(page)).red, { timeout: 20_000 })
    .toBeGreaterThan(0.02)
}

for (const mode of ['view', 'edit'] as const) {
  test(`a workbook with a floating picture shows its cells and the picture for ${mode}`, async ({
    page,
  }) => {
    const watched = watch(page)
    await open(page, pictureXlsx({ media: true }), mode)
    await expectCells(page)
    const picture = page.frameLocator('#viewer iframe').locator('img.xlsx-image')
    await expect(picture).toBeVisible({ timeout: 20_000 })
    await expect
      .poll(async () => (await shownColours(page)).blue, { timeout: 20_000 })
      .toBeGreaterThan(0.02)
    expect(watched.errors).toEqual([])
  })

  test(`a picture whose bytes are missing is left out and the cells still show for ${mode}`, async ({
    page,
  }) => {
    const watched = watch(page)
    await open(page, pictureXlsx({ media: false }), mode)
    await expectCells(page)
    // the app says which picture it left out, in the console
    await expect
      .poll(() => watched.warnings.some((text) => text.includes('workbook media unavailable')))
      .toBe(true)
    await expect(page.frameLocator('#viewer iframe').locator('img.xlsx-image')).toHaveCount(0)
    // the picture takes several percent of the embed when it shows
    expect((await shownColours(page)).blue).toBeLessThan(0.002)
    expect(watched.errors).toEqual([])
  })
}
