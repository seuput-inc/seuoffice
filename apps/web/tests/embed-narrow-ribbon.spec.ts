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
 * A phone-width embed in edit mode (390 px, as YUYU shows it on a phone) next to a desktop width
 * (1440 px). At both widths every tab name of the ribbon renders on one line, and every tab can be
 * brought on screen by scrolling the tab row (by the keyboard where the tabs are buttons). Nothing in
 * the ribbon's body wraps at 390 that does not wrap at 1440, and a body wider than the page scrolls.
 */

interface Ribbon {
  app: string
  file: string
  /** the frame the ribbon is drawn in: the app's page, or the studio frame inside the hwp page */
  frame: string
  /** every tab name of the tab row */
  tabs: string
  body: string
  /** the tabs are buttons: Tab reaches them, and a focused one is scrolled on screen */
  buttons: boolean
}

const RIBBONS: Ribbon[] = [
  {
    app: 'docs',
    file: join(repo, 'fixtures/generated/simple.docx'),
    frame: '/docs/',
    tabs: '.ribbon-tabs .ribbon-tab',
    body: '[data-ribbon-body]',
    buttons: true,
  },
  {
    app: 'sheets',
    file: join(repo, 'apps/sheets/fixtures/generated/compatibility-edit.xlsx'),
    frame: '/sheets/',
    tabs: '.ribbon-tabs button:not(.qa-btn)',
    body: '[data-ribbon-body]',
    buttons: true,
  },
  {
    app: 'slides',
    file: join(repo, 'packages/pptx-engine/tests/fixtures/01_standard_business.pptx'),
    frame: '/slides/',
    tabs: '.ribbon-tabs .ribbon-tab',
    body: '[data-ribbon-body]',
    buttons: true,
  },
  {
    app: 'pdf',
    file: join(repo, 'apps/pdf/tests/fixtures/plain-sample.pdf'),
    frame: '/pdf/',
    tabs: '.ribbon-tabs .ribbon-tab',
    body: '[data-ribbon-body]',
    buttons: true,
  },
  {
    app: 'hwp',
    file: join(repo, 'apps/hwp/tests/fixtures/memo.hwp'),
    frame: '/hwp/studio/',
    // the studio's menu row; its titles are not buttons
    tabs: '#menu-bar .menu-title',
    body: '#icon-toolbar',
    buttons: false,
  },
]

interface Measure {
  /** each tab's name, the lines it renders on and whether it came fully on screen */
  tabs: { name: string; lines: number; reachable: boolean }[]
  /** the body's texts that render on more than one line */
  wrapped: string[]
  /** the body fits the page or scrolls sideways */
  bodyReachable: boolean
}

/** Lay out the ribbon in `frame` and read what a person can see and reach */
function measure(frame: Frame, ribbon: Ribbon): Promise<Measure> {
  return frame.evaluate(({ tabs, body, buttons }) => {
    /** the distinct lines the text of `element` renders on */
    const lines = (element: Element): number => {
      const tops = new Set<number>()
      const walker = document.createTreeWalker(element, NodeFilter.SHOW_TEXT)
      for (let node = walker.nextNode(); node; node = walker.nextNode()) {
        if (!node.textContent?.trim()) continue
        const range = document.createRange()
        range.selectNodeContents(node)
        for (const rect of range.getClientRects())
          if (rect.width > 0) tops.add(Math.round(rect.top))
      }
      return tops.size
    }
    const onScreen = (element: HTMLElement): boolean => {
      const rect = element.getBoundingClientRect()
      if (rect.width === 0 || rect.left < -0.5 || rect.right > innerWidth + 0.5) return false
      const hit = document.elementFromPoint(rect.left + rect.width / 2, rect.top + rect.height / 2)
      return hit !== null && element.contains(hit)
    }
    const tabList = [...document.querySelectorAll<HTMLElement>(tabs)].filter(
      (tab) => tab.getClientRects().length > 0,
    )
    const read = tabList.map((tab) => {
      if (buttons) tab.focus()
      // centred where the row allows it: the studio's row keeps a sticky button at its right end
      else tab.scrollIntoView({ block: 'nearest', inline: 'center' })
      const focused = !buttons || (document.activeElement === tab && tab.tabIndex >= 0)
      return {
        name: tab.textContent!.trim(),
        lines: lines(tab),
        // the row scrolls, never the page itself
        reachable: focused && onScreen(tab) && document.scrollingElement!.scrollLeft === 0,
      }
    })
    ;(document.activeElement as HTMLElement | null)?.blur()
    const bodyElement = document.querySelector<HTMLElement>(body)!
    const wrapped: string[] = []
    const walker = document.createTreeWalker(bodyElement, NodeFilter.SHOW_ELEMENT)
    for (let node = walker.nextNode(); node; node = walker.nextNode()) {
      const element = node as HTMLElement
      const own = [...element.childNodes].some(
        (child) => child.nodeType === Node.TEXT_NODE && child.textContent!.trim(),
      )
      if (own && element.getClientRects().length > 0 && lines(element) > 1)
        wrapped.push(element.textContent!.trim())
    }
    const overflowX = getComputedStyle(bodyElement).overflowX
    return {
      tabs: read,
      wrapped,
      bodyReachable:
        bodyElement.scrollWidth <= bodyElement.clientWidth + 1 ||
        overflowX === 'auto' ||
        overflowX === 'scroll',
    }
  }, ribbon)
}

