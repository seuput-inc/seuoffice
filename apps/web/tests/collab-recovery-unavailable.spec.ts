/**
 * An admitted file cannot join an epoch of another format. Only this session refusal
 * may fall back to the host's edit lock; a source-preservation refusal must not.
 */
import { readFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { expect, test, type BrowserContext } from '@playwright/test'
import init, { HwpDocument } from '@rhwp/core'
import { openCoEdited } from './collab'

const repo = fileURLToPath(new URL('../../..', import.meta.url))
const files = new Map<string, { name: string; mimeType: string; buffer: Buffer }>()
let seed: BrowserContext | undefined

test.beforeAll(async () => {
  const require = createRequire(import.meta.url)
  const wasm = join(dirname(require.resolve('@rhwp/core')), 'rhwp_bg.wasm')
  await init({ module_or_path: readFileSync(wasm) })
  const doc = HwpDocument.createEmpty()
  try {
    doc.createBlankDocument()
    doc.insertText(0, 0, 0, 'Original')
    for (const format of ['hwp', 'hwpx']) {
      const name = `original.${format}`
      files.set(name, {
        name,
        mimeType: 'application/octet-stream',
        buffer: Buffer.from(format === 'hwp' ? doc.exportHwp() : doc.exportHwpx()),
      })
    }
  } finally {
    doc.free()
  }
})

test.beforeEach(async ({ browser }, info) => {
  const owner = await openCoEdited(browser, resolve(repo, 'fixtures/generated/simple.docx'), {
    documentId: info.testId,
    user: 'owner',
  })
  seed = owner.page.context()
})

test.afterEach(async () => {
  await seed?.close()
  seed = undefined
})

function textOf(bytes: Uint8Array): string {
  const doc = new HwpDocument(bytes)
  try {
    return doc.getTextFileText()
  } finally {
    doc.free()
  }
}

test('an hwp opened with collab ends up open under the edit lock, edits and saves without a version', async ({
  page,
}, info) => {
  const query = new URLSearchParams({
    collab: info.testId,
    user: 'alice',
    name: 'alice',
    access: 'edit',
  })
  await page.goto(`/?${query}`)
  await page.locator('#edit').setChecked(true)
  // The checkbox now describes the next file, not the HWP already opening for editing.
  await page.route(
    '**/hwp/index.html?*',
    async (route) => {
      await page.locator('#edit').uncheck()
      await route.continue()
    },
    { times: 1 },
  )
  const input = files.get('original.hwp')!
  await page.setInputFiles('#file', input)

  const app = page.frameLocator('#viewer iframe')
  await expect(app.locator('.hwp-app[data-phase="ready"]')).toHaveCount(1, { timeout: 60_000 })
  await expect
    .poll(() =>
      page.evaluate(() => ({
        mode: window.seuLauncher!.mode,
        openError: window.seuLauncher!.openError,
      })),
    )
    .toEqual({ mode: 'locked', openError: 'collab_unavailable' })
  expect(await page.evaluate(() => window.seuLauncher!.collab)).toBeNull()
  await expect(page.locator('body')).not.toHaveAttribute('data-collab-phase')
  await expect(page.locator('#status')).toHaveText(input.name)
  await expect(page.locator('#save')).toBeVisible()
  // there is no session whose file could be rescued
  expect(
    await page.evaluate(() => window.seuLauncher!.exportLocal().then(() => 'exported', String)),
  ).toContain('no co-edited document is open')

  const studio = app.frameLocator('.hwp-editor-host iframe')
  await expect(studio.locator('[data-cmd="ext:ieum-save"]')).toHaveCount(1, { timeout: 30_000 })
  const editor = page.frameLocator('#viewer iframe').locator('.hwp-editor-host iframe')
  const box = (await editor.boundingBox())!
  await page.mouse.click(box.x + box.width * 0.4, box.y + 300)
  await page.keyboard.type('SEURECOVER')
  await expect(page.locator('#status')).toContainText('(저장 안 함)')
  expect(await page.evaluate(() => window.seuLauncher!.save())).toBeNull()
  const stored = await page.evaluate(() => {
    const saved = window.seuLauncher!.saved!
    return { version: saved.version, bytes: Array.from(saved.bytes) }
  })
  expect(stored.version).toBeNull()
  const edited = textOf(new Uint8Array(stored.bytes))
  expect(edited).toContain('SEURECOVER')
  expect(edited.replace('SEURECOVER', '')).toBe(textOf(input.buffer))
})

for (const name of ['original.hwp', 'original.hwpx']) {
  test(`${name} falls back to viewing without saving`, async ({ page }, info) => {
    await page.goto(`/?collab=${encodeURIComponent(info.testId)}&access=view`)
    await page.setInputFiles('#file', files.get(name)!)

    await expect(page.locator('#status')).toHaveText(name, { timeout: 60_000 })
    const launcher = await page.evaluate(async () => {
      const host = window.seuLauncher!
      return {
        mode: host.mode,
        openError: host.openError,
        saveRejected: (await host.save()) !== null,
        saved: host.saved,
      }
    })
    expect(launcher).toEqual({
      mode: 'view',
      openError: 'collab_unavailable',
      saveRejected: true,
      saved: null,
    })
    await expect(page.locator('#save')).toBeHidden()
  })
}
