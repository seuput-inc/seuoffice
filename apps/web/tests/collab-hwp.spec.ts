import { randomUUID } from 'node:crypto'
import { readFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import { dirname, join } from 'node:path'
import { expect, test, type Browser, type BrowserContext, type Frame } from '@playwright/test'
import init, { HwpDocument } from '@rhwp/core'
import { unzipSync, zipSync } from 'fflate'
import type { AgentPlace, AgentPluginSurface } from '../../hwp/studio-plugin/agent-plugin.mjs'
import type { Change } from '../../../packages/collab/src/model'
import { settled, storedFile, type CoEditor } from './collab'

type Format = 'hwp' | 'hwpx'
const paragraphs = ['First', 'Middle', 'Last']
const imageFixture = new URL(
  '../../hwp/tests/fixtures/picture-brightness-contrast-cell-fill.hwpx',
  import.meta.url,
)
const png = Buffer.from(unzipSync(readFileSync(imageFixture))['BinData/image1.png']!)
const contexts: BrowserContext[] = []

test.setTimeout(180_000)
test.beforeAll(async () => {
  const wasm = join(dirname(createRequire(import.meta.url).resolve('@rhwp/core')), 'rhwp_bg.wasm')
  await init({ module_or_path: readFileSync(wasm) })
})
test.afterEach(async () => {
  await Promise.all(contexts.splice(0).map((context) => context.close()))
})

interface Fixture {
  documentId: string
  file: { name: string; mimeType: string; buffer: Buffer }
  table?: AgentPlace
}

interface Editor extends CoEditor {
  studio: Frame
  history: Change[]
  queued: (() => void)[]
  hold(): void
  release(): void
}

interface StudioWindow extends Window {
  rhwpStudio: {
    plugins: {
      load(id: string): Promise<unknown>
      invoke(id: string, method: string, args: unknown[]): unknown
    }
  }
}

function fixture(format: Format, withRows = false): Fixture {
  const doc = HwpDocument.createEmpty()
  try {
    doc.createBlankDocument()
    for (const [index, line] of paragraphs.entries()) {
      const end = doc.getParagraphLength(0, index)
      doc.splitParagraph(0, index, doc.textToLogicalOffset(0, index, end))
      doc.insertText(0, index + 1, 0, line)
    }
    let table: AgentPlace | undefined
    if (withRows) {
      const { paraIdx, controlIdx, ok } = JSON.parse(doc.createTable(0, 3, 4, 2, 1))
      expect(ok).toBe(true)
      for (const [cell, text] of ['remove row', 'keep row'].entries()) {
        doc.insertTextInCell(0, paraIdx, controlIdx, cell, 0, 0, text)
      }
      table = {
        sec: 0,
        para: paraIdx,
        path: [{ controlIndex: controlIdx, cellIndex: 0, cellParaIndex: 0 }],
        offset: 0,
        length: 0,
      }
    }
    const bytes = format === 'hwp' ? doc.exportHwp() : doc.exportHwpx()
    return {
      documentId: randomUUID(),
      file: {
        name: `shared.${format}`,
        mimeType: 'application/octet-stream',
        buffer: Buffer.from(bytes),
      },
      table,
    }
  } finally {
    doc.free()
  }
}

async function invoke<K extends keyof AgentPluginSurface>(
  studio: Frame,
  method: K,
  ...args: Parameters<AgentPluginSurface[K]>
): Promise<ReturnType<AgentPluginSurface[K]>> {
  const result = await studio.evaluate(
    async ({ method, args }: { method: string; args: unknown[] }) => {
      // Studio provides this API in its own frame, not on the launcher window.
      const win = window as unknown as StudioWindow
      const { plugins } = win.rhwpStudio
      await plugins.load('ieum-agent')
      return plugins.invoke('ieum-agent', method, args)
    },
    { method, args },
  )
  return result as ReturnType<AgentPluginSurface[K]>
}

async function open(
  browser: Browser,
  base: Fixture,
  user: string,
  access: 'view' | 'edit' = 'edit',
): Promise<Editor> {
  const context = await browser.newContext({ viewport: { width: 1400, height: 900 } })
  contexts.push(context)
  const page = await context.newPage()
  // The socket still connects to the real test server. Only submits wait at the barrier.
  const queued: (() => void)[] = []
  const history: Change[] = []
  let holding = false
  await page.routeWebSocket(
    (url) => url.pathname === '/collab',
    (client) => {
      const server = client.connectToServer()
      client.onMessage((data) => {
        if (holding && typeof data === 'string' && JSON.parse(data).t === 'submit') {
          queued.push(() => server.send(data))
        } else server.send(data)
      })
      server.onMessage((data) => {
        if (typeof data === 'string') {
          const message = JSON.parse(data)
          if (message.t === 'change') history.push(message)
        }
        client.send(data)
      })
    },
  )
  await page.goto(`/?${new URLSearchParams({ collab: base.documentId, user, name: user, access })}`)
  await page.locator('#edit').setChecked(access === 'edit')
  await page.setInputFiles('#file', base.file)
  const frame = page.frameLocator('#viewer iframe')
  await expect(page.locator('body')).toHaveAttribute('data-collab-phase', 'live', {
    timeout: 60_000,
  })
  await expect(
    frame.frameLocator('.hwp-editor-host iframe').locator('#scroll-content canvas').first(),
  ).toBeVisible()
  const studio = page.frames().find((frame) => frame.url().includes('/studio/index.html'))!
  const editor: CoEditor = {
    page,
    frame,
    status: () => page.evaluate(() => window.seuLauncher!.collab!),
    text: () => invoke(studio, 'readText'),
  }
  return {
    ...editor,
    studio,
    history,
    queued,
    hold() {
      holding = true
    },
    release() {
      holding = false
      for (const send of queued.splice(0)) send()
    },
  }
}

/** Show a place through Studio's navigation API, then click the actual page there. */
async function pointAt(editor: Editor, place: AgentPlace) {
  const canvas = editor.studio.locator('#scroll-content canvas').first()
  await canvas.click({ position: { x: 160, y: 145 } })
  expect(await invoke(editor.studio, 'reveal', place)).toEqual({ ok: true })
  const caret = await editor.studio.locator('#scroll-content .caret').boundingBox()
  if (!caret) throw new Error('Studio did not show the requested caret')
  const point = { x: caret.x + 1, y: caret.y + caret.height / 2 }
  await editor.page.mouse.click(point.x, point.y)
  return point
}
const body = (para: number): AgentPlace => ({ sec: 0, para, offset: 0, length: 0 })

/** Both pending edits have left their editors at the same base before Alice can win. */
async function releaseInOrder(alice: Editor, bob: Editor) {
  for (const editor of [alice, bob]) await expect.poll(() => editor.queued.length).toBe(1)
  expect((await alice.status()).version).toBe(0)
  expect((await bob.status()).version).toBe(0)
  alice.release()
  await expect.poll(async () => (await alice.status()).version).toBeGreaterThan(0)
  bob.release()
  return settled([alice, bob])
}

for (const format of ['hwp', 'hwpx'] as const) {
  test(`${format}: simultaneous typing and Korean composition, both saves and peer leave`, async ({
    browser,
  }) => {
    const base = fixture(format)
    const alice = await open(browser, base, 'Alice')
    const bob = await open(browser, base, 'Bob')
    await pointAt(alice, body(1))
    await alice.page.keyboard.press('End')
    await pointAt(bob, body(3))
    await bob.page.keyboard.press('End')
    const mark = alice.studio.locator('[data-presence-client]').filter({ hasText: /^Bob$/ })
    await expect(mark).toBeVisible()
    expect(
      await mark.evaluate((el) => el.closest('#scroll-container, #scroll-content') !== null),
    ).toBe(false)
    alice.hold()
    bob.hold()
    await alice.page.keyboard.type('A')
    await expect(mark).toHaveCount(0)
    const ime = await bob.page.context().newCDPSession(bob.page)
    for (const text of ['ㅎ', '하']) {
      await ime.send('Input.imeSetComposition', {
        text,
        selectionStart: text.length,
        selectionEnd: text.length,
      })
      await expect(bob.studio.locator('.caret-composition')).toHaveText(text)
      expect(await bob.text()).not.toContain(text)
      expect((await bob.status()).unconfirmed).toBe(0)
    }
    expect(await bob.page.evaluate(() => window.seuLauncher!.save())).not.toBeNull()
    await expect(bob.page.evaluate(() => window.seuLauncher!.exportLocal())).rejects.toThrow()
    expect(await storedFile(bob)).toBeNull()
    await expect(bob.studio.locator('.caret-composition')).toHaveText('하')
    await ime.send('Input.insertText', { text: '한' })
    await ime.detach()
    const version = await releaseInOrder(alice, bob)
    const expected = ['FirstA', 'Middle', 'Last한']
    for (const editor of [alice, bob]) {
      expect((await editor.text()).trim().split(/\r?\n/).filter(Boolean)).toEqual(expected)
    }
    const late = await open(browser, base, 'History reader', 'view')
    await settled([alice, bob, late])
    expect((await late.status()).canEdit).toBe(false)
    expect(await late.text()).toBe(await alice.text())
    expect(late.history.map((change) => change.v)).toEqual(
      Array.from({ length: version }, (_, i) => i + 1),
    )
    const committed = late.history
      .filter((change) => change.author === 'Bob')
      .flatMap((change) => change.ops)
    expect(committed).toHaveLength(1)
    expect(JSON.stringify(committed)).toContain('한')
    expect(JSON.stringify(committed)).not.toMatch(/[ㅎ하]/)

    await pointAt(bob, body(3))
    await bob.page.keyboard.press('ArrowRight')
    await expect(mark).toBeVisible()
    const peer = await mark.getAttribute('data-presence-client')
    expect(peer).toBeTruthy()
    await expect
      .poll(() =>
        alice.page.evaluate((client) => window.seuLauncher!.collab!.presences[client!]?.v, peer),
      )
      .toBe(version)
    await expect
      .poll(() =>
        mark.evaluate((label) => {
          const color = getComputedStyle(label).backgroundColor
          return (
            /^rgb\(\d+, \d+, \d+\)$/.test(color) &&
            label.parentElement !== null &&
            getComputedStyle(label.parentElement).backgroundColor === color
          )
        }),
      )
      .toBe(true)

    for (const editor of [alice, bob]) {
      if (editor === bob) await editor.page.locator('#save').click()
      else {
        await editor.studio.locator('#menu-bar .menu-item[data-menu="file"] .menu-title').click()
        await editor.studio.locator('.md-item[data-cmd="ext:ieum-save"]').click()
      }
      await expect.poll(async () => (await storedFile(editor))?.version).toBe(version)
      const reopened = new HwpDocument((await storedFile(editor))!.bytes)
      try {
        expect(reopened.getSourceFormat()).toBe(format)
        const lines = Array.from({ length: reopened.getParagraphCount(0) }, (_, i) =>
          reopened.getTextRange(0, i, 0, reopened.getParagraphLength(0, i)),
        )
        expect(lines).toEqual(['', ...expected])
      } finally {
        reopened.free()
      }
    }
    await expect(mark).toBeVisible()
    const previousCaret = await mark.boundingBox()
    if (!previousCaret) throw new Error('Bob’s caret has no visible position')
    await pointAt(bob, body(1))
    await expect(mark).toBeVisible()
    await expect
      .poll(async () => {
        const current = await mark.boundingBox()
        return current !== null && current.y !== previousCaret.y
      })
      .toBe(true)
    await bob.page.context().close()
    await expect(mark).toHaveCount(0, { timeout: 30_000 })
    await expect
      .poll(() =>
        alice.page.evaluate(() => window.seuLauncher!.collab!.peers.some((p) => p.name === 'Bob')),
      )
      .toBe(false)
  })
}

for (const [kind, format] of [
  ['picture', 'hwp'],
  ['text', 'hwpx'],
] as const) {
  test(`${format}: d9 removed ${kind === 'picture' ? 'row versus picture' : 'paragraph versus text'} leaves only the loser a copy`, async ({
    browser,
  }) => {
    const base = fixture(format, kind === 'picture')
    const alice = await open(browser, base, 'Alice')
    const bob = await open(browser, base, 'Bob')
    const target = base.table ?? body(2)
    await pointAt(alice, target)
    const point = await pointAt(bob, target)
    alice.hold()
    bob.hold()
    if (kind === 'picture') {
      await alice.page.keyboard.press('Control+Backspace')
      await bob.studio.locator('#menu-bar .menu-item[data-menu="insert"] .menu-title').click()
      const chooser = bob.page.waitForEvent('filechooser')
      await bob.studio.locator('.md-item[data-cmd="insert:image"]').click()
      await (await chooser).setFiles({ name: 'stamp.png', mimeType: 'image/png', buffer: png })
      await expect(bob.studio.locator('#scroll-container')).toHaveCSS('cursor', 'crosshair')
      await bob.page.mouse.click(point.x, point.y)
      await expect.poll(() => invoke(bob.studio, 'listPictures')).toMatchObject({ count: 1 })
    } else {
      await alice.page.keyboard.press('Control+y')
      await bob.page.keyboard.type('Z')
      await expect.poll(() => bob.text()).toContain('ZMiddle')
    }
    const version = await releaseInOrder(alice, bob)
    const late = await open(browser, base, 'History reader')
    await settled([alice, bob, late])
    // A fresh client's catch-up is what the server read from its log, not the attempted submits.
    expect(late.history.map((change) => change.v)).toEqual(
      Array.from({ length: version }, (_, i) => i + 1),
    )
    expect(late.history.length).toBeGreaterThan(0)
    expect(late.history.map((change) => change.author)).not.toContain('Bob')
    const notice = (editor: Editor) => editor.frame.locator('.hwp-collab-recovery fieldset')
    for (const editor of [alice, bob, late]) {
      expect(await editor.text()).toBe(await alice.text())
      if (kind === 'picture') {
        const table = await invoke(editor.studio, 'readTable', 1)
        expect(table).toMatchObject({ ok: true, rows: 1, cells: [{ text: 'keep row' }] })
        expect(await invoke(editor.studio, 'listPictures')).toMatchObject({ count: 0 })
      } else {
        expect((await editor.text()).trim().split(/\r?\n/).filter(Boolean)).toEqual([
          'First',
          'Last',
        ])
      }
      await expect(notice(editor)).toHaveCount(editor === bob ? 1 : 0)
    }
    const copy = notice(bob).getByRole('button', { name: /^(복사|Copy)$/ })
    await expect(copy).toBeVisible()
    await bob.page.context().grantPermissions(['clipboard-read', 'clipboard-write'], {
      origin: new URL(bob.page.url()).origin,
    })
    await bob.page.bringToFront()
    await bob.page.evaluate(() => navigator.clipboard.writeText('not the recovered fragment'))
    await copy.click()
    if (kind === 'text') {
      await expect(notice(bob).locator('.hwp-collab-recovery-text')).toHaveText('Z')
      await expect.poll(() => bob.page.evaluate(() => navigator.clipboard.readText())).toBe('Z')
    } else {
      const preview = notice(bob).getByRole('img')
      await expect(preview).toBeVisible()
      const previewBytes = await preview.evaluate(async (img: HTMLImageElement) =>
        Array.from(new Uint8Array(await (await fetch(img.src)).arrayBuffer())),
      )
      expect(Buffer.from(previewBytes)).toEqual(png)
      // Chromium may re-encode a PNG on copy. Compare decoded pixels, not its compression.
      await expect
        .poll(() =>
          preview.evaluate(async (img: HTMLImageElement) => {
            const item = (await navigator.clipboard.read()).find((entry) =>
              entry.types.includes('image/png'),
            )
            if (!item) return false
            const bitmap = await createImageBitmap(await item.getType('image/png'))
            try {
              if (bitmap.width !== img.naturalWidth || bitmap.height !== img.naturalHeight)
                return false
              const canvas = document.createElement('canvas')
              canvas.width = bitmap.width
              canvas.height = bitmap.height
              const context = canvas.getContext('2d')!
              context.drawImage(img, 0, 0)
              const original = canvas.toDataURL()
              context.clearRect(0, 0, canvas.width, canvas.height)
              context.drawImage(bitmap, 0, 0)
              return canvas.toDataURL() === original
            } finally {
              bitmap.close()
            }
          }),
        )
        .toBe(true)
    }
  })
}

for (const format of ['hwp', 'hwpx'] as const) {
  test(`${format}: a late row deletion keeps the confirmed picture and offers the deleter a copy`, async ({
    browser,
  }) => {
    const base = fixture(format, true)
    const alice = await open(browser, base, 'Alice')
    const bob = await open(browser, base, 'Bob')
    await pointAt(alice, base.table!)
    const point = await pointAt(bob, base.table!)
    alice.hold()
    bob.hold()
    await alice.page.keyboard.press('Control+Backspace')
    await bob.studio.locator('#menu-bar .menu-item[data-menu="insert"] .menu-title').click()
    const chooser = bob.page.waitForEvent('filechooser')
    await bob.studio.locator('.md-item[data-cmd="insert:image"]').click()
    await (await chooser).setFiles({ name: 'stamp.png', mimeType: 'image/png', buffer: png })
    await expect(bob.studio.locator('#scroll-container')).toHaveCSS('cursor', 'crosshair')
    await bob.page.mouse.click(point.x, point.y)
    await expect.poll(() => invoke(bob.studio, 'listPictures')).toMatchObject({ count: 1 })
    // Bob's insertion is confirmed first; Alice's deletion arrives against the older base.
    await releaseInOrder(bob, alice)
    const notice = (editor: Editor) => editor.frame.locator('.hwp-collab-recovery fieldset')
    for (const editor of [alice, bob]) {
      expect(await editor.text()).toBe(await bob.text())
      expect(await invoke(editor.studio, 'readTable', 1)).toMatchObject({ ok: true, rows: 2 })
      expect(await invoke(editor.studio, 'listPictures')).toMatchObject({ count: 1 })
      await expect(notice(editor)).toHaveCount(editor === alice ? 1 : 0)
    }
    await expect(notice(alice).locator('.hwp-collab-recovery-text')).toHaveText('remove row')
    await expect(
      notice(alice).getByRole('button', { name: /^(커서 위치에 넣기|Insert at cursor)$/ }),
    ).toHaveCount(0)
    const copy = notice(alice).getByRole('button', { name: /^(복사|Copy)$/ })
    await alice.page.context().grantPermissions(['clipboard-read', 'clipboard-write'], {
      origin: new URL(alice.page.url()).origin,
    })
    await alice.page.bringToFront()
    await copy.click()
    await expect
      .poll(() => alice.page.evaluate(() => navigator.clipboard.readText()))
      .toBe('remove row')
  })
}

for (const format of ['hwp', 'hwpx'] as const) {
  test(`${format}: opaque document data refuses entry without a writable private copy`, async ({
    page,
  }) => {
    const { file, documentId } = fixture(format)
    let buffer: Buffer
    if (format === 'hwp') {
      buffer = readFileSync(
        new URL('../../hwp/tests/fixtures/collab-opaque-record.hwp', import.meta.url),
      )
    } else {
      const parts = unzipSync(file.buffer)
      const section = 'Contents/section0.xml'
      const xml = new TextDecoder().decode(parts[section])
      parts[section] = new TextEncoder().encode(xml.replace('<hp:p ', '<hp:p data-sample="1" '))
      buffer = Buffer.from(zipSync(parts))
    }
    await page.goto(`/?collab=${documentId}&access=edit`)
    await page.locator('#edit').check()
    await page.setInputFiles('#file', { ...file, buffer })
    const app = page.frameLocator('#viewer iframe')
    await expect(app.locator('.hwp-app')).toHaveAttribute('data-phase', 'failed')
    await expect(app.locator('.hwp-notice-detail')).toContainText(
      /뜻을 알 수 없는|cannot interpret/,
    )
    await expect(app.locator('.hwp-notice-detail')).toContainText(
      format === 'hwp' ? '0x3fe' : 'data-sample',
    )
    await expect(page.locator('#save')).toBeHidden()
    expect(await page.evaluate(() => window.seuLauncher!.mode)).toBeNull()
  })
}
