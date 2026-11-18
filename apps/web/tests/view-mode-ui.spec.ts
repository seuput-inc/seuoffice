import { readFileSync, readdirSync } from 'node:fs'
import { resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import {
  expect,
  test,
  type Frame,
  type FrameLocator,
  type Page,
  type TestInfo,
} from '@playwright/test'

/**
 * A document opened for viewing (no `mode` option) shows reading tools only: no editing ribbon, no AI
 * panel or button without `ai`, no sideways scroll on a phone, and no way to change the document by
 * typing, pasting or dropping.
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

interface Format {
  name: string
  file: () => string
  /** the text the document is read from, when the page has it as DOM text */
  editable?: string
  /** controls that belong to editing, by their label */
  editingText: RegExp
  /** the page paints the document on a canvas: typed text would show only in the pixels */
  pixels?: boolean
  /** the page turns on a click or a space: only the document is compared, not the page indicator */
  turns?: boolean
  /** something only a reading page has */
  reading: string
}

const EDITING_TEXT = /자동 저장|붙여넣기|선택하여 붙여넣기|형광펜|텍스트 편집|텍스트 넣기|PDF 변환/

const FORMATS: Format[] = [
  {
    name: 'pdf',
    file: () => at('apps/pdf/tests/fixtures/plain-sample.pdf'),
    editingText: EDITING_TEXT,
    pixels: true,
    reading: '.tb-page-input',
  },
  {
    name: 'docx',
    file: () => at('fixtures/generated/simple.docx'),
    editable: '.ProseMirror',
    editingText: EDITING_TEXT,
    reading: '.ProseMirror',
  },
  {
    name: 'xlsx',
    file: firstXlsx,
    editingText: EDITING_TEXT,
    pixels: true,
    reading: '.status-bar .zoom-slider',
  },
  {
    name: 'pptx',
    file: () => at('packages/pptx-engine/tests/fixtures/01_standard_business.pptx'),
    editingText: EDITING_TEXT,
    turns: true,
    reading: '.reading-view',
  },
  {
    name: 'hwp',
    file: () => at('apps/hwp/tests/fixtures/memo.hwp'),
    editingText: EDITING_TEXT,
    reading: '.hwp-editor-host iframe',
  },
]

const AI_TOKENS = ['ai-dock', 'ai-panel', 'ai-rail', 'ai-entry', 'copilot']
const EDITING_CHROME = [
  '.ribbon-tab',
  '.ribbon-tabs',
  '.excel-header',
  '.hwp-topbar',
  '#menu-bar',
  '#icon-toolbar',
  '#style-bar',
  '.autosave-toggle',
  '.qa-btn',
  '.ribbon:not(:has(.tb-page-input))',
]

/** Every frame of the page below the launcher: the app, and for hwp the editor inside it */
function appFrames(page: Page): Frame[] {
  return page.frames().filter((frame) => frame !== page.mainFrame())
}

async function visibleCount(frame: Frame, selector: string): Promise<number> {
  return frame.evaluate((sel) => {
    return [...document.querySelectorAll(sel)].filter(
      (el) => el.getClientRects().length > 0 && getComputedStyle(el).visibility !== 'hidden',
    ).length
  }, selector)
}

/** Elements a person sees: laid out, not hidden and not fully transparent */
async function shownCount(frame: Frame, selector: string): Promise<number> {
  return frame.evaluate((sel) => {
    return [...document.querySelectorAll(sel)].filter((el) => {
      const style = getComputedStyle(el)
      return (
        el.getClientRects().length > 0 && style.visibility !== 'hidden' && style.opacity !== '0'
      )
    }).length
  }, selector)
}

/** Opens a document for viewing and returns its page once the reading surface is there */
async function openForViewing(
  page: Page,
  file: string,
  width: number,
  reading: string,
): Promise<FrameLocator> {
  await page.setViewportSize({ width, height: 900 })
  await page.goto('/')
  await page.setInputFiles('#file', {
    name: file.split('/').pop()!,
    mimeType: 'application/octet-stream',
    buffer: readFileSync(file),
  })
  await expect(page.locator('#status')).not.toContainText('여는 중', { timeout: 40_000 })
  await expect(page.locator('#status')).not.toContainText('오류')
  const app = page.frameLocator('#viewer iframe')
  await expect(app.locator(reading).first()).toBeAttached({ timeout: 30_000 })
  await page.waitForTimeout(4000)
  return app
}

async function attachScreenshot(page: Page, testInfo: TestInfo, name: string): Promise<void> {
  const shot = testInfo.outputPath(`${name}.png`)
  await page.screenshot({ path: shot })
  await testInfo.attach(name, { path: shot, contentType: 'image/png' })
}

