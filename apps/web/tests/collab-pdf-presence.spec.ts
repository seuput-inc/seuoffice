import { expect, test, type Locator } from '@playwright/test'
import { PDFDocument, PDFString } from 'pdf-lib'
import { openCoEdited, settled, type CoEditor } from './collab'

const sentence = 'Review this paragraph together.'

async function reviewPdf() {
  const doc = await PDFDocument.create()
  const first = doc.addPage([500, 500])
  first.drawText('Approval', { x: 48, y: 430, size: 18 })
  doc
    .getForm()
    .createTextField('Reviewer')
    .addToPage(first, { x: 48, y: 350, width: 160, height: 24 })
  first.node.addAnnot(
    doc.context.register(
      doc.context.obj({
        Type: 'Annot',
        Subtype: 'Text',
        Rect: [280, 350, 300, 370],
        Contents: PDFString.of('Check the amount'),
      }),
    ),
  )
  first.node.addAnnot(
    doc.context.register(
      doc.context.obj({
        Type: 'Annot',
        Subtype: 'Highlight',
        Rect: [320, 350, 420, 374],
        QuadPoints: [320, 374, 420, 374, 320, 350, 420, 350],
        C: [1, 1, 0],
        F: 4,
      }),
    ),
  )
  const second = doc.addPage([500, 500])
  second.drawText(sentence, { x: 48, y: 430, size: 18 })
  return { name: 'review.pdf', mimeType: 'application/pdf', buffer: Buffer.from(await doc.save()) }
}

const label = (editor: CoEditor, name: string) =>
  editor.frame.locator('.pdf-page [data-presence-client]').filter({ hasText: name })

async function expectOutline(tag: Locator, target: Locator) {
  await expect(tag).toBeVisible()
  const mark = tag.locator('..')
  await expect
    .poll(async () => {
      const a = await mark.boundingBox(),
        b = await target.boundingBox()
      return a && b
        ? Math.max(
            Math.abs(a.x - b.x),
            Math.abs(a.y - b.y),
            Math.abs(a.width - b.width),
            Math.abs(a.height - b.height),
          )
        : Infinity
    })
    .toBeLessThan(2)
  const color = await tag.evaluate((el) => getComputedStyle(el).backgroundColor)
  expect(color).not.toBe('rgba(0, 0, 0, 0)')
  await expect(mark).toHaveCSS('outline-color', color)
  await expect(mark).toHaveCSS('pointer-events', 'none')
}

test('a PDF viewer and editor see named selections on their own page layout without editing the file', async ({
  browser,
}) => {
  test.setTimeout(120_000)
  const file = await reviewPdf()
  const documentId = `pdf-presence-${crypto.randomUUID()}`
  const editors: CoEditor[] = []
  try {
    const alice = await openCoEdited(browser, file, {
      documentId,
      user: 'Alice',
      ready: 'canvas',
      viewport: { width: 1400, height: 1100 },
    })
    editors.push(alice)
    const bob = await openCoEdited(browser, file, {
      documentId,
      user: 'Bob',
      access: 'view',
      ready: 'canvas',
      viewport: { width: 1080, height: 900 },
    })
    editors.push(bob)
    const before = await settled(editors)
    const aliceField = alice.frame.locator('.pdf-page input[title="Reviewer"]')
    const bobField = bob.frame.getByRole('button', { name: 'Reviewer', exact: true })
    await bobField.click()
    await expectOutline(label(alice, 'Bob'), aliceField)
    await aliceField.click()
    await expectOutline(label(bob, 'Alice'), bobField)

    const note = '.pdf-page[data-page="0"] .pdf-note-pin'
    await bob.frame.locator(note).click()
    await expectOutline(label(alice, 'Bob'), alice.frame.locator(note))
    await expect(label(alice, 'Bob')).toHaveCount(1)

    const bobPage = bob.frame.locator('.pdf-page[data-page="0"]')
    const zoom = await bobPage.evaluate((p) =>
      Number((p as HTMLElement).style.getPropertyValue('--scale-factor')),
    )
    await bobPage.click({ position: { x: 370 * zoom, y: 138 * zoom } })
    await expect(bobPage.locator('.pdf-markup-selected')).toBeVisible()
    await expect
      .poll(() =>
        label(alice, 'Bob').evaluate((el) => {
          const box = el.parentElement!.getBoundingClientRect()
          const page = el.closest('.pdf-page')!.getBoundingClientRect()
          return (
            Math.abs((box.left - page.left) / page.width - 320 / 500) +
            Math.abs(box.width / page.width - 100 / 500)
          )
        }),
      )
      .toBeLessThan(0.005)

    const line = (ed: CoEditor) =>
      ed.frame.locator('.pdf-page[data-page="1"] .textLayer span', { hasText: sentence })
    await bob.frame.locator('.pdf-page[data-page="1"]').scrollIntoViewIfNeeded()
    await line(bob).dblclick()
    await alice.frame.locator('.pdf-page[data-page="1"]').scrollIntoViewIfNeeded()
    const range = alice.frame.locator('.pdf-page[data-page="1"] .pdf-presence-range', {
      hasText: 'Bob',
    })
    await expect(range).toBeVisible()
    const onLine = await line(alice).boundingBox()
    const word = await range.boundingBox()
    expect(word!.x).toBeGreaterThanOrEqual(onLine!.x - 2)
    expect(word!.x + word!.width).toBeLessThanOrEqual(onLine!.x + onLine!.width + 2)
    expect(word!.y).toBeLessThan(onLine!.y + onLine!.height)
    expect(word!.y + word!.height).toBeGreaterThan(onLine!.y)

    await alice.frame.locator('.ribbon-tab', { hasText: '편집' }).first().click()
    await alice.frame.getByRole('button', { name: '텍스트 편집', exact: true }).click()
    await line(alice).click()
    const draft = alice.frame.locator('textarea.pdf-textedit-input')
    await expect(draft).toBeFocused()
    await draft.press('Home')
    const caret = bob.frame.locator('.pdf-page[data-page="1"] .pdf-presence-caret', {
      hasText: 'Alice',
    })
    await expect(caret).toBeVisible()
    const bobLine = (await line(bob).boundingBox())!
    await expect
      .poll(async () => Math.abs(((await caret.boundingBox())?.x ?? Infinity) - bobLine.x))
      .toBeLessThan(8)
    await draft.press('End')
    await expect
      .poll(async () => (await caret.boundingBox())?.x ?? 0)
      .toBeGreaterThan(bobLine.x + 30)
    await draft.press('Shift+ArrowLeft')
    const selected = bob.frame.locator('.pdf-page[data-page="1"] .pdf-presence-range', {
      hasText: 'Alice',
    })
    await expect(selected).toBeVisible()
    await expect(caret).toHaveCount(0)
    expect((await selected.boundingBox())!.width).toBeGreaterThan(1)
    await expect(label(bob, 'Alice')).toHaveCount(1)
    expect(await settled(editors)).toBe(before)

    await alice.page.context().close()
    editors.shift()
    await expect(label(bob, 'Alice')).toHaveCount(0)
  } finally {
    await Promise.all(editors.map((ed) => ed.page.context().close()))
  }
})
