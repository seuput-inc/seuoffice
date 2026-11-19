#!/usr/bin/env node
// Decides the real-app tests (gate=e2e). The app ran each scenario (`SeuOffice --e2e <scenario>
// --out <dir>`) and wrote what it saw to <dir>/e2e.jsonl; it decided nothing. This reads those
// records, the files the app saved and exported, and prints one line per check:
//
//   PASS        the evidence line shows what the check asks for
//   FAIL        it shows the opposite, or a step the scenario asks for never ran
//   UNVERIFIED  nothing here bears on it (a reader that does not exist, a tool that did not run)
//
// Layout, per scenario <name> under <root> (the workflow makes it):
//
//   <root>/<name>/scenario.json   the scenario as run, placeholders replaced
//   <root>/<name>/out/e2e.jsonl   one JSON object per step: step, op, ok, ms, value?, error?
//   <root>/<name>/out/*.png       screenshots
//   <root>/<name>/work/           the files the scenario opened, saved and exported
//   <root>/<name>/exit-code.txt   the app's exit status
//
// Exit status: 1 when anything FAILs (a missing step included), 0 otherwise; 2 when only
// UNVERIFIED lines are left and --strict is given.
//
//   node e2e-check.mjs --root <dir> [--scenarios <dir>] [--only a,b] [--json out.json]
//                      [--markdown out.md] [--strict]

import fs from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'
import { decodePng } from './probe-media.mjs'
import { REPO_ROOT, engineText, independentText, pdfPageCount } from './e2e-disk.mjs'

// ------------------------------------------------------------------------------------ the checks

const nfc = (text) => text.normalize('NFC')
const MAX_OPEN_MS = 90000

/** What each scenario must show. The scenario files say what to do; this says what counts. */
const query = (scenario, as, title, expect) => ({ scenario, kind: 'query', as, title, ...expect })
const disk = (scenario, file, title, contains, extra) => ({
  scenario,
  kind: 'disk',
  file,
  title,
  contains,
  ...extra,
})
const exported = (scenario, file, title, contains) => [
  { scenario, kind: 'pdf', file, title: `${title}: a PDF of at least one page`, minPages: 1 },
  { scenario, kind: 'pdfText', file, title: `${title}: the PDF carries the text`, contains },
]
const screenshot = (scenario, name, title) => ({ scenario, kind: 'screenshot', name, title })
const reopen = (scenario, as, contains, title = 'the editor shows the text after reopen') =>
  query(scenario, as, title, { contains })
const typed = (scenario, as, contains) =>
  query(scenario, as, 'the editor shows the typed text before saving', { contains })

