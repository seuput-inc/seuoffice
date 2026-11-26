// The view-mode benchmark of large workbooks: a host page on one site fetches
// each synthetic workbook of bench-xlsx-fixtures.ts and opens it through
// @seuoffice/embed from apps/web/dist served on another site, as a production
// host does (view mode, a fresh browser context each round, so every round is a
// first visit). It reports what the host waits on: the time from the click to
// drawn cells, frames while the wheel scrolls for 3 s, main-thread stalls after
// the first paint, the growth of the browser's document processes' memory (also less what a
// cross-origin iframe with no app in it grows them by, the control), and the bytes the page
// fetched (brotli sizes of the files served).
// Runs only under SEUOFFICE_BENCH_XLSX=<label>, against a built apps/web/dist:
//   SEUOFFICE_BENCH_XLSX=main SEUOFFICE_BENCH_XLSX_SIZES=large,xl \
//     npx playwright test --config apps/web/playwright.config.ts bench-xlsx
// The results land in apps/web/test-results/bench-xlsx-<label>-<browser>.json
// with a screenshot of the first cells and of the last row of each workbook,
// which tools/bench-xlsx-compare.mjs compares between two labels.
// SEUOFFICE_BENCH_XLSX_SLOW_ITALIC_MS=<ms> holds back the italic Carlito faces
// that long, as a slow network would, for the italic face check.
import { execFile, execFileSync } from 'node:child_process'
import { existsSync, mkdirSync, readFileSync, statSync, writeFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { promisify } from 'node:util'
import { brotliCompressSync, constants } from 'node:zlib'
import { chromium, expect, test, type Browser, type Frame, type Page } from '@playwright/test'
import {
  BUNDLE_HOST,
  buildEmbedScript,
  HOST_NAME,
  origin,
  serveBundle,
  serveHost,
  type HostWindow,
  type Site,
} from './cross-origin-embed'
import { benchFixture, FIXTURE_ROWS, type FixtureName } from './bench-xlsx-fixtures'

const label = process.env.SEUOFFICE_BENCH_XLSX
const ROUNDS = Number(process.env.SEUOFFICE_BENCH_XLSX_ROUNDS ?? 3)
const SIZES = (
  process.env.SEUOFFICE_BENCH_XLSX_SIZES ??
  'small,medium,large,styled10,xl,italic,italicruns,italicfrozen,rules'
).split(',') as FixtureName[]
const SLOW_ITALIC_MS = Number(process.env.SEUOFFICE_BENCH_XLSX_SLOW_ITALIC_MS ?? 0)
const SCROLL_MS = 3_000
// the host's own measure (YUYU, 2026-11-26): one 120 px wheel step, 16 ms pause, for 3 s, at
// 1440x900 and scale 1; the Playwright round trip makes it about one step per 50 ms
const WHEEL_PX = 120
const WHEEL_PAUSE_MS = 16
const VIEWPORT = { width: 1440, height: 900 }
const repo = fileURLToPath(new URL('../../..', import.meta.url))
const dist = resolve(repo, 'apps/web/dist')
const out = resolve(repo, 'apps/web/test-results')
// outside test-results, which playwright empties on every run: the xl workbook takes seconds to write
const fixtures = resolve(repo, 'node_modules/.cache/bench-xlsx')

interface Round {
  /** handing over the file to the first drawn cell text */
  firstCellsMs: number
  scrollFps: number
  scrollMaxGapMs: number
  /** wheel steps sent, and those the sheets frame received while the scroll lasted */
  wheels: { sent: number; received: number }
  /** frame gaps over 200 ms in the sheets frame from the first cells to the end of the scroll */
  stallsOver200ms: number[]
  /** the long tasks the browser itself reports (Chromium only) over the same span */
  longTasksOver200ms: number[] | null
  memoryGrowthMb: number
  /** the host's measure (peakProcessSampler), from 400 ms before the click to 1 s after the scroll */
  processPeakMb: number
  /** the same over macOS phys_footprint, compressed memory included */
  processPeakFootprintMb: number
  transfer: { files: number; rawMb: number; brotliMb: number; repeated: string[] }
}

/** What the init script records in each window; every time is epoch ms */
interface Recorder {
  /** the sheets frame: when the grid first drew cell text */
  firstCell: number | null
  /** the sheets frame: the highest row number in the cell text it drew ("항목 <row>-<column>") */
  lastRow: number
  /** the sheets frame: cell text drawn italic in a family with a bundled italic face */
  italicDraws: number
  /** those draws made while that face was not loaded (a substitute was drawn), and when */
  italicUnloaded: { font: string; at: number }[]
  frames: number[]
  wheels: number[]
  longTasks: { start: number; duration: number }[]
}

declare global {
  interface Window {
    benchXlsx?: Recorder
  }
}

// rAF timestamps and long tasks of each window; the sheets frame also notes wheel events and when
// its grid first draws cell text. Every fixture's first column holds
// "항목 ..." text, which no header, toolbar or status text has (the canvas cannot be read back
// instead: WebKit taints it).
function record(): void {
  const epoch = (time: number): number => performance.timeOrigin + time
  const recorder: Recorder = {
    firstCell: null,
    lastRow: 0,
    italicDraws: 0,
    italicUnloaded: [],
    frames: [],
    wheels: [],
    longTasks: [],
  }
  window.benchXlsx = recorder
  if (window !== window.top) {
    addEventListener('wheel', () => recorder.wheels.push(epoch(performance.now())), {
      capture: true,
      passive: true,
    })
    const fillText = CanvasRenderingContext2D.prototype.fillText
    CanvasRenderingContext2D.prototype.fillText = function (
      this: CanvasRenderingContext2D,
      ...args: Parameters<CanvasRenderingContext2D['fillText']>
    ) {
      const row = /^항목 (\d+)-/.exec(String(args[0]))
      if (row) {
        recorder.firstCell ??= epoch(performance.now())
        recorder.lastRow = Math.max(recorder.lastRow, Number(row[1]))
      }
      // any text, since a rich-text run is drawn in pieces; only text the bundled faces cover
      // (Hangul in an italic run draws with a fallback face either way)
      const font = this.font
      if (/\bitalic\b/.test(font) && /[\p{Script=Latin}\d]/u.test(String(args[0]))) {
        const unquote = (name: string): string => name.trim().replace(/^["']|["']$/g, '')
        const family = unquote(/\dpx(?:\/\S+)?\s+([^,]*)/.exec(font)?.[1] ?? '')
        const weights = /\b(bold|[6-9]00)\b/.test(font) ? ['bold', '700'] : ['normal', '400']
        const faces = [...document.fonts].filter(
          (face) =>
            unquote(face.family) === family &&
            face.style === 'italic' &&
            weights.includes(face.weight),
        )
        if (faces.length > 0) {
          recorder.italicDraws++
          if (!faces.some((face) => face.status === 'loaded')) {
            recorder.italicUnloaded.push({ font, at: epoch(performance.now()) })
          }
        }
      }
      return fillText.apply(this, args)
    }
  }
  const tick = (time: number): void => {
    recorder.frames.push(epoch(time))
    requestAnimationFrame(tick)
  }
  requestAnimationFrame(tick)
  try {
    new PerformanceObserver((list) => {
      for (const entry of list.getEntries()) {
        recorder.longTasks.push({ start: epoch(entry.startTime), duration: entry.duration })
      }
    }).observe({ type: 'longtask', buffered: true })
  } catch {
    // WebKit has no long task entries: the frame gaps stand in for them
  }
}

async function sheetsFrame(page: Page, bundle: string): Promise<Frame> {
  for (let i = 0; i < 2_400; i++) {
    const frame = page.frames().find((f) => f.url().startsWith(`${bundle}/sheets/`))
    if (frame) return frame
    await page.waitForTimeout(50)
  }
  throw new Error('the Sheets frame never appeared')
}

interface ProcessRow {
  pid: number
  ppid: number
  rss: number
  command: string
}

function processes(): ProcessRow[] {
  return execFileSync('ps', ['-A', '-o', 'pid=,ppid=,rss=,command='], {
    encoding: 'utf8',
    maxBuffer: 64 * 1024 * 1024,
  })
    .split('\n')
    .map((line) => /^\s*(\d+)\s+(\d+)\s+(\d+)\s+(.*)$/.exec(line))
    .filter((match) => match !== null)
    .map(([, pid, ppid, rss, command]) => ({
      pid: Number(pid),
      ppid: Number(ppid),
      rss: Number(rss),
      command: command!,
    }))
}

// Playwright's WebKit content processes are XPC services: launchd is their parent, not the browser
const WEBKIT_CONTENT = /ms-playwright\/webkit-[^/]+\/com\.apple\.WebKit\.WebContent/

/** the WebKit content processes running before a round starts: not the round's */
const webkitContentBefore = (): Set<number> =>
  new Set(
    processes()
      .filter((row) => WEBKIT_CONTENT.test(row.command))
      .map((row) => row.pid),
  )

/** RSS in MB of each document process of this test worker's browser: Chromium renderers, WebKit content */
function documentProcesses(notMine: ReadonlySet<number>): Map<number, number> {
  const rows = processes()
  const mine = new Set([process.pid])
  for (let grew = true; grew;) {
    grew = false
    for (const row of rows) {
      if (!mine.has(row.pid) && mine.has(row.ppid)) {
        mine.add(row.pid)
        grew = true
      }
    }
  }
  return new Map(
    rows
      .filter(
        (row) =>
          (mine.has(row.pid) && /--type=renderer/.test(row.command)) ||
          (WEBKIT_CONTENT.test(row.command) && !notMine.has(row.pid)),
      )
      .map((row) => [row.pid, row.rss / 1024]),
  )
}

function documentProcessesMb(notMine: ReadonlySet<number>): number {
  return [...documentProcesses(notMine).values()].reduce((sum, rss) => sum + rss, 0)
}

/** The top call that reads macOS phys_footprint (MEM: resident and compressed memory together) */
const footprintArgs = (pids: readonly number[]): string[] => [
  '-l',
  '1',
  '-stats',
  'pid,mem',
  ...pids.flatMap((pid) => ['-pid', String(pid)]),
]

/** phys_footprint in MB by pid, from what that top call printed */
function parseFootprints(text: string): Map<number, number> {
  const unit: Record<string, number> = { B: 1 / 1024 / 1024, K: 1 / 1024, M: 1, G: 1024 }
  const out = new Map<number, number>()
  for (const match of text.matchAll(/^(\d+)\s+([\d.]+)([BKMG])[+-]?\s*$/gm)) {
    out.set(Number(match[1]), Number(match[2]) * unit[match[3]!]!)
  }
  return out
}

/**
 * The host's measure (YUYU, 2026-11-27): each document process's peak RSS, sampled every 150 ms,
 * less its RSS before the click when it ran then; a process born after the click (the
 * cross-origin SeuOffice renderer) counts whole. The largest of them. The same rule over macOS
 * phys_footprint, sampled every 0.5 s: RSS leaves out memory the system compressed.
 */
function peakProcessSampler(
  notMine: ReadonlySet<number>,
): () => Promise<{ rssMb: number; footprintMb: number }> {
  const before = documentProcesses(notMine)
  const footprintBefore =
    before.size === 0
      ? new Map<number, number>()
      : parseFootprints(
          execFileSync('/usr/bin/top', footprintArgs([...before.keys()]), { encoding: 'utf8' }),
        )
  const peaks = new Map<number, number>()
  const footprintPeaks = new Map<number, number>()
  const sample = (): void => {
    for (const [pid, rss] of documentProcesses(notMine)) {
      peaks.set(pid, Math.max(peaks.get(pid) ?? 0, rss))
    }
  }
  const timer = setInterval(sample, 150)
  let stopped = false
  const run = promisify(execFile)
  const sampleFootprints = async (): Promise<void> => {
    if (peaks.size === 0) return
    const { stdout } = await run('/usr/bin/top', footprintArgs([...peaks.keys()]))
    for (const [pid, mb] of parseFootprints(stdout)) {
      footprintPeaks.set(pid, Math.max(footprintPeaks.get(pid) ?? 0, mb))
    }
  }
  const footprintLoop = (async () => {
    while (!stopped) {
      await sampleFootprints().catch(() => {})
      await new Promise((resolve) => setTimeout(resolve, 500))
    }
  })()
  const largest = (peakOf: Map<number, number>, baseOf: Map<number, number>): number =>
    Math.max(0, ...[...peakOf].map(([pid, peak]) => peak - (baseOf.get(pid) ?? 0)))
  return async () => {
    if (!stopped) {
      stopped = true
      clearInterval(timer)
      sample()
      await footprintLoop
      await sampleFootprints().catch(() => {})
    }
    return {
      rssMb: largest(peaks, before),
      footprintMb: largest(footprintPeaks, footprintBefore),
    }
  }
}

const brotliSizes = new Map<string, number>()
function brotliSize(path: string): number {
  const known = brotliSizes.get(path)
  if (known !== undefined) return known
  const size = brotliCompressSync(readFileSync(path), {
    params: { [constants.BROTLI_PARAM_QUALITY]: 9 },
  }).length
  brotliSizes.set(path, size)
  return size
}

/**
 * The paths the page fetched from the bundle's site with a body: a revalidated file (304) moved
 * no bytes. Each entry settles once its sizes are known.
 */
function trackFetches(page: Page, bundle: string): Promise<string | null>[] {
  const fetched: Promise<string | null>[] = []
  page.context().on('requestfinished', (request) => {
    const url = new URL(request.url())
    if (url.origin !== bundle || request.method() !== 'GET') return
    fetched.push(
      request.sizes().then((sizes) => (sizes.responseBodySize > 0 ? url.pathname : null)),
    )
  })
  return fetched
}

async function transfer(fetched: Promise<string | null>[]): Promise<Round['transfer']> {
  const urls = (await Promise.all(fetched)).filter((path) => path !== null)
  let raw = 0
  let brotli = 0
  const counts = new Map<string, number>()
  for (const path of urls) {
    counts.set(path, (counts.get(path) ?? 0) + 1)
    const file = resolve(
      dist,
      `.${decodeURIComponent(path)}${path.endsWith('/') ? 'index.html' : ''}`,
    )
    if (!existsSync(file)) continue
    raw += statSync(file).size
    brotli += brotliSize(file)
  }
  return {
    files: urls.length,
    rawMb: raw / 1e6,
    brotliMb: brotli / 1e6,
    repeated: [...counts].filter(([, n]) => n > 1).map(([path, n]) => `${path} x${n}`),
  }
}

async function oneRound(
  browser: Browser,
  sites: { host: string; bundle: string },
  name: FixtureName,
  shots: string | null,
): Promise<Round> {
  const notMine = webkitContentBefore()
  const context = await browser.newContext({ viewport: VIEWPORT, deviceScaleFactor: 1 })
  try {
    await context.addInitScript(record)
    if (SLOW_ITALIC_MS > 0) {
      await context.route(/Carlito-(Bold)?Italic[^/]*\.ttf/, async (route) => {
        await new Promise((resolve) => setTimeout(resolve, SLOW_ITALIC_MS))
        await route.continue()
      })
    }
    const page = await context.newPage()
    const errors: string[] = []
    page.on('pageerror', (error) => errors.push(error.message))
    const urls = trackFetches(page, sites.bundle)
    await page.goto(`${sites.host}/host.html`)
    await page.waitForTimeout(400)
    const processPeak = peakProcessSampler(notMine)
    const memoryBefore = documentProcessesMb(notMine)
    let memoryPeak = memoryBefore
    const sampler = setInterval(() => {
      memoryPeak = Math.max(memoryPeak, documentProcessesMb(notMine))
    }, 250)
    try {
      // the host fetches the file and hands it over, as a document list's click does
      const start = await page.evaluate(
        async ({ baseUrl, fileName }) => {
          const clicked = performance.timeOrigin + performance.now()
          const bytes = new Uint8Array(await (await fetch(`/${fileName}`)).arrayBuffer())
          const host = window as unknown as HostWindow
          host.embedded = host.SeuofficeEmbed.openDocument(document.getElementById('viewer')!, {
            baseUrl,
            bytes,
            fileName,
          })
          return clicked
        },
        { baseUrl: `${sites.bundle}/`, fileName: `${name}.xlsx` },
      )
      const frame = await sheetsFrame(page, sites.bundle)
      const firstCell = await frame.waitForFunction(() => window.benchXlsx?.firstCell, undefined, {
        timeout: 180_000,
        polling: 50,
      })
      const cellsAt = (await firstCell.jsonValue()) as number
      // what the page still does after its first cells (the host's thumbnail request and so on)
      await page.waitForTimeout(5_000)
      if (shots) {
        await page.locator('#viewer iframe').screenshot({ path: `${shots}-top.png` })
      }
      const box = (await page.locator('#viewer iframe').boundingBox())!
      await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2)
      await page.waitForTimeout(300)
      const now = (): Promise<number> =>
        frame.evaluate(() => performance.timeOrigin + performance.now())
      const scrollStart = await now()
      let sent = 0
      for (const until = Date.now() + SCROLL_MS; Date.now() < until; sent++) {
        await page.mouse.wheel(0, WHEEL_PX)
        await page.waitForTimeout(WHEEL_PAUSE_MS)
      }
      const scrollEnd = await now()
      await page.waitForTimeout(1_000)
      const processPeaks = await processPeak()
      const recorded = await frame.evaluate(() => window.benchXlsx!)
      const gaps = (from: number, to: number): number[] => {
        const frames = recorded.frames.filter((t) => t >= from && t <= to)
        return frames.slice(1).map((t, i) => t - frames[i]!)
      }
      const scrollGaps = gaps(scrollStart, scrollEnd)
      if (shots) {
        // the last row, reached the same way in every browser (WebKit takes no Ctrl+End there):
        // one wheel step past the end, then wait until its text is drawn
        await page.mouse.wheel(0, 50_000_000)
        await frame.waitForFunction(
          (rows) => window.benchXlsx!.lastRow === rows,
          FIXTURE_ROWS[name],
          {
            timeout: 120_000,
            polling: 100,
          },
        )
        await page.waitForTimeout(1_000)
        await page.locator('#viewer iframe').screenshot({ path: `${shots}-end.png` })
      }
      expect(errors, `${name}: page errors`).toEqual([])
      // italic cells draw with their bundled italic face from the first frame on, never a
      // substitute. WebKit only up to the scroll: during it WebKit now and then reports the bold
      // italic face unloaded again for one draw, with every face loaded at start too.
      const webkit = browser.browserType().name() === 'webkit'
      expect(
        recorded.italicUnloaded
          .filter((draw) => !webkit || draw.at < scrollStart)
          .map((draw) => draw.font),
        `${name}: italic drawn before its face loaded`,
      ).toEqual([])
      if (name.startsWith('italic')) expect(recorded.italicDraws, 'italic draws').toBeGreaterThan(0)
      return {
        firstCellsMs: cellsAt - start,
        scrollFps: scrollGaps.length / ((scrollEnd - scrollStart) / 1_000),
        scrollMaxGapMs: Math.max(0, ...scrollGaps),
        wheels: {
          sent,
          received: recorded.wheels.filter((t) => t >= scrollStart && t <= scrollEnd).length,
        },
        stallsOver200ms: gaps(cellsAt, scrollEnd)
          .filter((gap) => gap > 200)
          .map(Math.round),
        longTasksOver200ms:
          browser.browserType().name() === 'chromium'
            ? recorded.longTasks
                .filter((task) => task.start >= cellsAt && task.duration > 200)
                .map((task) => Math.round(task.duration))
            : null,
        memoryGrowthMb: Math.max(memoryPeak, documentProcessesMb(notMine)) - memoryBefore,
        processPeakMb: processPeaks.rssMb,
        processPeakFootprintMb: processPeaks.footprintMb,
        transfer: await transfer(urls),
      }
    } finally {
      clearInterval(sampler)
      void processPeak()
    }
  } finally {
    await context.close()
  }
}

/**
 * The control: the same host opens a cross-origin iframe of a page with no app in it (the
 * bundle site's bare host page), held as long as a round's open and scroll, measured the same
 * way. Its growth is what any cross-origin iframe costs, whatever it shows.
 */
async function controlRound(
  browser: Browser,
  sites: { host: string; bundle: string },
): Promise<number> {
  const notMine = webkitContentBefore()
  const context = await browser.newContext({ viewport: VIEWPORT, deviceScaleFactor: 1 })
  try {
    const page = await context.newPage()
    await page.goto(`${sites.host}/host.html`)
    const memoryBefore = documentProcessesMb(notMine)
    let memoryPeak = memoryBefore
    const sampler = setInterval(() => {
      memoryPeak = Math.max(memoryPeak, documentProcessesMb(notMine))
    }, 250)
    try {
      await page.evaluate((src) => {
        const frame = document.createElement('iframe')
        frame.src = src
        frame.style.cssText = 'width:100%;height:100%;border:0'
        document.getElementById('viewer')!.append(frame)
        return new Promise((loaded) => frame.addEventListener('load', loaded, { once: true }))
      }, `${sites.bundle}/host.html`)
      await page.waitForTimeout(5_000 + 300 + SCROLL_MS)
      return Math.max(memoryPeak, documentProcessesMb(notMine)) - memoryBefore
    } finally {
      clearInterval(sampler)
    }
  } finally {
    await context.close()
  }
}

const median = (values: number[]): number => {
  const sorted = [...values].sort((a, b) => a - b)
  return sorted[Math.floor(sorted.length / 2)]!
}

test.describe('xlsx view benchmark', () => {
  test.skip(!label, 'set SEUOFFICE_BENCH_XLSX to run the benchmark')
  test.setTimeout(60 * 60_000)

  let host: Site | undefined
  let bundle: Site | undefined
  test.beforeAll(async () => {
    await buildEmbedScript()
    const files: Record<string, string> = {}
    for (const name of SIZES) {
      files[`/${name}.xlsx`] = await benchFixture(name, resolve(fixtures, `${name}.xlsx`))
    }
    host = await serveHost(files)
    const origins = JSON.stringify({ origins: [origin(host, HOST_NAME)] })
    bundle = await serveBundle({ status: 200, body: origins })
  })
  test.afterAll(async () => {
    await host?.close()
    await bundle?.close()
  })

  test(`opens and scrolls large workbooks in view mode (${label})`, async ({
    browser: given,
  }, testInfo) => {
    const browserName = testInfo.project.name
    // Chrome puts a cross-site iframe in a process of its own, and so does this Chromium: the
    // one Playwright launches keeps it in the host's process, which leaves that process's own
    // memory out of the growth
    const isolated =
      browserName === 'chromium' ? await chromium.launch({ args: ['--site-per-process'] }) : null
    const browser = isolated ?? given
    try {
      mkdirSync(out, { recursive: true })
      const results: Record<string, { rounds: Round[]; median: Record<string, number> }> = {}
      const sites = { host: origin(host!, HOST_NAME), bundle: origin(bundle!, BUNDLE_HOST) }
      const control: number[] = []
      for (let round = 0; round < ROUNDS; round++) control.push(await controlRound(browser, sites))
      const controlMb = median(control)
      console.log(`[bench-xlsx] ${label} ${browserName} control`, JSON.stringify({ controlMb }))
      for (const name of SIZES) {
        const rounds: Round[] = []
        for (let round = 0; round < ROUNDS; round++) {
          const shots =
            round === 0 ? resolve(out, `bench-xlsx-${label}-${browserName}-${name}`) : null
          rounds.push(await oneRound(browser, sites, name, shots))
        }
        results[name] = {
          rounds,
          median: {
            firstCellsMs: median(rounds.map((r) => r.firstCellsMs)),
            scrollFps: median(rounds.map((r) => r.scrollFps)),
            scrollMaxGapMs: median(rounds.map((r) => r.scrollMaxGapMs)),
            wheelsReceived: median(rounds.map((r) => r.wheels.received)),
            stallsOver200ms: median(rounds.map((r) => r.stallsOver200ms.length)),
            memoryGrowthMb: median(rounds.map((r) => r.memoryGrowthMb)),
            memoryGrowthOverControlMb: median(rounds.map((r) => r.memoryGrowthMb)) - controlMb,
            processPeakMb: median(rounds.map((r) => r.processPeakMb)),
            processPeakFootprintMb: median(rounds.map((r) => r.processPeakFootprintMb)),
            brotliMb: median(rounds.map((r) => r.transfer.brotliMb)),
          },
        }
        console.log(
          `[bench-xlsx] ${label} ${browserName} ${name}`,
          JSON.stringify(results[name].median),
        )
        // after each workbook: a later one that fails keeps the earlier results
        writeFileSync(
          resolve(out, `bench-xlsx-${label}-${browserName}.json`),
          JSON.stringify(
            {
              label,
              browser: browserName,
              wheel: { px: WHEEL_PX, pauseMs: WHEEL_PAUSE_MS },
              control: { rounds: control, medianMb: controlMb },
              results,
            },
            null,
            1,
          ) + '\n',
        )
      }
    } finally {
      await isolated?.close()
    }
  })
})
