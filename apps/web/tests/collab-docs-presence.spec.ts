import { fileURLToPath } from 'node:url'
import { expect, test, type Locator } from '@playwright/test'
import { openCoEdited, settled } from './collab'

const file = fileURLToPath(new URL('../../../fixtures/generated/simple.docx', import.meta.url))

async function atOffset(caret: Locator, paragraph: Locator, offset: number) {
  await expect
    .poll(async () => {
      const box = await caret.boundingBox()
      const target = await paragraph.boundingBox()
      if (!box || !target) return Infinity
      const point = await paragraph.evaluate((el, offset) => {
        const walk = document.createTreeWalker(el, NodeFilter.SHOW_TEXT)
        for (let node = walk.nextNode(); node; node = walk.nextNode()) {
          const length = node.textContent!.length
          if (offset > length) {
            offset -= length
            continue
          }
          const range = document.createRange()
          range.setStart(node, offset)
          range.collapse(true)
          const r = range.getBoundingClientRect()
          const p = el.getBoundingClientRect()
          return { x: r.left - p.left, y: (r.top + r.bottom) / 2 - p.top }
        }
        throw new Error('offset outside paragraph')
      }, offset)
      return Math.max(
        Math.abs(box.x - target.x - point.x),
        Math.abs(box.y + box.height / 2 - target.y - point.y),
      )
    })
    .toBeLessThan(2)
}

test('a viewer follows a named caret and backwards selection, shares a selection, and sees the editor leave', async ({
  browser,
}) => {
  test.setTimeout(180_000)
  const documentId = `docs-presence-${crypto.randomUUID()}`
  const alice = await openCoEdited(browser, file, { documentId, user: 'alice' })
  const bob = await openCoEdited(browser, file, { documentId, user: 'bob', access: 'view' })
  const version = await settled([alice, bob])
  const writer = alice.frame.locator('.ProseMirror').first()
  const reader = bob.frame.locator('.ProseMirror').first()
  await expect(reader).toHaveAttribute('contenteditable', 'false')
  const caret = bob.frame.locator('.docs-presence-caret')
  const label = caret.locator('[data-presence-client]')
  const highlight = reader.locator('.docs-presence-selection')
  const end = process.platform === 'darwin' ? 'Meta+ArrowRight' : 'End'

  await writer.getByText('第二段。').click()
  await alice.page.keyboard.press(end)
  await atOffset(caret, reader.locator('p').filter({ hasText: '第二段。' }), 4)
  await expect(label).toBeVisible()
  await expect(label).toHaveText('alice')
  const color = await label.evaluate((el) => getComputedStyle(el).backgroundColor)
  expect(color).toMatch(/^rgb\(\d+, \d+, \d+\)$/)
  await expect(caret).toHaveCSS('border-inline-start-color', color)
  await expect(highlight).toHaveCount(0)

  await alice.page.keyboard.press('Shift+ArrowLeft')
  await alice.page.keyboard.press('Shift+ArrowLeft')
  await atOffset(caret, reader.locator('p').filter({ hasText: '第二段。' }), 2)
  await expect(highlight).toHaveText('段。')
  const tint = await highlight.evaluate((el) => getComputedStyle(el).backgroundColor)
  expect(tint).toMatch(/^rgba\(\d+, \d+, \d+, 0\.\d+\)$/)
  expect(tint.match(/\d+/g)?.slice(0, 3)).toEqual(color.match(/\d+/g))

  await writer.getByText('第一段。').click()
  await alice.page.keyboard.press(end)
  await atOffset(caret, reader.locator('p').filter({ hasText: '第一段。' }), 4)
  await expect(highlight).toHaveCount(0)

  // A native browser selection on the non-editable page is not necessarily a PM transaction.
  await reader.getByText('第二段。').evaluate((el) => {
    const text = document.createTreeWalker(el, NodeFilter.SHOW_TEXT).nextNode()!
    getSelection()!.setBaseAndExtent(text, 1, text, 3)
  })
  await expect(writer.locator('.docs-presence-selection')).toHaveText('二段')
  await expect(alice.frame.locator('.docs-presence-caret [data-presence-client]')).toHaveText('bob')
  expect(await settled([alice, bob])).toBe(version)

  await alice.page.context().close()
  await expect(caret).toHaveCount(0, { timeout: 30_000 })
  await expect(highlight).toHaveCount(0)
})