export const CHECKS = [
  typed('docs-edit', 'docs-before', 'SEU-E2E-docs'),
  reopen('docs-edit', 'docs-after', 'SEU-E2E-docs'),
  disk('docs-edit', 'docs.docx', 'the saved .docx holds the text', 'SEU-E2E-docs'),
  ...exported('docs-edit', 'docs-export.pdf', 'docs export', 'SEU-E2E-docs'),
  screenshot('docs-edit', 'docs-after', 'the window shows the document'),

  reopen(
    'docs-new',
    'docs-new-after',
    'SEU-E2E-docs-new',
    'a new document, saved to a chosen path, reopens with the text',
  ),
  disk(
    'docs-new',
    'docs-new.docx',
    'the new document was written to the chosen path',
    'SEU-E2E-docs-new',
  ),

  query('docs-ime', 'ime-before', 'Korean 2-Set keys gksrmf compose 한글 in the editor', {
    contains: '한글',
  }),
  reopen('docs-ime', 'ime-after', '한글', 'the composed 한글 is there after save and reopen'),
  disk('docs-ime', 'docs-ime.docx', 'the saved .docx holds 한글', '한글'),

  query('docs-paste', 'paste-text', 'pasted clipboard text lands in the document', {
    contains: 'SEU-E2E-PASTE-TEXT',
  }),
  query('docs-paste', 'paste-html', 'pasted clipboard HTML keeps its bold', {
    path: 'bold',
    contains: 'SEU-E2E-PASTE-HTML',
  }),
  query('docs-paste', 'paste-roundtrip', 'select all, copy and paste back keeps the pasted text', {
    contains: 'SEU-E2E-PASTE-HTML',
  }),

  disk(
    'docs-quit',
    'docs-quit.docx',
    'quitting with a dirty document and answering save writes it',
    'SEU-E2E-quit',
  ),

  typed('sheets-edit', 'sheets-before', 'SEU-E2E-sheets'),
  reopen('sheets-edit', 'sheets-after', 'SEU-E2E-sheets'),
  disk('sheets-edit', 'sheets.xlsx', 'the saved .xlsx holds the text', 'SEU-E2E-sheets'),
  ...exported('sheets-edit', 'sheets-export.pdf', 'sheets export', 'SEU-E2E-sheets'),
  screenshot('sheets-edit', 'sheets-after', 'the window shows the workbook'),

  query('sheets-ime', 'ime-before', 'Korean 2-Set keys gksrmf compose 한글 in a cell', {
    contains: '한글',
  }),
  reopen(
    'sheets-ime',
    'ime-after',
    '한글',
    'the composed 한글 is in the cell after save and reopen',
  ),
  disk('sheets-ime', 'sheets-ime.xlsx', 'the saved .xlsx holds 한글', '한글'),

  typed('sheets-big', 'big-before', 'SEU-E2E-big'),
  reopen(
    'sheets-big',
    'big-after',
    'SEU-E2E-big',
    'the far cell holds the text after save and reopen',
  ),
  disk(
    'sheets-big',
    'big.xlsx',
    'the saved 50000-row .xlsx holds the far cell text',
    'SEU-E2E-big',
    { engine: false },
  ),
  {
    scenario: 'sheets-big',
    kind: 'stepMs',
    op: 'waitReady',
    nth: 1,
    max: MAX_OPEN_MS,
    title: `the 50000-row workbook is ready within ${MAX_OPEN_MS / 1000} s`,
  },
  {
    scenario: 'sheets-big',
    kind: 'stepMs',
    op: 'waitReady',
    nth: 2,
    max: MAX_OPEN_MS,
    title: `it is ready again within ${MAX_OPEN_MS / 1000} s after save and reopen`,
  },

  reopen(
    'slides-edit',
    'slides-after',
    'SEU-E2E-slides',
    'the title shape text editor shows the text after reopen',
  ),
  disk('slides-edit', 'slides.pptx', 'the saved .pptx holds the text', 'SEU-E2E-slides'),
  ...exported('slides-edit', 'slides-export.pdf', 'slides export', 'SEU-E2E-slides'),
  screenshot('slides-edit', 'slides-after', 'the window shows the deck'),

  query('hwp-edit', 'hwp-before', 'the HWP editor finds the typed text', { path: 'count', min: 1 }),
  query('hwp-edit', 'hwp-after', 'the HWP editor finds the text after reopen', {
    path: 'count',
    min: 1,
  }),
  disk('hwp-edit', 'hwp.hwp', 'the saved .hwp body holds the text', 'SEU-E2E-hwp'),
  ...exported('hwp-edit', 'hwp-export.pdf', 'hwp export', 'SEU-E2E-hwp'),
  screenshot('hwp-edit', 'hwp-after', 'the window shows the HWP document'),

  query('hwpx-edit', 'hwp-before', 'the HWP editor finds the typed text', {
    path: 'count',
    min: 1,
  }),
  query('hwpx-edit', 'hwp-after', 'the HWP editor finds the text after reopen', {
    path: 'count',
    min: 1,
  }),
  disk('hwpx-edit', 'hwpx.hwpx', 'the saved .hwpx section holds the text', 'SEU-E2E-hwpx'),

  typed('markdown-edit', 'markdown-before', 'SEU-E2E-markdown'),
  reopen('markdown-edit', 'markdown-after', 'SEU-E2E-markdown'),
  disk('markdown-edit', 'markdown.md', 'the saved .md holds the text', 'SEU-E2E-markdown'),
  ...exported('markdown-edit', 'markdown-export.pdf', 'markdown export', 'SEU-E2E-markdown'),
  screenshot('markdown-edit', 'markdown-after', 'the window shows the note'),

  typed('html-edit', 'html-before', 'SEU-E2E-html'),
  reopen('html-edit', 'html-after', 'SEU-E2E-html'),
  disk('html-edit', 'html.html', 'the saved .html holds the text', 'SEU-E2E-html'),
  ...exported('html-edit', 'html-export.pdf', 'html export', 'SEU-E2E-html'),
  screenshot('html-edit', 'html-after', 'the window shows the page'),

  typed('mindmap-edit', 'mindmap-before', 'SEU-E2E-mindmap'),
  reopen('mindmap-edit', 'mindmap-after', 'SEU-E2E-mindmap'),
  disk('mindmap-edit', 'mindmap.xmind', 'the saved .xmind holds the topic', 'SEU-E2E-mindmap'),

  {
    scenario: 'diagram-edit',
    kind: 'unverified',
    title: 'the diagram editor shows the label',
    reason:
      'the draw.io editor runs in a cross-origin frame (the drawio scheme), so the host page cannot read its label; the saved file is checked instead',
  },
  disk('diagram-edit', 'diagram.drawio', 'the saved .drawio holds the label', 'SEU-E2E-diagram'),
  screenshot('diagram-edit', 'diagram-after', 'the window shows the diagram'),

  query('pdf-view', 'pdf-pages', 'the PDF viewer reports 5 pages', {
    path: 'error.detail.valid_range',
    equals: '1-5',
  }),
  query('pdf-view', 'pdf-goto', 'navigating to page 3 succeeds', { path: 'status', equals: 'ok' }),
  query('pdf-view', 'pdf-current', 'the viewer is on page 3 after navigating', {
    path: 'result.page',
    equals: 3,
  }),
  screenshot('pdf-view', 'pdf-page3', 'the window shows page 3'),

  { scenario: '*', kind: 'coedit', title: 'co-editing in the desktop app' },
]

