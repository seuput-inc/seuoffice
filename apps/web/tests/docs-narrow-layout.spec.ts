import { expect, test, type Frame, type Page } from '@playwright/test'
import { strToU8, zipSync } from 'fflate'

/**
 * A Word document is laid out once, at its paper size, and a narrow screen only shows that
 * layout smaller: the same lines, the same page. WebKit keeps text under a page zoom at 9 px or
 * more, so a page zoomed to fit a phone used to grow its text and wrap every paragraph again.
 */
const WORD_NS = 'xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"'
const PACKAGE_RELS = 'http://schemas.openxmlformats.org/package/2006/relationships'
const OFFICE_RELS = 'http://schemas.openxmlformats.org/officeDocument/2006/relationships'
const WORD_TYPE = 'application/vnd.openxmlformats-officedocument.wordprocessingml.document'

const SENTENCE = 'The quick brown fox jumps over the lazy dog and keeps running along the river. '
// enough for a second page: the break between the pages is laid out with the lines
const PARAGRAPHS = 20

/** A4 pages of 11 pt paragraphs, each several lines long: about 6 px text when fitted to 390 px */
function longParagraphsDocx(): Buffer {
  const paragraphs = Array.from(
    { length: PARAGRAPHS },
    (_, index) =>
      '<w:p><w:r><w:rPr><w:sz w:val="22"/></w:rPr>' +
      `<w:t>${index + 1}. ${SENTENCE.repeat(4 + (index % 3))}</w:t></w:r></w:p>`,
  )
  const xml = (body: string): Uint8Array => strToU8(`<?xml version="1.0" encoding="UTF-8"?>${body}`)
  return Buffer.from(
    zipSync({
      '[Content_Types].xml': xml(
        '<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">' +
          '<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>' +
          '<Default Extension="xml" ContentType="application/xml"/>' +
          `<Override PartName="/word/document.xml" ContentType="${WORD_TYPE}.main+xml"/></Types>`,
      ),
      '_rels/.rels': xml(
        `<Relationships xmlns="${PACKAGE_RELS}"><Relationship Id="rId1" Type="${OFFICE_RELS}/officeDocument" Target="word/document.xml"/></Relationships>`,
      ),
      'word/document.xml': xml(
        `<w:document ${WORD_NS}><w:body>` +
          paragraphs.join('') +
          '<w:sectPr><w:pgSz w:w="11906" w:h="16838"/>' +
          '<w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="708" w:footer="708" w:gutter="0"/>' +
          '</w:sectPr></w:body></w:document>',
      ),
    }),
  )
}

/** The page as it is shown, in units of its own width: the same at every size of the screen */
interface ShownLayout {
  /** shown page width over the pane's width: under 1 once the page was scaled to fit */
  fit: number
  /** shown page height over shown page width */
  pageShape: number
  /** lines each paragraph wraps into */
  lines: number[]
  /** each paragraph's height over the page width */
  heights: number[]
  /** how far the pane scrolls sideways */
  sideways: number
  /** pane left of the page less pane right of it, in px */
  offCentre: number
  /** what the pane scrolls past the end of the page, over the shown page height */
  tail: number
}

async function open(page: Page, width: number, mode: 'view' | 'edit' = 'view'): Promise<Frame> {
  await page.setViewportSize({ width, height: 900 })
  await page.goto('/')
  await page.locator('#edit').setChecked(mode === 'edit')
  await page.setInputFiles('#file', {
    name: 'long-paragraphs.docx',
    mimeType: 'application/octet-stream',
    buffer: longParagraphsDocx(),
  })
  await expect(page.locator('#status')).not.toContainText('여는 중', { timeout: 40_000 })
  await expect(page.locator('#status')).not.toContainText('오류')
  const paragraphs = page.frameLocator('#viewer iframe').locator('.editor-scroll .ProseMirror p')
  await expect(paragraphs).toHaveCount(PARAGRAPHS, { timeout: 30_000 })
  return page.frames().find((frame) => frame.url().includes('/docs/'))!
}

