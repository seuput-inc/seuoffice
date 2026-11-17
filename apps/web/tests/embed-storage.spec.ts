import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { expect, test, type Frame, type FrameLocator, type Page } from '@playwright/test'
import { PDFDocument, StandardFonts } from 'pdf-lib'

/**
 * What an embedded page leaves in the SeuOffice origin: nothing of the document, however long
 * a person works in it, and no autosave unless the host asked for it. The launcher is the host
 * and shares the origin of the pages, so its `localStorage` is the one the pages would write to.
 *
 * Each format opens a document whose text and file name are marked, does what the desktop app
 * would remember in localStorage (find and replace, a list format, a custom show, a view
 * position), closes the page and reads the storage back.
 */

const repo = fileURLToPath(new URL('../../..', import.meta.url))
const DOC_MARKER = 'seumarker4c1f9d'
const LEGACY_MARKER = 'seulegacy77b2e0'
/** every autosave timer in the apps is 30 s; blur triggers one at once */
const AUTOSAVE_WAIT_MS = 35_000

type App = 'docs' | 'sheets' | 'slides' | 'pdf' | 'hwp'

interface Format {
  app: App
  /** not a name any fixture or localStorage key of the apps could contain */
  fileName: string
  /** the app's autosave toggle key in the desktop build */
  autoSaveKey: string
  /** with the launcher loaded: open the document for editing and wait until it is on screen */
  open(page: Page): Promise<void>
  /** what the desktop app would write to localStorage from this document */
  explore(page: Page): Promise<void>
  /** an edit the page counts as unsaved changes; `n` differs on every call */
  edit(page: Page, n: number): Promise<void>
  /** the person's own save: the app's button or shortcut */
  userSave(page: Page): Promise<void>
}

const appFrame = (page: Page, app: App): Frame =>
  page.frames().find((candidate) => candidate.url().includes(`/${app}/index.html`))!

const appLocator = (page: Page): FrameLocator => page.frameLocator('#viewer iframe')

/** The origin's real localStorage, as the launcher (which installs no embed store) sees it */
const storageOf = (page: Page): Promise<Record<string, string>> =>
  page.evaluate(() =>
    Object.fromEntries(Object.keys(localStorage).map((key) => [key, localStorage.getItem(key)!])),
  )

/** What an older build left: the document's text, names and paths, under the keys it wrote them */
const LEGACY_KEYS: Record<string, string> = {
  'genoffice.sheets.findHistory': JSON.stringify([LEGACY_MARKER]),
  'genoffice.sheets.replaceHistory': JSON.stringify([LEGACY_MARKER]),
  'aisheets.spell-dictionary': JSON.stringify([LEGACY_MARKER]),
  'ai-excel-chat-history': JSON.stringify([{ role: 'user', content: LEGACY_MARKER }]),
  'aidocs.list.recent.bullets': JSON.stringify([[{ numFmt: 'bullet', lvlText: LEGACY_MARKER }]]),
  'aidocs.list.recent.numbers': JSON.stringify([[{ numFmt: 'decimal', lvlText: LEGACY_MARKER }]]),
  'aidocs.list.recent.multi': JSON.stringify([[{ numFmt: 'decimal', lvlText: LEGACY_MARKER }]]),
  [`ai-slides-custom-shows:/home/me/${LEGACY_MARKER}.pptx`]: JSON.stringify([
    { id: 'cs-1', name: LEGACY_MARKER, slideIndices: [0] },
  ]),
  [`ai-slides-guides:/home/me/${LEGACY_MARKER}.pptx`]: JSON.stringify([
    { axis: 'v', position: 0.5 },
  ]),
  'genoffice-pdf-view-state': JSON.stringify({
    [`/home/me/${LEGACY_MARKER}.pdf`]: { page: 3, frac: 0.2, at: 1 },
  }),
  [`genoffice-pdf-job-pages:${LEGACY_MARKER}:ocr`]: '[1,2]',
  'rhwp-document-font-preferences': JSON.stringify({ [LEGACY_MARKER]: { substitutions: {} } }),
}
/** a layout preference the pages keep: an embed must not wipe what it has no reason to touch */
const KEPT_KEY = 'sheets-ai-panel-width'