// ---------------------------------------------------------------------------------------- io

export const realIo = {
  exists: (file) => fs.existsSync(file),
  readText: (file) => fs.readFileSync(file, 'utf8'),
  readBytes: (file) => fs.readFileSync(file),
  size: (file) => fs.statSync(file).size,
  list: (dir) => fs.readdirSync(dir),
  independentText,
  engineText,
  pdfPageCount,
  decodePng,
}

// ------------------------------------------------------------------------------------ loading

export function parseJsonl(text) {
  const records = []
  let unreadable = 0
  for (const line of text.split('\n')) {
    if (!line.trim()) continue
    try {
      records.push(JSON.parse(line))
    } catch {
      unreadable += 1
    }
  }
  return { records, unreadable }
}

/** One scenario's run, or `{ missing: true }` when it left nothing. */
export function loadRun(root, name, io) {
  const dir = path.join(root, name)
  const scenarioFile = path.join(dir, 'scenario.json')
  const logFile = path.join(dir, 'out', 'e2e.jsonl')
  const exitFile = path.join(dir, 'exit-code.txt')
  const run = {
    name,
    dir,
    outDir: path.join(dir, 'out'),
    workDir: path.join(dir, 'work'),
    steps: [],
    records: [],
    unreadable: 0,
    exitCode: undefined,
    missing: !io.exists(dir),
  }
  if (run.missing) return run
  if (io.exists(scenarioFile)) run.steps = JSON.parse(io.readText(scenarioFile)).steps ?? []
  if (io.exists(logFile)) Object.assign(run, parseJsonl(io.readText(logFile)))
  else run.noLog = true
  if (io.exists(exitFile)) {
    const code = Number.parseInt(io.readText(exitFile).trim(), 10)
    if (Number.isFinite(code)) run.exitCode = code
  }
  return run
}

// ------------------------------------------------------------------------------------ results

const pass = (evidence) => ({ status: 'PASS', evidence })
const fail = (evidence) => ({ status: 'FAIL', evidence })
const unverified = (evidence) => ({ status: 'UNVERIFIED', evidence })