/** The page on a phone, once it settled on the zoom that fits it */
async function openFitted(page: Page, mode: 'view' | 'edit' = 'view'): Promise<Frame> {
  const frame = await open(page, 390, mode)
  await expect.poll(async () => (await shownLayout(frame)).fit, { timeout: 15_000 }).toBeLessThan(1)
  return frame
}

function shownLayout(frame: Frame): Promise<ShownLayout> {
  return frame.evaluate(() => {
    const scroller = document.querySelector('.editor-scroll')!
    const paper = document.querySelector('.editor-scroll .doc-page')!.getBoundingClientRect()
    const paragraphs = [...document.querySelectorAll('.editor-scroll .ProseMirror p')]
    const lines = paragraphs.map((paragraph) => {
      const range = document.createRange()
      range.selectNodeContents(paragraph)
      // one rect per line box of the run; lines differ by their top edge
      const tops = [...range.getClientRects()].map((rect) => Math.round(rect.top))
      return new Set(tops).size
    })
    const pane = scroller.getBoundingClientRect()
    return {
      fit: paper.width / scroller.clientWidth,
      pageShape: paper.height / paper.width,
      lines,
      heights: paragraphs.map(
        (paragraph) => paragraph.getBoundingClientRect().height / paper.width,
      ),
      sideways: scroller.scrollWidth - scroller.clientWidth,
      offCentre: paper.left - pane.left - (pane.left + scroller.clientWidth - paper.right),
      tail: (scroller.scrollHeight - (paper.bottom - pane.top + scroller.scrollTop)) / paper.height,
    }
  })
}

test('a Word page fitted to a phone keeps the lines and the shape it has on a wide screen', async ({
  page,
  browserName,
}) => {
  const wide = await shownLayout(await open(page, 1440))
  // every paragraph of the fixture wraps: a second line is what a larger text would push around
  expect(Math.min(...wide.lines)).toBeGreaterThan(1)

  const narrow = await shownLayout(await openFitted(page))

  if (browserName !== 'webkit') {
    // Chromium scales the page with CSS zoom, which lays the text out again at the small size:
    // hinted 6 px text may end a paragraph one line early or late (it does on Linux). Text
    // held at 9 px adds two lines to the longer paragraphs.
    narrow.lines.forEach((count, index) => {
      expect(Math.abs(count - wide.lines[index]!), `paragraph ${index + 1}`).toBeLessThanOrEqual(1)
    })
    return
  }
  expect(narrow.lines).toEqual(wide.lines)
  expect(Math.abs(narrow.pageShape / wide.pageShape - 1)).toBeLessThan(0.01)
  narrow.heights.forEach((height, index) => {
    expect(Math.abs(height / wide.heights[index]! - 1), `paragraph ${index + 1}`).toBeLessThan(0.02)
  })
})

test('a fitted Word page sits in the middle of the pane, which scrolls to the page end and no further', async ({
  page,
}) => {
  const shown = await shownLayout(await openFitted(page))
  expect(shown.sideways).toBeLessThanOrEqual(1)
  expect(Math.abs(shown.offCentre)).toBeLessThanOrEqual(2)
  // the pane's own padding under the page, not the room of the page at its full size
  expect(shown.tail).toBeLessThan(0.5)
})

test('typing lands in the paragraph pressed on a fitted Word page', async ({ page }) => {
  const frame = await openFitted(page, 'edit')
  const third = page.frameLocator('#viewer iframe').locator('.editor-scroll .ProseMirror p').nth(2)
  const box = (await third.boundingBox())!
  // inside the first line, well away from both margins
  await page.mouse.click(box.x + box.width / 2, box.y + box.height / 12)
  await page.keyboard.type('ZZTOP')
  await expect(third).toContainText('ZZTOP')
  const texts = await frame.evaluate(() =>
    [...document.querySelectorAll('.editor-scroll .ProseMirror p')].map((p) => p.textContent ?? ''),
  )
  expect(texts.filter((text) => text.includes('ZZTOP'))).toHaveLength(1)
})
