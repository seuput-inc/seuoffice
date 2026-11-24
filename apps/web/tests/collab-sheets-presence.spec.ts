import { readFileSync } from 'node:fs'
import { expect, test, type Locator } from '@playwright/test'
import { strFromU8, strToU8, unzipSync, zipSync } from 'fflate'
import { nameBox, openCoEdited, selectCell, type CoEditor } from './collab'

const fixture = new URL(
  '../../sheets/fixtures/generated/compatibility-structure.xlsx',
  import.meta.url,
)

// Let the receiving grid identify the cell under the painted border, independently of the sender.
async function expectCorner(
  editor: CoEditor,
  border: Locator,
  corner: 'first' | 'last',
  address: string,
): Promise<void> {
  await expect(async () => {
    const box = await border.boundingBox({ timeout: 2_000 })
    if (!box) throw new Error('The selection border is not on screen')
    const x = box.x + (corner === 'first' ? 4 : box.width - 4)
    const y = box.y + (corner === 'first' ? 4 : box.height - 4)
    await editor.page.mouse.click(x, y)
    await expect(nameBox(editor.frame)).toHaveValue(address, { timeout: 1_000 })
  }).toPass({ timeout: 15_000 })
}

test('a viewer sees and shares cell selections on the matching sheet until the peer leaves', async ({
  browser,
}) => {
  test.setTimeout(120_000)
  const documentId = `sheet-presence-${crypto.randomUUID()}`
  const parts = unzipSync(readFileSync(fixture))
  const path = 'xl/worksheets/sheet1.xml'
  parts[path] = strToU8(
    strFromU8(parts[path]!).replace(
      '<cols>',
      '<sheetViews><sheetView workbookViewId="0"><pane xSplit="2" ySplit="2" topLeftCell="C3" state="frozen"/></sheetView></sheetViews><cols>',
    ),
  )
  const file = {
    name: 'frozen.xlsx',
    mimeType: 'application/vnd.openxmlformats-officedocument.spreadsheetml.sheet',
    buffer: Buffer.from(zipSync(parts)),
  }
  const alice = await openCoEdited(browser, file, { documentId, user: 'Alice', ready: 'canvas' })
  const bob = await openCoEdited(browser, file, {
    documentId,
    user: 'Bob',
    access: 'view',
    ready: 'canvas',
  })
  try {
    const label = bob.frame.locator('[data-presence-client]').filter({ hasText: /^Alice$/ })
    const border = label.locator('..')
    await selectCell(alice, 'B7')
    await expect(label).toBeVisible()
    const color = await label.evaluate((el) => getComputedStyle(el).backgroundColor)
    expect(color).toMatch(/^rgb\(/)
    await expect(border).toHaveCSS('border-top-color', color)
    await expect(border).toHaveCSS('border-top-style', 'solid')
    await expectCorner(bob, border, 'first', 'B7')

    const address = nameBox(alice.frame)
    await address.fill('d8:e9')
    await address.press('Enter')
    await bob.frame.locator('button.zoom-btn').filter({ hasText: '+' }).click()
    await expectCorner(bob, border, 'first', 'D8')
    await expectCorner(bob, bob.frame.locator('.sheet-presence-active'), 'last', 'D8')
    await expectCorner(bob, border, 'last', 'E9')
    await expect(label).toHaveCSS('background-color', color)

    await bob.page.mouse.wheel(0, 1000)
    await expect(label).toBeHidden()
    await selectCell(alice, 'A2')
    await expectCorner(bob, border, 'first', 'A2')
    await address.fill('d8:e9')
    await address.press('Enter')
    // Wheel over the scrolling body rather than the frozen corner just clicked.
    const frame = (await bob.page.locator('#viewer iframe').boundingBox())!
    await bob.page.mouse.move(frame.x + frame.width / 2, frame.y + frame.height / 2)
    await bob.page.mouse.wheel(0, -2000)
    await expectCorner(bob, border, 'last', 'E9')

    const tabs = bob.frame.locator('[data-u-comp="slide-tab-item"]')
    await tabs.filter({ hasText: 'Other' }).click()
    await expect(label).toHaveCount(0)
    await tabs.filter({ hasText: 'Data' }).click()
    await expectCorner(bob, border, 'last', 'E9')

    const viewerLabel = alice.frame.locator('[data-presence-client]').filter({ hasText: /^Bob$/ })
    await expectCorner(alice, viewerLabel.locator('..'), 'first', 'E9')
    expect((await bob.status()).version).toBe(0)
    await alice.page.context().close()
    await expect(label).toHaveCount(0)
  } finally {
    if (!alice.page.isClosed()) await alice.page.context().close()
    await bob.page.context().close()
  }
})