const clip = (text, max = 200) => (text.length > max ? `${text.slice(0, max - 3)}...` : text)
const around = (text, needle) => {
  const at = text.indexOf(needle)
  const from = Math.max(0, at - 20)
  return JSON.stringify(text.slice(from, at + needle.length + 20))
}
const get = (value, dotted) =>
  dotted
    .split('.')
    .reduce((node, key) => (node === undefined || node === null ? undefined : node[key]), value)
const asText = (value) =>
  typeof value === 'string' ? value : (JSON.stringify(value) ?? 'undefined')

/** The record of the first step of this op (and name), or why there is none. */
function recordFor(run, match) {
  const index = run.steps.findIndex(match)
  if (index < 0) return { problem: 'the scenario has no such step' }
  const record = run.records[index]
  if (!record)
    return { problem: `step ${index} never ran (the log has ${run.records.length} records)` }
  if (record.op !== run.steps[index].op) {
    return {
      problem: `step ${index} is ${run.steps[index].op} in the scenario but ${record.op} in the log`,
    }
  }
  return { index, record }
}

// ---------------------------------------------------------------------------------- evaluators

function runCheck(run) {
  if (run.missing) return fail('the scenario was not run (no folder)')
  const problems = []
  if (run.noLog) problems.push('no e2e.jsonl')
  const total = run.steps.length
  if (run.records.length < total) {
    problems.push(
      `missing steps ${run.records.length}..${total - 1} (${run.records.length} of ${total} records)`,
    )
  }
  run.records.forEach((record, index) => {
    const step = run.steps[index]
    if (step && record.op !== step.op)
      problems.push(`step ${index} is ${step.op} but the log says ${record.op}`)
    else if (record.ok !== true)
      problems.push(`step ${index} ${record.op} failed: ${record.error ?? 'no error given'}`)
  })
  if (run.unreadable > 0) problems.push(`${run.unreadable} unreadable log lines`)
  if (run.exitCode === undefined) problems.push('no exit status recorded')
  else if (run.exitCode !== 0) {
    const meaning = { 2: 'refused to run', 3: 'a step could not run', 124: 'passed its deadline' }[
      run.exitCode
    ]
    problems.push(`exit status ${run.exitCode}${meaning ? ` (${meaning})` : ''}`)
  }
  if (problems.length > 0) return fail(clip(problems.join('; '), 600))
  const ms = run.records.reduce((sum, record) => sum + (record.ms ?? 0), 0)
  return pass(`${total} steps ran and succeeded in ${ms} ms, exit 0`)
}

function evalQuery(run, check) {
  const found = recordFor(run, (step) => step.op === 'query' && step.as === check.as)
  if (found.problem) return fail(`query ${check.as}: ${found.problem}`)
  const { index, record } = found
  if (record.ok !== true)
    return fail(`query ${check.as} (step ${index}) failed: ${record.error ?? 'no error given'}`)
  const subject = check.path ? get(record.value, check.path) : record.value
  const where = `query ${check.as} (step ${index})${check.path ? ` .${check.path}` : ''}`
  if ('equals' in check) {
    return subject === check.equals
      ? pass(`${where} = ${JSON.stringify(subject)}`)
      : fail(
          `${where} = ${clip(JSON.stringify(subject) ?? 'undefined')}, wanted ${JSON.stringify(check.equals)}`,
        )
  }
  if ('min' in check) {
    return typeof subject === 'number' && subject >= check.min
      ? pass(`${where} = ${subject}`)
      : fail(
          `${where} = ${clip(JSON.stringify(subject) ?? 'undefined')}, wanted at least ${check.min}`,
        )
  }
  const text = nfc(asText(subject))
  const needle = nfc(check.contains)
  return text.includes(needle)
    ? pass(`${where} contains ${around(text, needle)}`)
    : fail(`${where} lacks ${JSON.stringify(check.contains)}: ${clip(text)}`)
}

