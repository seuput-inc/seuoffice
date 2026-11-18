import { join } from 'node:path'
import { expect, test } from '@playwright/test'
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
 * Zotero reaches a document through the desktop app, which talks to the Zotero running on the
 * same computer. A web page has no such connection, so an embedded edit page offers no Zotero
 * tool on any ribbon tab: there is nothing to press that could only fail. Read at a desktop
 * width (1440 px) and at a phone width (390 px, as YUYU shows it on a phone).
 */

test.beforeAll(buildEmbedScript)

let host: Site
let bundle: Site
test.beforeEach(async () => ({ host, bundle } = await serveHostAndBundle()))
test.afterEach(() => Promise.all([host.close(), bundle.close()]))

for (const width of [1440, 390]) {
  test(`docs at ${width}: no ribbon tab offers a Zotero tool`, async ({ page }) => {
    test.setTimeout(180_000)
    await page.setViewportSize({ width, height: 900 })
    await embed(page, origin(host, HOST_NAME), `${origin(bundle, BUNDLE_HOST)}/`, {
      file: join(repo, 'fixtures/generated/simple.docx'),
      mode: 'edit',
    })
    await expectOpened(page)
    const frame = page.frames().find((candidate) => candidate.url().includes('/docs/'))!
    const tabs = frame.locator('.ribbon-tabs .ribbon-tab').filter({ visible: true })
    await expect(tabs.first()).toBeVisible({ timeout: 60_000 })
    const tools = frame.locator('[data-ribbon-body] button').filter({ visible: true })
    // the tools are switched on once the document is in the editor
    await expect(tools.and(frame.locator(':enabled')).first()).toBeVisible({ timeout: 60_000 })

    const names = await tabs.allTextContents()
    expect(names.length).toBeGreaterThan(3)
    // last to first: the first tab is the open one, and a press on the open tab folds the ribbon
    for (const name of names.reverse()) {
      await tabs.filter({ hasText: name }).first().click()
      await expect(tools.first(), `the tools of ${name}`).toBeVisible()
      await expect(
        frame.getByText(/zotero/i).filter({ visible: true }),
        `Zotero on ${name}`,
      ).toHaveCount(0)
    }
  })
}
