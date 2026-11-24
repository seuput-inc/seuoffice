import { fileURLToPath } from 'node:url'
import { expect, test, type Locator } from '@playwright/test'
import { openCoEdited, type CoEditor } from './collab'

const file = fileURLToPath(
  new URL(
    '../../../packages/pptx-engine/tests/fixtures/01_standard_business.pptx',
    import.meta.url,
  ),
)
const stage = (p: CoEditor) => p.frame.locator('.reading-slide, .stage-rel').first()
const label = (p: CoEditor, name: string) =>
  p.frame.locator('[data-presence-client]').filter({ hasText: name })

type Box = { x: number; y: number; w: number; h: number }
type ShownSlide = {
  widthPx: number
  nodes: { box: Box; text?: { lines: { runs: { text: string }[] }[] } }[]
}
async function boxes(p: CoEditor): Promise<Box[]> {
  return stage(p).evaluate(async () => {
    const { slidesApi } = window as unknown as {
      slidesApi: { getRenderSlides(): Promise<ShownSlide[]> }
    }
    const slide = (await slidesApi.getRenderSlides())?.[0]
    if (!slide) throw new Error('first slide not drawn')
    return ['Q3 Business Review', 'Fiscal Year 2026'].map((text) => {
      const b = slide.nodes.find(
        (n) =>
          'text' in n && n.text?.lines.some((l) => l.runs.map((r) => r.text).join('') === text),
      )!.box
      return {
        x: b.x / slide.widthPx,
        y: b.y / slide.widthPx,
        w: b.w / slide.widthPx,
        h: b.h / slide.widthPx,
      }
    })
  })
}

async function clickBox(p: CoEditor, box: Box, double = false) {
  const rect = (await stage(p).boundingBox())!
  const x = rect.x + (box.x + box.w / 2) * rect.width,
    y = rect.y + (box.y + box.h / 2) * rect.width
  if (double) await p.page.mouse.dblclick(x, y)
  else await p.page.mouse.click(x, y)
}

async function relativeBounds(mark: Locator, p: CoEditor): Promise<Box | null> {
  const [surface, r] = await Promise.all([stage(p).boundingBox(), mark.boundingBox()])
  return (
    surface &&
    r && {
      x: (r.x - surface.x) / surface.width,
      y: (r.y - surface.y) / surface.width,
      w: r.width / surface.width,
      h: r.height / surface.width,
    }
  )
}

async function atBox(mark: Locator, p: CoEditor, expected: Box) {
  await expect(mark).toBeVisible()
  await expect
    .poll(async () => {
      const b = await relativeBounds(mark, p)
      return b
        ? Math.max(...(['x', 'y', 'w', 'h'] as const).map((k) => Math.abs(b[k] - expected[k])))
        : Infinity
    })
    .toBeLessThan(0.003)
}

test('slides show the other person’s selected objects, caret and text range, including in reading view', async ({
  browser,
}) => {
  test.setTimeout(120_000)
  const documentId = `slide-presence-${crypto.randomUUID()}`
  const alice = await openCoEdited(browser, file, { documentId, user: 'Alice', ready: 'canvas' })
  const bob = await openCoEdited(browser, file, {
    documentId,
    user: 'Bob',
    access: 'view',
    ready: 'canvas',
    viewport: { width: 1080, height: 760 },
  })
  try {
    const [title, subtitle] = await boxes(alice)
    const onBob = bob.frame.locator('[data-slide-presence-object]')
    const onAlice = alice.frame.locator('[data-slide-presence-object]')
    await clickBox(alice, title!)
    await expect(label(bob, 'Alice')).toBeVisible()
    await atBox(onBob, bob, title!)
    const color = await label(bob, 'Alice').evaluate((e) => getComputedStyle(e).backgroundColor)
    await expect(onBob).toHaveCSS('outline-color', color)
    await clickBox(alice, subtitle!)
    await atBox(onBob, bob, subtitle!)

    await clickBox(bob, title!)
    await expect(label(alice, 'Bob')).toBeVisible()
    await atBox(onAlice, alice, title!)
    await bob.page.keyboard.down('Shift')
    await clickBox(bob, subtitle!)
    await bob.page.keyboard.up('Shift')
    await expect(onAlice).toHaveCount(2)
    await clickBox(bob, subtitle!)
    await expect(onAlice).toHaveCount(1)
    await atBox(onAlice, alice, subtitle!)
    const readerText = bob.frame.locator('.slide-reader-text')
    await readerText.getByText('Fiscal', { exact: false }).last().dblclick()
    await expect(alice.frame.locator('[data-slide-presence="range"]').first()).toBeVisible()
    expect(
      await readerText.evaluate((el) => el instanceof HTMLElement && el.isContentEditable),
    ).toBe(false)

    await clickBox(alice, subtitle!, true)
    const editor = alice.frame.locator('.slide-text-editor')
    await expect(editor).toBeFocused()
    await editor.press('ControlOrMeta+A')
    await editor.press('ArrowLeft')
    await editor.press('ArrowRight')
    const caret = bob.frame.locator('[data-slide-presence="caret"]')
    const localCaret = () =>
      editor.evaluate(() => {
        const r = window.getSelection()!.getRangeAt(0).getBoundingClientRect()
        const s = document.querySelector('.stage-rel')!.getBoundingClientRect()
        return {
          x: (r.x - s.x) / s.width,
          y: (r.y - s.y) / s.width,
          w: 2 / s.width,
          h: r.height / s.width,
        }
      })
    const first = await localCaret()
    await atBox(caret, bob, first)
    await editor.press('ArrowRight')
    const moved = await localCaret()
    expect(moved.x).toBeGreaterThan(first.x)
    await atBox(caret, bob, moved)
    await editor.press('Shift+ArrowRight')
    await editor.press('Shift+ArrowRight')
    await expect(caret).toHaveCount(0)
    const range = bob.frame.locator('[data-slide-presence="range"]').first()
    await expect(range).toBeVisible()
    await expect(range.locator('span').first()).toHaveCSS('background-color', color)
    expect((await range.boundingBox())!.width).toBeGreaterThan(2)

    await bob.frame.locator('.reading-bar button').last().click()
    await expect(label(bob, 'Alice')).toHaveCount(0)
    await expect(label(alice, 'Bob')).toHaveCount(0)
    await bob.frame.locator('.reading-bar button').first().click()
    await expect(label(bob, 'Alice')).toBeVisible()
    expect(await bob.status()).toMatchObject({ version: 0, canEdit: false, unconfirmed: 0 })
    await alice.page.close()
    await expect(label(bob, 'Alice')).toHaveCount(0)
  } finally {
    await Promise.all([alice.page.context().close(), bob.page.context().close()])
  }
})