async function evalDisk(run, check, io) {
  const file = path.join(run.workDir, check.file)
  if (!io.exists(file)) return fail(`${check.file} is missing from the work folder`)
  const needle = nfc(check.contains)
  const size = io.size(file)
  let text
  try {
    text = nfc(await io.independentText(file))
  } catch (error) {
    return fail(`${check.file} (${size} bytes) cannot be read back: ${error.message}`)
  }
  if (!text.includes(needle)) {
    return fail(
      `${check.file} (${size} bytes) lacks ${JSON.stringify(check.contains)} in an independent read of its ${text.length} characters`,
    )
  }
  let engine = 'repo parser skipped'
  if (check.engine !== false) {
    const parsed = await io.engineText(file)
    if (parsed.ok && nfc(parsed.text).includes(needle)) engine = 'repo parser found it too'
    else if (parsed.ok) {
      return fail(
        `${check.file}: the zip/OLE reader found ${JSON.stringify(check.contains)} but the repo parser's text lacks it`,
      )
    } else if (parsed.unavailable) engine = `repo parser did not run (${clip(parsed.error, 120)})`
    else {
      return fail(
        `${check.file}: the zip/OLE reader found the text but the repo parser refuses the file: ${clip(parsed.error ?? '', 160)}`,
      )
    }
  }
  return pass(
    `${check.file} (${size} bytes): independent read found ${around(text, needle)}; ${engine}`,
  )
}

async function evalPdf(run, check, io) {
  const file = path.join(run.workDir, check.file)
  if (!io.exists(file)) return fail(`${check.file} was not written`)
  const size = io.size(file)
  let pages
  try {
    pages = await io.pdfPageCount(file)
  } catch (error) {
    return fail(`${check.file} (${size} bytes) is not a readable PDF: ${error.message}`)
  }
  return pages >= check.minPages
    ? pass(`${check.file}: ${size} bytes, ${pages} page(s)`)
    : fail(`${check.file}: ${pages} page(s), wanted at least ${check.minPages}`)
}

async function evalPdfText(run, check, io) {
  const file = path.join(run.workDir, check.file)
  if (!io.exists(file)) return fail(`${check.file} was not written`)
  const parsed = await io.engineText(file)
  if (!parsed.ok)
    return unverified(
      `no text could be read from ${check.file}: ${clip(parsed.error ?? 'unknown', 160)}`,
    )
  const text = nfc(parsed.text)
  const needle = nfc(check.contains)
  if (text.includes(needle)) return pass(`${check.file} text contains ${around(text, needle)}`)
  // A printed page wraps a long line where it likes, also inside the text (a slide title
  // breaks after a hyphen): the text is on the page, across two lines. Only line breaks are
  // taken out; a blank inside a line still makes it another text.
  const unwrapped = text.replace(/[ \t]*\r?\n[ \t]*/g, '')
  if (unwrapped.includes(needle))
    return pass(
      `${check.file} text contains ${around(unwrapped, needle)}, wrapped over a line break on the page`,
    )
  return text.trim() === ''
    ? unverified(
        `${check.file} has no extractable text (fonts without a text map, or an image-only print)`,
      )
    : fail(
        `${check.file} text lacks ${JSON.stringify(check.contains)}: ${clip(text.replace(/\s+/g, ' '))}`,
      )
}

function evalScreenshot(run, check, io) {
  const file = path.join(run.outDir, `${check.name}.png`)
  if (!io.exists(file)) return fail(`${check.name}.png was not written`)
  let image
  try {
    image = io.decodePng(io.readBytes(file))
  } catch (error) {
    return fail(`${check.name}.png cannot be decoded: ${error.message}`)
  }
  const colors = new Set()
  const stepX = Math.max(1, Math.floor(image.width / 48))
  const stepY = Math.max(1, Math.floor(image.height / 48))
  for (let y = 0; y < image.height; y += stepY) {
    for (let x = 0; x < image.width; x += stepX) colors.add(image.pixel(x, y).join(','))
  }
  const evidence = `${check.name}.png ${image.width}x${image.height}, ${colors.size} distinct sampled colors`
  if (image.width < 200 || image.height < 200) return fail(`${evidence}: too small to be a window`)
  return colors.size >= 8 ? pass(evidence) : fail(`${evidence}: the window is blank`)
}