/** The page's window loses focus: the apps' autosave runs at once when it is on and there is something to save */
const blur = (page: Page, app: App): Promise<void> =>
  appFrame(page, app).evaluate(() => void window.dispatchEvent(new Event('blur')))

const savesOf = (page: Page) => page.evaluate(() => window.seuLauncher!.saves)

// ── pdf ──────────────────────────────────────────────────────────────────────────────────────

async function pdfFixture(): Promise<Buffer> {
  const pdf = await PDFDocument.create()
  const font = await pdf.embedFont(StandardFonts.Helvetica)
  for (let n = 1; n <= 4; n++) {
    const sheet = pdf.addPage([400, 600])
    sheet.drawText(n === 1 ? `Board notes ${DOC_MARKER}` : `Page ${n} body text`, {
      x: 40,
      y: 540,
      size: 16,
      font,
    })
  }
  return Buffer.from(await pdf.save())
}

const pdf: Format = {
  app: 'pdf',
  fileName: 'quarterly-board-minutes.pdf',
  autoSaveKey: 'pdfapp.autoSave',
  async open(page) {
    await page.locator('#edit').setChecked(true)
    await page.setInputFiles('#file', {
      name: this.fileName,
      mimeType: 'application/pdf',
      buffer: await pdfFixture(),
    })
    await expect(page.locator('#status')).not.toContainText('여는 중', { timeout: 30_000 })
    await expect(appLocator(page).getByText(DOC_MARKER).first()).toBeVisible({ timeout: 20_000 })
  },
  async explore(page) {
    const app = appLocator(page)
    // reading position: the desktop remembers it by the file's path
    await app.locator('.pdf-page-content').nth(3).scrollIntoViewIfNeeded()
    await page.waitForTimeout(800)
    await app.locator('.pdf-page-content').first().scrollIntoViewIfNeeded()
    await page.waitForTimeout(800)
    // search
    await app.locator('.pdf-page-content').first().click()
    await page.keyboard.press('ControlOrMeta+f')
    await app.locator('.pdf-search-input').fill(DOC_MARKER)
    await app.locator('.pdf-search-input').press('Enter')
    await page.waitForTimeout(500)
    await app.locator('.pdf-search-input').press('Escape')
  },
  async edit(page) {
    const app = appLocator(page)
    // a rotation made while the page reloads after a save is dropped: press again until it counts
    await expect(async () => {
      const rotate = app.getByRole('button', { name: '오른쪽으로 회전' })
      // a second press of the open tab would collapse the ribbon
      if (!(await rotate.isVisible()))
        await app.locator('.ribbon-tab', { hasText: '페이지' }).click()
      await rotate.click()
      await expect(page.locator('#status')).toContainText('저장 안 함', { timeout: 3000 })
    }).toPass({ timeout: 20_000 })
  },
  async userSave(page) {
    await appLocator(page).locator('.pdf-page-content').first().click()
    await page.keyboard.press('ControlOrMeta+s')
  },
}

// ── hwp ──────────────────────────────────────────────────────────────────────────────────────

const hwp: Format = {
  app: 'hwp',
  fileName: 'quarterly-board-minutes.hwp',
  autoSaveKey: 'hwpapp.autoSave',
  async open(page) {
    await page.locator('#edit').setChecked(true)
    await page.setInputFiles('#file', {
      name: this.fileName,
      mimeType: 'application/octet-stream',
      buffer: readFileSync(resolve(repo, 'apps/hwp/tests/fixtures/memo.hwp')),
    })
    const app = appLocator(page)
    await expect(app.locator('.hwp-app[data-phase="ready"]')).toHaveCount(1, { timeout: 60_000 })
    await expect(
      app.frameLocator('.hwp-editor-host iframe').locator('[data-cmd="ext:ieum-save"]'),
    ).toHaveCount(1, { timeout: 30_000 })
  },
  async explore(page) {
    await this.edit(page, 0)
  },
  async edit(page, n) {
    const editor = appLocator(page).locator('.hwp-editor-host iframe')
    const box = (await editor.boundingBox())!
    await page.mouse.click(box.x + box.width * 0.4, box.y + 300)
    await page.keyboard.press('End')
    await page.keyboard.type(`${DOC_MARKER}${n}`)
  },
  async userSave(page) {
    await page.keyboard.press('ControlOrMeta+s')
  },
}

