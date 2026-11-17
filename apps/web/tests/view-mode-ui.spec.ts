import { readdirSync } from 'node:fs'
import { resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { expect, test, type Frame, type Page } from '@playwright/test'

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
    pixels: true,
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
        const before = await bodyText(page)
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
        expect(await bodyText(page)).toBe(before)
        expect(await bodyText(page)).not.toContain('ZZTOP')
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
}
