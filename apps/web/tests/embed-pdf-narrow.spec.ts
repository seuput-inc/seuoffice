import { join } from 'node:path'
import { expect, test, type Frame, type Page } from '@playwright/test'
import {
  BUNDLE_HOST,
  buildEmbedScript,
  embed,
  expectOpened,
  HOST_NAME,
  origin,
  repo,
  serveHostAndBundle,
  type Site,
} from './cross-origin-embed'

/**
 * The page thumbnails beside a PDF. At a phone width (390 px, as YUYU shows it on a phone) they
 * would take four tenths of the page, so the document opens without them and has the width to
 * itself; the button on the edge brings them back. At a desktop width (1440 px) they are there
 * when the document opens, also right after a narrow page was open in the same browser, and
 * also when the host opened the document in a viewer it had not shown yet.
 */

const PDF = join(repo, 'apps/pdf/tests/fixtures/plain-sample.pdf')

test.beforeAll(buildEmbedScript)

let host: Site
let bundle: Site
test.beforeEach(async () => ({ host, bundle } = await serveHostAndBundle()))
test.afterEach(() => Promise.all([host.close(), bundle.close()]))

/** Embed the PDF at `width` and wait until its first page is drawn */
async function openAt(page: Page, width: number, mode: 'view' | 'edit'): Promise<Frame> {
  await page.setViewportSize({ width, height: 844 })
  await embed(page, origin(host, HOST_NAME), `${origin(bundle, BUNDLE_HOST)}/`, { file: PDF, mode })
  await expectOpened(page)
  const frame = page.frames().find((candidate) => candidate.url().includes('/pdf/'))!
  await expect(frame.locator('.pdf-scroll canvas').first()).toBeVisible({ timeout: 60_000 })
  return frame
}

for (const mode of ['view', 'edit'] as const) {
  test(`pdf ${mode}: the thumbnails are closed at 390 until asked for and open at 1440`, async ({
    page,
  }) => {
    test.setTimeout(180_000)
    const narrow = await openAt(page, 390, mode)
    await expect(narrow.locator('.pdf-thumb')).toHaveCount(0)
    const documentShare = await narrow.evaluate(
      () => document.querySelector('.pdf-scroll')!.getBoundingClientRect().width / innerWidth,
    )
    expect(documentShare).toBeGreaterThan(0.95)

    const wide = await openAt(page, 1440, mode)
    await expect(wide.locator('.pdf-thumb').first()).toBeVisible({ timeout: 30_000 })

    const narrowAgain = await openAt(page, 390, mode)
    await expect(narrowAgain.locator('.pdf-thumb')).toHaveCount(0)
    await narrowAgain.locator('.pdf-side-reopen').click()
    await expect(narrowAgain.locator('.pdf-thumb').first()).toBeVisible({ timeout: 30_000 })
  })
}

test('pdf: a document opened in a hidden viewer has its thumbnails once shown at 1440', async ({
  page,
}) => {
  test.setTimeout(180_000)
  await page.setViewportSize({ width: 1440, height: 844 })
  await page.addInitScript(() =>
    addEventListener('DOMContentLoaded', () => {
      document.getElementById('viewer')!.style.display = 'none'
    }),
  )
  await embed(page, origin(host, HOST_NAME), `${origin(bundle, BUNDLE_HOST)}/`, { file: PDF })
  await expectOpened(page)
  await page.evaluate(() => {
    document.getElementById('viewer')!.style.display = ''
  })
  const frame = page.frames().find((candidate) => candidate.url().includes('/pdf/'))!
  await expect(frame.locator('.pdf-thumb').first()).toBeVisible({ timeout: 30_000 })
})