// ── sheets ───────────────────────────────────────────────────────────────────────────────────

const nameBox = (app: FrameLocator) => app.locator('input.univer-appearance-none').first()

const sheets: Format = {
  app: 'sheets',
  fileName: 'quarterly-board-minutes.xlsx',
  autoSaveKey: 'ai-sheets-auto-save',
  async open(page) {
    await page.locator('#edit').setChecked(true)
    await page.setInputFiles('#file', {
      name: this.fileName,
      mimeType: 'application/octet-stream',
      buffer: readFileSync(resolve(repo, 'apps/sheets/fixtures/generated/compatibility-edit.xlsx')),
    })
    await expect(page.locator('#status')).not.toContainText('여는 중', { timeout: 30_000 })
    const app = appLocator(page)
    await expect(nameBox(app)).toBeVisible({ timeout: 20_000 })
    await expect(app.locator('footer.status-bar')).toContainText('모두 로드', { timeout: 20_000 })
  },
  async explore(page) {
    const app = appLocator(page)
    await nameBox(app).fill('A1')
    await nameBox(app).press('Enter')
    await nameBox(app).blur()
    // the Home tab's Find button opens the app's find and replace panel
    await app.locator('.inline-tools button', { hasText: '찾기' }).first().click()
    const panel = app.locator('.find-replace-panel')
    await expect(panel).toBeVisible({ timeout: 10_000 })
    const query = panel.locator('input').first()
    await query.fill(DOC_MARKER)
    await query.press('Enter')
    await panel.getByRole('tab', { name: '바꾸기' }).click()
    const inputs = panel.locator('.fr-grid input:not([type="checkbox"])')
    await inputs.nth(1).fill(`${DOC_MARKER}replacement`)
    await inputs.nth(1).press('Enter')
    await inputs.first().press('Escape')
  },
  async edit(page, n) {
    const app = appLocator(page)
    await expect(async () => {
      await nameBox(app).fill(`D${n + 2}`)
      await nameBox(app).press('Enter')
      await nameBox(app).blur()
      await page.keyboard.type(`${DOC_MARKER}${n}`)
      await page.keyboard.press('Enter')
      await expect(page.locator('#status')).toContainText('저장 안 함', { timeout: 2000 })
    }).toPass({ timeout: 15_000 })
  },
  async userSave(page) {
    await appLocator(page)
      .getByRole('button', { name: /^저장 ?\((⌘|Ctrl\+)S\)( \(Alt\+\d\))?$/ })
      .click()
  },
}

// ── docs ─────────────────────────────────────────────────────────────────────────────────────

const docs: Format = {
  app: 'docs',
  fileName: 'quarterly-board-minutes.docx',
  autoSaveKey: 'aidocs.autoSave',
  async open(page) {
    await page.locator('#edit').setChecked(true)
    await page.setInputFiles('#file', {
      name: this.fileName,
      mimeType: 'application/octet-stream',
      buffer: readFileSync(resolve(repo, 'fixtures/generated/simple.docx')),
    })
    await expect(page.locator('#status')).not.toContainText('여는 중', { timeout: 30_000 })
    await expect(appLocator(page).locator('.ProseMirror')).toBeVisible()
  },
  async explore(page) {
    await this.edit(page, 0)
    // the list formats the desktop app remembers: a bullet and a number format of the person's own text
    await appFrame(page, 'docs').evaluate((marker) => {
      localStorage.setItem(
        'aidocs.list.recent.bullets',
        JSON.stringify([[{ numFmt: 'bullet', lvlText: marker }]]),
      )
      localStorage.setItem(
        'aidocs.list.recent.numbers',
        JSON.stringify([[{ numFmt: 'decimal', lvlText: `${marker}.%1` }]]),
      )
    }, DOC_MARKER)
  },
  async edit(page, n) {
    const body = appLocator(page).locator('.ProseMirror')
    await body.locator('p').first().click()
    await page.keyboard.press(process.platform === 'darwin' ? 'Meta+ArrowRight' : 'End')
    await page.keyboard.type(` ${DOC_MARKER}${n}`)
  },
  async userSave(page) {
    await appLocator(page)
      .getByRole('button', { name: /^저장 \((⌘|Ctrl\+)S\)$/ })
      .click()
  },
}

