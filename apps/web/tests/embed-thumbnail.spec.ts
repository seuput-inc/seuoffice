import { readdirSync, readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { expect, test, type Page } from '@playwright/test'

/**
 * A host asks the page for a picture of its first page (`doc.thumbnail()`) as soon as `opened`
 * settled. pdf, pptx and hwp answer a PNG of the asked width that shows the page, opened for
 * viewing or for editing; xlsx and docx refuse with `thumbnail_unsupported`.
 */
const repo = fileURLToPath(new URL('../../..', import.meta.url))
const at = (path: string): string => resolve(repo, path)

function firstXlsx(): string {
  const dir = at('apps/sheets/fixtures/generated')
  const name = readdirSync(dir)
    .filter((file) => file.endsWith('.xlsx'))
    .sort()[0]
  if (!name) throw new Error(`no .xlsx in ${dir}`)
  return resolve(dir, name)
}

const DRAWN = [
  { name: 'pdf', file: () => at('apps/pdf/tests/fixtures/plain-sample.pdf') },
  {
    name: 'pptx',
    file: () => at('packages/pptx-engine/tests/fixtures/01_standard_business.pptx'),
  },
  { name: 'hwp', file: () => at('apps/hwp/tests/fixtures/format-copy-table.hwp') },
  { name: 'hwpx', file: () => at('apps/hwp/tests/fixtures/generated-form-tables.hwpx') },
]

const REFUSED = [
  { name: 'xlsx', file: firstXlsx },
  { name: 'docx', file: () => at('fixtures/generated/simple.docx') },
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
    for (let i = 0; i < data.length; i += 4) {
      const key = (data[i]! << 16) | (data[i + 1]! << 8) | data[i + 2]!
      counts.set(key, (counts.get(key) ?? 0) + 1)
      if (data[i + 3] !== 255) opaque = false
    }
    return {
      width: image.width,
      height: image.height,
      png: Array.from(image.bytes),
      decodedWidth: bitmap.width,
      decodedHeight: bitmap.height,
      inked: 1 - Math.max(...counts.values()) / (data.length / 4),
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

for (const format of DRAWN) {
  for (const mode of ['view', 'edit'] as const) {
    test(`${format.name} gives its first page right after it opened for ${mode}`, async ({
      page,
    }, testInfo) => {
      const problems: string[] = []
      page.on('pageerror', (error) => problems.push(error.message))
      await open(page, format.file(), mode)
      const shot = await thumbnail(page, 320)
      expectPage(shot, 320)
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
  await open(page, DRAWN[0]!.file(), 'view')
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

for (const format of REFUSED) {
  test(`${format.name} refuses with thumbnail_unsupported`, async ({ page }) => {
    await open(page, format.file(), 'view')
    expect(await thumbnail(page, 320)).toEqual({ code: 'thumbnail_unsupported' })
  })
}