function evalStepMs(run, check) {
  const indexes = run.steps
    .map((step, index) => (step.op === check.op ? index : -1))
    .filter((index) => index >= 0)
  const index = indexes[check.nth - 1]
  if (index === undefined) return fail(`the scenario has no ${check.op} number ${check.nth}`)
  const record = run.records[index]
  if (!record) return fail(`${check.op} number ${check.nth} (step ${index}) never ran`)
  if (record.ok !== true)
    return fail(
      `${check.op} number ${check.nth} (step ${index}) failed: ${record.error ?? 'no error given'}`,
    )
  return record.ms <= check.max
    ? pass(
        `${check.op} number ${check.nth} (step ${index}) took ${record.ms} ms, bound ${check.max} ms`,
      )
    : fail(
        `${check.op} number ${check.nth} (step ${index}) took ${record.ms} ms, bound ${check.max} ms`,
      )
}

// ---------------------------------------------------------------------------------- co-editing

const SOURCE_FILE = /\.(?:ts|tsx|mjs|js|rs)$/

function* walk(dir) {
  let entries
  try {
    entries = fs.readdirSync(dir, { withFileTypes: true })
  } catch {
    return
  }
  for (const entry of entries) {
    if (entry.name === 'node_modules' || entry.name === 'target' || entry.name === 'out') continue
    const full = path.join(dir, entry.name)
    if (entry.isDirectory()) yield* walk(full)
    else if (SOURCE_FILE.test(entry.name)) yield full
  }
}

/**
 * Whether the desktop app co-edits, read from the sources: the editors reach a session only
 * through `window.seuCollab`, which one web-platform module installs. A desktop source that
 * mentions it (or the collab package) means a desktop scenario is owed.
 */
export function coeditCheck(repoRoot) {
  const definition = path.join(repoRoot, 'packages/web-platform/src/collab-page.ts')
  const definesIt =
    fs.existsSync(definition) && fs.readFileSync(definition, 'utf8').includes('window.seuCollab =')
  const desktopRoots = [
    'apps/shell/src',
    'crates/shell/src',
    'crates/shell/bridge',
    ...(fs.existsSync(path.join(repoRoot, 'apps'))
      ? fs
          .readdirSync(path.join(repoRoot, 'apps'))
          .flatMap((app) => [`apps/${app}/src/main`, `apps/${app}/src/preload`])
      : []),
  ]
  let scanned = 0
  const hits = []
  for (const rel of desktopRoots) {
    for (const file of walk(path.join(repoRoot, rel))) {
      scanned += 1
      if (/seuCollab|@seuoffice\/collab/.test(fs.readFileSync(file, 'utf8')))
        hits.push(path.relative(repoRoot, file))
    }
  }
  const count = (dir, pattern) => {
    try {
      return fs.readdirSync(path.join(repoRoot, dir)).filter((name) => pattern.test(name))
    } catch {
      return []
    }
  }
  const webSpecs = count('apps/web/tests', /^collab.*\.spec\.ts$/)
  const packageTests = count('packages/collab/tests', /\.test\.ts$/)
  if (hits.length > 0) {
    return fail(
      `desktop sources reference co-editing (${hits.slice(0, 3).join(', ')}) but no desktop scenario covers it`,
    )
  }
  if (!definesIt) {
    return unverified(
      'packages/web-platform/src/collab-page.ts does not install window.seuCollab here, so the claim that co-editing is web-only could not be established',
    )
  }
  return unverified(
    `no desktop scenario: co-editing is web-only. window.seuCollab is installed only by packages/web-platform/src/collab-page.ts and none of ${scanned} desktop sources (apps/shell, crates/shell, every app's main and preload) mentions it. The web tests are the evidence: apps/web/tests/collab-*.spec.ts (${webSpecs.length} files), packages/collab/tests (${packageTests.length} files), packages/embed/tests/collab*.test.ts and packages/web-platform/tests/collab-*.test.ts, run by the web job of .github/workflows/seuoffice.yml`,
  )
}

// ------------------------------------------------------------------------------------ the run

