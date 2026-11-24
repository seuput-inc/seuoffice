import { fileURLToPath } from 'node:url'
import { expect, test, type Locator } from '@playwright/test'
import { openCoEdited, settled } from './collab'

const file = fileURLToPath(new URL('../../../fixtures/generated/simple.docx', import.meta.url))

function beforeCaret(caret: Locator): Promise<string | null> {
  return caret.evaluateAll(([mark]) => {
    const paragraph = mark?.closest('p')
    if (!mark || !paragraph) return null
    const range = document.createRange()
    range.selectNodeContents(paragraph)
    range.setEndBefore(mark)
    return range.toString()
  })
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
  const caret = reader.locator('.docs-presence-caret')
  const label = caret.locator('[data-presence-client]')
  const highlight = reader.locator('.docs-presence-selection')
  const end = process.platform === 'darwin' ? 'Meta+ArrowRight' : 'End'

  await writer.getByText('第二段。').click()
  await alice.page.keyboard.press(end)
  await expect.poll(() => beforeCaret(caret)).toBe('第二段。')
  await expect(label).toBeVisible()
  await expect(label).toHaveText('alice')
  const color = await label.evaluate((el) => getComputedStyle(el).backgroundColor)
  expect(color).toMatch(/^rgb\(\d+, \d+, \d+\)$/)
  await expect(caret).toHaveCSS('border-inline-start-color', color)
  await expect(highlight).toHaveCount(0)

  await alice.page.keyboard.press('Shift+ArrowLeft')
  await alice.page.keyboard.press('Shift+ArrowLeft')
  await expect.poll(() => beforeCaret(caret)).toBe('第二')
  await expect(highlight).toHaveText('段。')
  const tint = await highlight.evaluate((el) => getComputedStyle(el).backgroundColor)
  expect(tint).toMatch(/^rgba\(\d+, \d+, \d+, 0\.\d+\)$/)
  expect(tint.match(/\d+/g)?.slice(0, 3)).toEqual(color.match(/\d+/g))

  await writer.getByText('第一段。').click()
  await alice.page.keyboard.press(end)
  await expect.poll(() => beforeCaret(caret)).toBe('第一段。')
  await expect(highlight).toHaveCount(0)

  // A native browser selection on the non-editable page is not necessarily a PM transaction.
  await reader.getByText('第二段。').evaluate((el) => {
    const text = document.createTreeWalker(el, NodeFilter.SHOW_TEXT).nextNode()!
    getSelection()!.setBaseAndExtent(text, 1, text, 3)
  })
  await expect(writer.locator('.docs-presence-selection')).toHaveText('二段')
  await expect(writer.locator('[data-presence-client]')).toHaveText('bob')
  expect(await settled([alice, bob])).toBe(version)

  await alice.page.context().close()
  await expect(caret).toHaveCount(0, { timeout: 30_000 })
  await expect(highlight).toHaveCount(0)
})