// ── slides ───────────────────────────────────────────────────────────────────────────────────

/** the title text the slide shows now; `edit` replaces it */
let slideTitle = 'Q3 Business Review'

const slides: Format = {
  app: 'slides',
  fileName: 'quarterly-board-minutes.pptx',
  autoSaveKey: 'ai-slides-auto-save',
  async open(page) {
    slideTitle = 'Q3 Business Review'
    await page.setChecked('#edit', true)
    await page.setInputFiles('#file', {
      name: this.fileName,
      mimeType: 'application/octet-stream',
      buffer: readFileSync(
        resolve(repo, 'packages/pptx-engine/tests/fixtures/01_standard_business.pptx'),
      ),
    })
    await expect(page.locator('#status')).toHaveText(this.fileName, { timeout: 30_000 })
    await expect(appLocator(page).locator('.thumb')).toHaveCount(5)
  },
  async explore(page) {
    await this.edit(page, 0)
    // a custom show and a guide: the desktop app keeps both under the document's path
    await appFrame(page, 'slides').evaluate(
      ({ marker, fileName }) => {
        localStorage.setItem(
          `ai-slides-custom-shows:/web/x/${fileName}`,
          JSON.stringify([{ id: 'cs-1', name: marker, slideIndices: [0, 1] }]),
        )
        localStorage.setItem(
          `ai-slides-guides:/web/x/${fileName}`,
          JSON.stringify([{ axis: 'v', position: 0.5 }]),
        )
      },
      { marker: DOC_MARKER, fileName: this.fileName },
    )
  },
  async edit(page, n) {
    const frame = appFrame(page, 'slides')
    const at = await frame.evaluate(async (wanted) => {
      const stage = document.querySelector('.stage-rel')!.getBoundingClientRect()
      const api = (
        window as unknown as {
          slidesApi: {
            getRenderSlides(): Promise<
              Array<{
                widthPx: number
                nodes: Array<{
                  box: { centerX: number; centerY: number }
                  text?: { lines: Array<{ runs: Array<{ text: string }> }> }
                }>
              }>
            >
          }
        }
      ).slidesApi
      const slide = (await api.getRenderSlides())[0]!
      const node = slide.nodes.find((candidate) =>
        candidate.text?.lines.some((line) =>
          line.runs
            .map((run) => run.text)
            .join('')
            .includes(wanted),
        ),
      )!
      const k = stage.width / slide.widthPx
      return { x: stage.x + node.box.centerX * k, y: stage.y + node.box.centerY * k }
    }, slideTitle)
    const viewer = (await page.locator('#viewer iframe').boundingBox())!
    await page.mouse.dblclick(viewer.x + at.x, viewer.y + at.y)
    await expect(appLocator(page).locator('.slide-text-editor')).toBeFocused()
    await page.keyboard.press('ControlOrMeta+A')
    slideTitle = `${DOC_MARKER}${n}`
    await page.keyboard.type(slideTitle)
    await appLocator(page).locator('.notes-pane textarea').click()
  },
  async userSave(page) {
    await page.keyboard.press('ControlOrMeta+S')
  },
}

const formats = [pdf, hwp, sheets, docs, slides]