test.beforeAll(buildEmbedScript)

let host: Site
let bundle: Site
test.beforeEach(async () => ({ host, bundle } = await serveHostAndBundle()))
test.afterEach(() => Promise.all([host.close(), bundle.close()]))

const ribbonFrame = async (page: Page, ribbon: Ribbon): Promise<Frame> => {
  await expect
    .poll(() => page.frames().some((frame) => frame.url().includes(ribbon.frame)), {
      timeout: 60_000,
    })
    .toBe(true)
  return page.frames().find((frame) => frame.url().includes(ribbon.frame))!
}

for (const ribbon of RIBBONS) {
  test(`${ribbon.app}: every tab name on one line and in reach, at 1440 and at 390`, async ({
    page,
  }, testInfo) => {
    test.setTimeout(180_000)
    await page.setViewportSize({ width: 1440, height: 900 })
    await embed(page, origin(host, HOST_NAME), `${origin(bundle, BUNDLE_HOST)}/`, {
      file: ribbon.file,
      mode: 'edit',
    })
    await expectOpened(page)
    const frame = await ribbonFrame(page, ribbon)
    await expect(frame.locator(ribbon.tabs).first()).toBeVisible({ timeout: 60_000 })
    await expect(frame.locator(ribbon.body)).toBeVisible({ timeout: 30_000 })
    // the first layout settles (fonts, the studio's toolbars) before anything is read
    await page.waitForTimeout(1500)
    /** the page as it first shows, kept beside the report */
    const shot = async (name: string): Promise<void> => {
      const path = testInfo.outputPath(name)
      await page.screenshot({ path })
      await testInfo.attach(name, { path, contentType: 'image/png' })
    }

    await shot(`${ribbon.app}-1440.png`)
    const wide = await measure(frame, ribbon)

    await page.setViewportSize({ width: 390, height: 844 })
    await expect.poll(() => frame.evaluate(() => innerWidth)).toBeLessThanOrEqual(390)
    await page.waitForTimeout(500)
    await shot(`${ribbon.app}-390.png`)
    const narrow = await measure(frame, ribbon)

    expect(wide.tabs.length).toBeGreaterThan(3)
    expect(narrow.tabs.map((tab) => tab.name)).toEqual(wide.tabs.map((tab) => tab.name))
    const fine = (tabs: Measure['tabs']) =>
      tabs.map(({ name }) => ({ name, lines: 1, reachable: true }))
    expect(wide.tabs).toEqual(fine(wide.tabs))
    expect(narrow.tabs).toEqual(fine(narrow.tabs))
    expect(narrow.wrapped.filter((text) => !wide.wrapped.includes(text))).toEqual([])
    expect(narrow.bodyReachable).toBe(true)
  })
}