/** Every slide's text as the renderer draws it, so a page turn cannot hide a changed deck */
async function deckText(page: Page): Promise<string> {
  const frame = appFrames(page)[0]!
  return frame.evaluate(async () => {
    const slides = await (
      window as unknown as {
        slidesApi: {
          getRenderSlides(): Promise<
            { nodes: { text?: { lines: { runs: { text: string }[] }[] } }[] }[] | null
          >
        }
      }
    ).slidesApi.getRenderSlides()
    return JSON.stringify(
      (slides ?? []).map((slide) =>
        slide.nodes.map((node) =>
          node.text?.lines.map((line) => line.runs.map((run) => run.text).join('')),
        ),
      ),
    )
  })
}

async function bodyText(page: Page): Promise<string> {
  const parts = await Promise.all(
    appFrames(page).map((frame) => frame.evaluate(() => document.body?.innerText ?? '')),
  )
  return parts.join('\n')
}

{
  /** Pixels that differ by more than anti-aliasing noise between two PNGs of one size */
  async function changedPixels(page: Page, a: Buffer, b: Buffer): Promise<number> {
    return page.evaluate(
      async ([first, second]) => {
        const read = async (base64: string): Promise<Uint8ClampedArray> => {
          const bitmap = await createImageBitmap(
            await (await fetch(`data:image/png;base64,${base64}`)).blob(),
          )
          const canvas = document.createElement('canvas')
          canvas.width = bitmap.width
          canvas.height = bitmap.height
          const context = canvas.getContext('2d')!
          context.drawImage(bitmap, 0, 0)
          return context.getImageData(0, 0, bitmap.width, bitmap.height).data
        }
        const [x, y] = await Promise.all([read(first!), read(second!)])
        if (x.length !== y.length) return Number.MAX_SAFE_INTEGER
        let changed = 0
        for (let i = 0; i < x.length; i += 4) {
          if (
            Math.abs(x[i]! - y[i]!) > 24 ||
            Math.abs(x[i + 1]! - y[i + 1]!) > 24 ||
            Math.abs(x[i + 2]! - y[i + 2]!) > 24
          )
            changed++
        }
        return changed
      },
      [a.toString('base64'), b.toString('base64')],
    )
  }

  for (const width of [1440, 390]) {
    for (const format of FORMATS) {
      test(`${format.name} view mode at ${width}px`, async ({ page, browserName }, testInfo) => {
        const problems: string[] = []
        page.on('pageerror', (error) => problems.push(error.message))
        await page.setViewportSize({ width, height: 900 })
        await page.goto('/')
        await page.setInputFiles('#file', {
          name: format.file().split('/').pop()!,
          mimeType: 'application/octet-stream',
          buffer: (await import('node:fs')).readFileSync(format.file()),
        })
        await expect(page.locator('#status')).not.toContainText('여는 중', { timeout: 40_000 })
        await expect(page.locator('#status')).not.toContainText('오류')
        const app = page.frameLocator('#viewer iframe')
        await expect(app.locator(format.reading).first()).toBeAttached({ timeout: 30_000 })
        await page.waitForTimeout(4000)

        // reading tools only: no editing chrome, no editing labels
        for (const frame of appFrames(page)) {
          for (const selector of EDITING_CHROME) {
            expect(await visibleCount(frame, selector), `${selector} visible`).toBe(0)
          }
        }
        expect(await bodyText(page)).not.toMatch(format.editingText)

        // no AI panel and no AI button: absent, not closed
        for (const frame of appFrames(page)) {
          const present = await frame.evaluate((tokens) => {
            return [...document.querySelectorAll('[class]')]
              .filter((el) => [...el.classList].some((c) => tokens.includes(c)))
              .map((el) => el.className)
          }, AI_TOKENS)
          expect(present, 'AI elements in the DOM').toEqual([])
        }
        expect(await bodyText(page)).not.toMatch(/AI 도우미|AI 연결|AI 요약/)

        // the document uses the whole width
        for (const frame of appFrames(page)) {
          const widths = await frame.evaluate(() => ({
            scroll: document.scrollingElement!.scrollWidth,
            client: document.scrollingElement!.clientWidth,
          }))
          expect(widths.scroll, 'horizontal overflow').toBeLessThanOrEqual(widths.client)
        }

        // typing, pasting and dropping change nothing
        const viewer = page.locator('#viewer iframe')
        const documentText = async (): Promise<string> => {
          const text = await bodyText(page)
          // both page indicators: "2 / 5" in the reading bar and "2/5" in the status line
          return format.turns ? text.replace(/\d+ ?\/ ?\d+/g, '') + (await deckText(page)) : text
        }
        const before = await documentText()
        const box = (await viewer.boundingBox())!
        await page.mouse.click(box.x + box.width / 2, box.y + box.height / 2)
        const pixelsBefore = format.pixels ? await viewer.screenshot() : null
        await page.keyboard.type('ZZTOP')
        await page.keyboard.press('Delete')
        for (const frame of appFrames(page)) {
          await frame.evaluate(() => {
            const target = document.activeElement ?? document.body
            const data = new DataTransfer()
            data.setData('text/plain', 'ZZTOP')
            data.setData('text/html', '<b>ZZTOP</b>')
            target.dispatchEvent(
              new ClipboardEvent('paste', { clipboardData: data, bubbles: true, cancelable: true }),
            )
            const drop = new DataTransfer()
            drop.setData('text/plain', 'ZZTOP')
            drop.items.add(new File(['x'], 'x.png', { type: 'image/png' }))
            for (const type of ['dragenter', 'dragover', 'drop']) {
              target.dispatchEvent(
                new DragEvent(type, { dataTransfer: drop, bubbles: true, cancelable: true }),
              )
            }
          })
        }
        await page.waitForTimeout(800)
        if (pixelsBefore) {
          expect(await changedPixels(page, pixelsBefore, await viewer.screenshot())).toBe(0)
        }
        expect(await documentText()).toBe(before)
        expect(await documentText()).not.toContain('ZZTOP')
        if (format.editable) {
          const editable = await app
            .locator(format.editable)
            .first()
            .getAttribute('contenteditable')
          expect(editable).not.toBe('true')
        }
        expect(problems).toEqual([])

        const shot = testInfo.outputPath(`${format.name}-${width}-${browserName}.png`)
        await page.screenshot({ path: shot })
        await testInfo.attach(`${format.name}-${width}-${browserName}`, {
          path: shot,
          contentType: 'image/png',
        })
      })
    }
  }

  /**
   * What is left to press in a document opened for viewing: each format's ways into an edit (a
   * button, a menu, a handle, a caret, a hint) are gone, and the reading tools beside them work.
   * Every check is soft, so one run names each way that is still there.
   */
  for (const width of [1440, 390]) {
    test(`xlsx view mode has no way into an edit at ${width}px`, async ({
      page,
      browserName,
    }, testInfo) => {
      const problems: string[] = []
      page.on('pageerror', (error) => problems.push(error.message))
      const app = await openForViewing(
        page,
        at('apps/sheets/fixtures/generated/compatibility-sheets.xlsx'),
        width,
        '.status-bar .zoom-slider',
      )
      const frame = appFrames(page)[0]!
      const viewer = page.locator('#viewer iframe')
      const box = (await viewer.boundingBox())!
      const bar = app.locator('[data-u-comp="formula-bar"]')
      const nameBox = bar.locator('[data-u-comp="defined-name"] input')

      expect
        .soft(await app.locator('.status-bar .status-msg').innerText(), 'status line hint')
        .not.toMatch(/편집/)

      // between the name box and the content line: no cancel, confirm or function button
      const icons = await frame.evaluate(() => {
        const formulaBar = document.querySelector('[data-u-comp="formula-bar"]')!
        const name = formulaBar.querySelector('[data-u-comp="defined-name"]')!
        const line = formulaBar.querySelector('canvas')!
        const from = name.getBoundingClientRect().right
        const to = line.getBoundingClientRect().left
        return [...formulaBar.querySelectorAll('svg')].filter((icon) => {
          const rect = icon.getBoundingClientRect()
          return rect.width > 0 && rect.left >= from - 1 && rect.right <= to + 1
        }).length
      })
      expect.soft(icons, 'buttons between the name box and the content line').toBe(0)
      await expect
        .soft(bar.locator('[data-u-comp="defined-name"] [aria-haspopup]'), 'name manager arrow')
        .toBeHidden()

      // a reference typed into the name box moves nothing
      const cell = await nameBox.inputValue()
      await nameBox.click()
      await page.keyboard.press('ControlOrMeta+a')
      await page.keyboard.type('C5')
      await page.keyboard.press('Enter')
      await page.waitForTimeout(500)
      await expect.soft(nameBox, 'name box after typing a reference').toHaveValue(cell)

      // a press on the content line takes no caret, and typing there shows nothing
      await page.mouse.click(box.x + box.width / 2, box.y + box.height / 2)
      await frame.evaluate(() => document.activeElement?.setAttribute('data-focus-before', ''))
      await bar.locator('canvas').click({ position: { x: 12, y: 10 } })
      expect
        .soft(
          await frame.evaluate(() => document.activeElement?.hasAttribute('data-focus-before')),
          'focus after a press on the content line',
        )
        .toBe(true)
      const beforeTyping = await viewer.screenshot()
      await page.keyboard.type('ZZTOP')
      await page.waitForTimeout(500)
      expect
        .soft(await changedPixels(page, beforeTyping, await viewer.screenshot()), 'typed pixels')
        .toBe(0)

      // one button stands before the sheet tabs, and it lists the sheets
      const tabs = app.locator('[data-u-comp="slide-tab-item"]')
      const beforeTabs = await frame.evaluate(() => {
        const first = document.querySelector('[data-u-comp="slide-tab-bar"]')!
        const left = first.getBoundingClientRect().left
        return [...first.closest('footer')!.querySelectorAll('button')].filter((button) => {
          const rect = button.getBoundingClientRect()
          return rect.width > 0 && rect.right <= left + 1
        }).length
      })
      expect.soft(beforeTabs, 'buttons before the sheet tabs').toBe(1)
      await app.locator('#univer-container footer button:visible').first().click()
      await expect.soft(app.getByRole('menuitem', { name: 'Other' })).toBeVisible()
      await page.keyboard.press('Escape')

      // another sheet opens on a press; the open sheet's tab takes no rename, no drag, no menu
      const open = app.locator('[data-u-comp="slide-tab-item"][aria-selected="true"]')
      const other = app.locator('[data-u-comp="slide-tab-item"][aria-selected="false"]').last()
      const otherName = await other.innerText()
      await other.click()
      await expect.soft(open, 'sheet after a press on its tab').toHaveText(otherName)
      await open.dblclick()
      await page.waitForTimeout(400)
      expect
        .soft(await shownCount(frame, '[data-u-comp="slide-tab-bar"] [contenteditable="true"]'))
        .toBe(0)
      await page.keyboard.press('Escape')
      const tab = (await open.boundingBox())!
      await page.mouse.move(tab.x + tab.width / 2, tab.y + tab.height / 2)
      await page.mouse.down()
      await page.waitForTimeout(700)
      expect
        .soft(await open.evaluate((el) => (el as HTMLElement).style.cursor), 'held tab')
        .not.toBe('move')
      await page.mouse.up()
      await open.click({ button: 'right' })
      await page.waitForTimeout(400)
      expect.soft(await shownCount(frame, '[role="menu"]'), 'sheet tab menu').toBe(0)
      await page.keyboard.press('Escape')
      expect.soft(await tabs.count(), 'sheets').toBe(3)

      // a right click on a cell opens no editing menu
      await page.mouse.click(box.x + box.width / 2, box.y + box.height / 2, { button: 'right' })
      await page.waitForTimeout(500)
      expect
        .soft(await bodyText(page), 'menu after a right click on a cell')
        .not.toMatch(/붙여넣기|자르기|이름 변경|시트 보호/)
      expect.soft(problems).toEqual([])
      await attachScreenshot(page, testInfo, `xlsx-pressing-${width}-${browserName}`)
    })

    test(`docx view mode has no way into an edit at ${width}px`, async ({
      page,
      browserName,
    }, testInfo) => {
      const problems: string[] = []
      page.on('pageerror', (error) => problems.push(error.message))
      const app = await openForViewing(
        page,
        at('fixtures/generated/kitchen-sink.docx'),
        width,
        '.ProseMirror',
      )
      const frame = appFrames(page)[0]!
      // the pointer over a picture or an equation brings no handle and no edit button
      for (const block of ['.doc-protected-image', '.doc-protected-formula']) {
        const target = app.locator(block).first()
        await target.scrollIntoViewIfNeeded()
        await target.hover()
        await page.waitForTimeout(400)
        expect
          .soft(
            await shownCount(frame, '.doc-move-handle, .doc-formula-edit, .img-resize-handle'),
            `handles over ${block}`,
          )
          .toBe(0)
        const cursors = await target.evaluate((el) =>
          [el, ...el.querySelectorAll('*')]
            .filter((node) => node.getClientRects().length > 0)
            .map((node) => getComputedStyle(node).cursor),
        )
        expect.soft(cursors.join(' '), `cursor over ${block}`).not.toMatch(/grab|move|resize/)
      }

      // a drag of the picture moves nothing, and the browser lifts no block either
      await frame.evaluate(() => {
        window.addEventListener('dragstart', (event) => {
          document.documentElement.dataset['dragStarted'] = String(!event.defaultPrevented)
        })
      })
      const image = app.locator('.doc-protected-image img').first()
      const picture = (await image.boundingBox())!
      await page.mouse.move(picture.x + picture.width / 2, picture.y + picture.height / 2)
      await page.mouse.down()
      await page.mouse.move(picture.x + picture.width / 2 + 40, picture.y - 60, { steps: 6 })
      await page.mouse.up()
      await page.waitForTimeout(500)
      const dropped = (await image.boundingBox())!
      expect
        .soft([Math.round(dropped.x), Math.round(dropped.y)], 'picture after a drag')
        .toEqual([Math.round(picture.x), Math.round(picture.y)])
      expect
        .soft(
          await frame.evaluate(() => document.documentElement.dataset['dragStarted']),
          'drag of a block',
        )
        .not.toBe('true')
      expect.soft(problems).toEqual([])
      await attachScreenshot(page, testInfo, `docx-pressing-${width}-${browserName}`)
    })

    test(`pdf view mode has no way into an edit at ${width}px`, async ({
      page,
      browserName,
    }, testInfo) => {
      const problems: string[] = []
      page.on('pageerror', (error) => problems.push(error.message))
      const app = await openForViewing(
        page,
        at('apps/pdf/tests/fixtures/plain-sample.pdf'),
        width,
        '.tb-page-input',
      )
      // a phone-width page opens without the thumbnails: the button on the edge brings them
      if (width === 390) await app.locator('.pdf-side-reopen').click()
      // a right click on a page thumbnail opens no page menu (rotate, delete, insert)
      await app.locator('.pdf-thumb').first().click({ button: 'right' })
      await page.waitForTimeout(400)
      expect
        .soft(await bodyText(page), 'menu after a right click on a thumbnail')
        .not.toMatch(/회전|페이지 삭제|빈 페이지 삽입|페이지 가져오기/)
      expect.soft(problems).toEqual([])
      await attachScreenshot(page, testInfo, `pdf-pressing-${width}-${browserName}`)
    })

    test(`hwp view mode has no way into an edit at ${width}px`, async ({
      page,
      browserName,
    }, testInfo) => {
      const problems: string[] = []
      page.on('pageerror', (error) => problems.push(error.message))
      // a header with a line of text and a picture, over one line of body text
      await openForViewing(
        page,
        at('apps/hwp/tests/fixtures/header-picture-and-text.hwpx'),
        width,
        '.hwp-editor-host iframe',
      )
      const studio = page.frames().find((frame) => frame.url().includes('/studio/'))!
      const sheet = (await page
        .frameLocator('#viewer iframe')
        .frameLocator('.hwp-editor-host iframe')
        .locator('.document-page-canvas')
        .first()
        .boundingBox())!
      const on = (x: number, y: number): [number, number] => [
        sheet.x + sheet.width * x,
        sheet.y + sheet.height * y,
      ]
      const HEADER_TEXT = on(0.3, 0.09)
      const HEADER_PICTURE = on(0.727, 0.084)
      const BODY = on(0.5, 0.3)
      const studioText = (): Promise<string> => studio.evaluate(() => document.body.innerText)
      const paper = (): Promise<Buffer> =>
        page.screenshot({
          clip: { x: sheet.x, y: sheet.y, width: sheet.width, height: sheet.height * 0.35 },
        })

      expect
        .soft(await studio.locator('#status-bar').innerText(), 'status line')
        .not.toMatch(/삽입|수정/)

      // a press in the text shows no caret, a press on a picture no handles
      const untouched = await paper()
      await page.mouse.click(...BODY)
      await page.mouse.click(...HEADER_PICTURE)
      await page.waitForTimeout(600)
      expect
        .soft(await changedPixels(page, untouched, await paper()), 'caret or handles drawn')
        .toBe(0)
      await page.mouse.click(...BODY)

      // a double click on the header starts no header editing
      await page.mouse.dblclick(...HEADER_TEXT)
      await page.waitForTimeout(600)
      expect
        .soft(await studioText(), 'after a double click on the header')
        .not.toMatch(/머리말|꼬리말/)
      await page.keyboard.press('Escape')

      // a right click opens no editing menu
      await page.mouse.click(...BODY, { button: 'right' })
      await page.waitForTimeout(500)
      expect
        .soft(await studioText(), 'menu after a right click')
        .not.toMatch(/오려 두기|붙이기|모양 복사/)
      expect.soft(problems).toEqual([])
      await attachScreenshot(page, testInfo, `hwp-pressing-${width}-${browserName}`)
    })
  }
}
