import { join } from 'node:path'
import { expect, test, type Frame } from '@playwright/test'
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
 * Safari on an iPhone enlarges the text of a block wider than the screen unless the page asks for
 * `-webkit-text-size-adjust: 100%`. A document is laid out at its paper width and scaled to the
 * frame, so enlarged text no longer fits the lines it was measured for: a docx opened for viewing
 * showed its body text three times too large, the lines on top of each other.
 *
 * Only that Safari enlarges, and no runner of this suite has it. What can be checked here is what
 * stops it: every app's page asks for 100%, for viewing and for editing, and asks in the prefixed
 * spelling, the only one Safari knows. Chromium applies either spelling; desktop WebKit knows
 * neither, so this spec stays out of the webkit project.
 */
const FORMATS: { name: string; file: string; page: string }[] = [
  { name: 'docx', file: 'fixtures/generated/simple.docx', page: '/docs/index.html' },
  {
    name: 'xlsx',
    file: 'apps/sheets/fixtures/generated/compatibility-edit.xlsx',
    page: '/sheets/index.html',
  },
  {
    name: 'pptx',
    file: 'packages/pptx-engine/tests/fixtures/01_standard_business.pptx',
    page: '/slides/index.html',
  },
  { name: 'pdf', file: 'apps/pdf/tests/fixtures/plain-sample.pdf', page: '/pdf/index.html' },
  { name: 'hwp', file: 'apps/hwp/tests/fixtures/memo.hwp', page: '/hwp/index.html' },
]

/** What the root of the page asks for, and whether a stylesheet of the page says it Safari's way */
function textSizeAdjust(frame: Frame): Promise<{ applied: string; prefixed: boolean }> {
  return frame.evaluate(async () => {
    const inline = [...document.querySelectorAll('style')].map((style) => style.textContent ?? '')
    const linked = await Promise.all(
      [...document.querySelectorAll<HTMLLinkElement>('link[rel="stylesheet"]')].map((link) =>
        fetch(link.href).then((response) => response.text()),
      ),
    )
    return {
      applied: getComputedStyle(document.documentElement).getPropertyValue('text-size-adjust'),
      prefixed: [...inline, ...linked].some((css) => /-webkit-text-size-adjust:\s*100%/.test(css)),
    }
  })
}

test.beforeAll(buildEmbedScript)

let host: Site
let bundle: Site
test.beforeEach(async () => ({ host, bundle } = await serveHostAndBundle()))
test.afterEach(() => Promise.all([host.close(), bundle.close()]))

for (const format of FORMATS) {
  for (const mode of ['view', 'edit'] as const) {
    test(`${format.name} for ${mode}: the page keeps its text at the size it laid out`, async ({
      page,
    }) => {
      test.setTimeout(120_000)
      await page.setViewportSize({ width: 390, height: 844 })
      const bundleOrigin = origin(bundle, BUNDLE_HOST)
      await embed(page, origin(host, HOST_NAME), `${bundleOrigin}/`, {
        file: join(repo, format.file),
        mode,
      })
      await expectOpened(page)

      const frame = page
        .frames()
        .find((frame) => frame.url().startsWith(bundleOrigin + format.page))
      expect(frame, `the frame of ${format.page}`).toBeDefined()
      expect(await textSizeAdjust(frame!)).toEqual({ applied: '100%', prefixed: true })
    })
  }
}
