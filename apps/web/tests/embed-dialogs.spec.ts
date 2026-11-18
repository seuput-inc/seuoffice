import { mkdtempSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { expect, test, type FrameLocator, type Page } from '@playwright/test'
import { strToU8, zipSync } from 'fflate'
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
 * An embed in edit mode under the default sandbox, which has no allow-modals: the browser's
 * confirm answers false at once, alert and prompt show nothing and print is ignored. Every
 * question an app asks there is drawn in the page instead, and the answer counts.
 */

const W = 'xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"'
const R = 'xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"'
const SECT_PR =
  '<w:pgSz w:w="12240" w:h="15840"/><w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="720" w:footer="720" w:gutter="0"/>'

/** Two sections, the first with a header the second inherits, and no heading anywhere */
function twoSectionDocx(): string {
  const para = (text: string) => `<w:p><w:r><w:t xml:space="preserve">${text}</w:t></w:r></w:p>`
  const filler = (label: string) =>
    Array.from({ length: 70 }, (_, i) => para(`${label} paragraph ${i + 1}`)).join('')
  const zip = zipSync({
    '[Content_Types].xml': strToU8(
      '<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/><Override PartName="/word/header1.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.header+xml"/></Types>',
    ),
    '_rels/.rels': strToU8(
      '<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>',
    ),
    'word/_rels/document.xml.rels': strToU8(
      '<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId5" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/header" Target="header1.xml"/></Relationships>',
    ),
    'word/header1.xml': strToU8(
      `<?xml version="1.0" encoding="UTF-8"?><w:hdr ${W}><w:p><w:r><w:t>Plain header</w:t></w:r></w:p></w:hdr>`,
    ),
    'word/document.xml': strToU8(
      `<?xml version="1.0" encoding="UTF-8"?><w:document ${W} ${R}><w:body>${filler('First')}` +
        `<w:p><w:pPr><w:sectPr><w:headerReference w:type="default" r:id="rId5"/>${SECT_PR}</w:sectPr></w:pPr></w:p>` +
        `${filler('Second')}<w:sectPr>${SECT_PR}</w:sectPr></w:body></w:document>`,
    ),
  })
  const file = join(mkdtempSync(join(tmpdir(), 'seuoffice-dialogs-')), 'two-sections.docx')
  writeFileSync(file, zip)
  return file
}

test.beforeAll(buildEmbedScript)

let host: Site
let bundle: Site
/** every native dialog the browser was asked to show: none may be */
let nativeDialogs: string[]
test.beforeEach(async ({ page }) => {
  ;({ host, bundle } = await serveHostAndBundle())
  nativeDialogs = []
  page.on('dialog', (dialog) => {
    nativeDialogs.push(dialog.type())
    void dialog.dismiss()
  })
})
test.afterEach(() => Promise.all([host.close(), bundle.close()]))

async function open(page: Page, file: string): Promise<FrameLocator> {
  await embed(page, origin(host, HOST_NAME), `${origin(bundle, BUNDLE_HOST)}/`, {
    file,
    mode: 'edit',
  })
  await expectOpened(page)
  return page.frameLocator('#viewer iframe')
}

/** The in-page dialog of the app's page (not of a frame inside it) */
const pageDialog = (app: FrameLocator) => app.locator('[data-web-sandbox-dialog]')

test('docx: the header’s link to the previous section asks in the page, and both answers count', async ({
  page,
}) => {
  test.setTimeout(120_000)
  const app = await open(page, twoSectionDocx())
  const gapHeader = app.locator('.page-gap-hf.page-hf-header[data-hf-section="1"]').first()
  await gapHeader.scrollIntoViewIfNeeded({ timeout: 30_000 })
  await expect(gapHeader).toContainText('Plain header')
  await gapHeader.dblclick()
  await expect(gapHeader.locator('.page-hf-editor')).toBeVisible()
  const link = app.getByRole('button', { name: '이전 구역에 연결' })
  await expect(link).toHaveClass(/active/)
  // unlinking asks nothing
  await link.click()
  await expect(link).not.toHaveClass(/active/)

  // linking again deletes this header: the page asks first, and Cancel keeps it
  await link.click()
  const dialog = pageDialog(app)
  await expect(dialog).toContainText('이전 구역의 머리글에 연결할까요?')
  await dialog.getByRole('button', { name: '취소' }).click()
  await expect(dialog).toHaveCount(0)
  await expect(link).not.toHaveClass(/active/)
  await expect(gapHeader.locator('.page-hf-editor')).toBeVisible()

  await link.click()
  await dialog.getByRole('button', { name: '확인' }).click()
  await expect(dialog).toHaveCount(0)
  // linked: the section's own header is gone, the editor with it
  await expect(gapHeader.locator('.page-hf-editor')).toHaveCount(0)
  await expect(gapHeader).toContainText('Plain header')
  expect(nativeDialogs).toEqual([])
})

test('docx: a references notice shows in the page', async ({ page }) => {
  test.setTimeout(120_000)
  const app = await open(page, twoSectionDocx())
  await app.locator('.ribbon-tab', { hasText: '참조' }).click()
  await app.getByRole('button', { name: '목차 업데이트' }).click()
  const dialog = pageDialog(app)
  await expect(dialog).toContainText('목차를 만들 수 없습니다')
  await dialog.getByRole('button', { name: '확인' }).click()
  await expect(dialog).toHaveCount(0)
  expect(nativeDialogs).toEqual([])
})

test('pdf: printing where the sandbox ignores it says so in the page', async ({ page }) => {
  test.setTimeout(120_000)
  const app = await open(page, join(repo, 'apps/pdf/tests/fixtures/plain-sample.pdf'))
  await app.locator('.pdf-page-content').first().click({ timeout: 30_000 })
  await page.keyboard.press('ControlOrMeta+p')
  await app.locator('.pdf-modal-btn.primary', { hasText: '인쇄' }).click()
  await expect(app.getByText('이 화면에서는 인쇄 창을 열 수 없습니다').first()).toBeVisible({
    timeout: 15_000,
  })
  expect(nativeDialogs).toEqual([])
})

test('hwp: the studio’s notices show in the page', async ({ page }) => {
  test.setTimeout(150_000)
  const app = await open(page, join(repo, 'apps/hwp/tests/fixtures/memo.hwp'))
  await expect(app.locator('.hwp-app[data-phase="ready"]')).toHaveCount(1, { timeout: 60_000 })
  const studio = app.frameLocator('.hwp-editor-host iframe')
  await expect(studio.locator('[data-cmd="ext:ieum-save"]')).toHaveCount(1, { timeout: 30_000 })
  // the style dialog (F6), from the document's body: the Normal style cannot be deleted
  const editor = (await app.locator('.hwp-editor-host iframe').boundingBox())!
  await page.mouse.click(editor.x + editor.width * 0.4, editor.y + 300)
  await page.keyboard.press('F6')
  const first = studio.locator('.sd-style-item').first()
  await expect(first).toBeVisible({ timeout: 15_000 })
  await first.click()
  await studio.locator('.sd-icon-btn[title="스타일 삭제"]').click()
  const dialog = pageDialog(app)
  await expect(dialog).toContainText('스타일은 삭제할 수 없습니다')
  await dialog.getByRole('button', { name: '확인' }).click()
  await expect(dialog).toHaveCount(0)
  expect(nativeDialogs).toEqual([])
})