/** Runs every check of the named scenarios; resolves rows `{ scenario, id, title, status, evidence }`. */
export async function runChecks({
  root,
  scenarios,
  io = realIo,
  repoRoot = REPO_ROOT,
  checks = CHECKS,
}) {
  const rows = []
  const add = (scenario, id, title, result) => rows.push({ scenario, id, title, ...result })
  for (const name of scenarios) {
    const run = loadRun(root, name, io)
    add(name, `${name}/run`, 'every step ran and succeeded', runCheck(run))
    for (const check of checks.filter((candidate) => candidate.scenario === name)) {
      const result = run.missing
        ? fail('the scenario was not run')
        : check.kind === 'unverified'
          ? unverified(check.reason)
          : await evaluate(run, check, io)
      add(name, checkId(check), check.title, result)
    }
  }
  for (const check of checks.filter((candidate) => candidate.kind === 'coedit')) {
    add('coedit', 'coedit', check.title, coeditCheck(repoRoot))
  }
  return rows
}

function checkId(check) {
  const subject =
    check.as ?? check.file ?? check.name ?? (check.op ? `${check.op}${check.nth}` : '')
  return `${check.scenario}/${check.kind}${subject ? `:${subject}` : ''}`
}

async function evaluate(run, check, io) {
  switch (check.kind) {
    case 'query':
      return evalQuery(run, check)
    case 'disk':
      return evalDisk(run, check, io)
    case 'pdf':
      return evalPdf(run, check, io)
    case 'pdfText':
      return evalPdfText(run, check, io)
    case 'screenshot':
      return evalScreenshot(run, check, io)
    case 'stepMs':
      return evalStepMs(run, check)
    default:
      return fail(`unknown check kind ${check.kind}`)
  }
}

export const formatRow = (row) =>
  `${row.status.padEnd(10)} ${row.id.padEnd(40)} ${row.title} | ${row.evidence}`

export function exitCodeOf(rows, strict = false) {
  if (rows.some((row) => row.status === 'FAIL')) return 1
  return strict && rows.some((row) => row.status === 'UNVERIFIED') ? 2 : 0
}

export function summary(rows) {
  const count = (status) => rows.filter((row) => row.status === status).length
  return `${count('PASS')} PASS, ${count('FAIL')} FAIL, ${count('UNVERIFIED')} UNVERIFIED of ${rows.length} checks`
}

export function markdown(rows) {
  const cell = (text) => text.replaceAll('|', '\\|').replaceAll('\n', ' ')
  return [
    `${summary(rows)}`,
    '',
    '| status | check | what | evidence |',
    '| --- | --- | --- | --- |',
    ...rows.map(
      (row) => `| ${row.status} | ${cell(row.id)} | ${cell(row.title)} | ${cell(row.evidence)} |`,
    ),
  ].join('\n')
}

// -------------------------------------------------------------------------------------- main

async function main(argv) {
  const option = (name) => {
    const at = argv.indexOf(name)
    return at >= 0 ? argv[at + 1] : undefined
  }
  const root = option('--root')
  if (!root) {
    console.error(
      'usage: e2e-check.mjs --root <dir> [--scenarios <dir>] [--only a,b] [--json out.json] [--markdown out.md] [--strict]',
    )
    return 64
  }
  const scenariosDir =
    option('--scenarios') ??
    path.join(path.dirname(fileURLToPath(import.meta.url)), '../e2e/scenarios')
  const only = option('--only')
    ?.split(',')
    .map((name) => name.trim())
    .filter(Boolean)
  const all = fs
    .readdirSync(scenariosDir)
    .filter((file) => file.endsWith('.json'))
    .map((file) => file.slice(0, -5))
    .sort()
  const scenarios = only && !only.includes('all') ? all.filter((name) => only.includes(name)) : all
  const rows = await runChecks({ root, scenarios })
  for (const row of rows) console.log(formatRow(row))
  console.log(summary(rows))
  if (option('--json')) fs.writeFileSync(option('--json'), `${JSON.stringify(rows, null, 2)}\n`)
  if (option('--markdown')) fs.writeFileSync(option('--markdown'), `${markdown(rows)}\n`)
  return exitCodeOf(rows, argv.includes('--strict'))
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  process.exitCode = await main(process.argv.slice(2))
}