for (const format of formats) {
  test.describe(format.app, () => {
    test('leaves nothing of the document in the origin’s localStorage', async ({ page }) => {
      await page.goto('/')
      await page.evaluate(
        ([legacy, kept]) => {
          for (const [key, value] of Object.entries(legacy)) localStorage.setItem(key, value)
          localStorage.setItem(kept, '333')
        },
        [LEGACY_KEYS, KEPT_KEY] as const,
      )
      await format.open(page)

      // an older build's leftovers are gone once a page started, and nothing else was touched
      let stored = await storageOf(page)
      expect(JSON.stringify(stored)).not.toContain(LEGACY_MARKER)
      expect(stored[KEPT_KEY]).toBe('333')

      await format.explore(page)
      stored = await storageOf(page)
      const open = JSON.stringify(stored)
      expect(open).not.toContain(DOC_MARKER)
      expect(open).not.toContain(format.fileName)
      expect(open).not.toContain('quarterly-board')

      // closed: the page is gone, and the origin holds nothing of it
      await page.goto('/')
      const closed = JSON.stringify(await storageOf(page))
      expect(closed).not.toContain(DOC_MARKER)
      expect(closed).not.toContain(LEGACY_MARKER)
      expect(closed).not.toContain('quarterly-board')
    })

    test('has no autosave unless the host asks for it, whatever an earlier page stored', async ({
      page,
    }) => {
      test.setTimeout(AUTOSAVE_WAIT_MS + 90_000)
      await page.goto('/')
      // the choice an older page could have left under the toggle's key
      await page.evaluate(
        (key) => localStorage.setItem(key, '{"on":true,"base":0}'),
        format.autoSaveKey,
      )
      await format.open(page)
      await expect(appLocator(page).locator('.autosave-toggle')).toHaveCount(0)

      await format.edit(page, 1)
      await expect(page.locator('#status')).toContainText('저장 안 함')
      await blur(page, format.app)
      await page.waitForTimeout(AUTOSAVE_WAIT_MS)
      expect(await savesOf(page)).toEqual([])
      await expect(page.locator('#status')).toContainText('저장 안 함')
    })

    test('with autoSave the page saves by itself, says so, and keeps its switch to itself', async ({
      page,
    }) => {
      test.setTimeout(AUTOSAVE_WAIT_MS + 60_000)
      await page.goto('/?autoSave=1')
      await format.open(page)
      const toggle = appLocator(page).locator('.autosave-toggle')
      await expect(toggle).toHaveCount(1)
      await expect(toggle.locator('input')).toBeChecked()

      await format.edit(page, 1)
      await expect(page.locator('#status')).toContainText('저장 안 함')
      await blur(page, format.app)
      await expect.poll(() => savesOf(page), { timeout: AUTOSAVE_WAIT_MS }).toHaveLength(1)
      expect(await savesOf(page)).toEqual([{ fileName: format.fileName, reason: 'auto' }])
      await expect(page.locator('#status')).not.toContainText('저장 안 함')

      // the person's switch lasts as long as the page: nothing is stored for the next one
      await toggle.click()
      await expect(toggle.locator('input')).not.toBeChecked()
      expect(JSON.stringify(await storageOf(page))).not.toContain(format.autoSaveKey)
    })

    test('names why each save happened: the person, the host, the autosave', async ({ page }) => {
      test.setTimeout(AUTOSAVE_WAIT_MS + 90_000)
      await page.goto('/?autoSave=1')
      await format.open(page)
      // switch the autosave off first, so the two saves below are the only ones
      await appLocator(page).locator('.autosave-toggle').click()

      await format.edit(page, 1)
      await expect(page.locator('#status')).toContainText('저장 안 함')
      await format.userSave(page)
      await expect.poll(() => savesOf(page)).toHaveLength(1)

      await format.edit(page, 2)
      await expect(page.locator('#status')).toContainText('저장 안 함')
      await page.locator('#save').click()
      await expect.poll(() => savesOf(page)).toHaveLength(2)

      expect((await savesOf(page)).map((save) => save.reason)).toEqual(['user', 'host'])
    })
  })
}
