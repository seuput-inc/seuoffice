#!/usr/bin/env node
// Runs the native-host specs (specs.mjs) on the real app, on a GitHub-hosted runner only (the
// workflow refuses anything else, and the app refuses a profile without its e2e marker).
//
//   node crates/shell/e2e/native-host/run.mjs --bin <SeuOffice> --root <dir> --fixtures <dir>
//                                             [--only a,b | all | native-host]
//
// Per spec <name> it leaves, for ../../ci/native-host-check.mjs:
//
//   <root>/<name>/work/                    the files the spec opens, kept across its runs
//   <root>/<name>/run-<n>/scenario.json    the run as the app got it
//   <root>/<name>/run-<n>/out/             e2e.jsonl, native-ipc.jsonl, e2e-dialogs.jsonl
//   <root>/<name>/run-<n>/run.log          the app's output
//   <root>/<name>/run-<n>/exit.json        how the run ended and what was damaged before it
//
// The runs of a spec share one throwaway profile, which is removed after the last. A run with
// `end: 'kill'` is killed from outside once its last step is on record: the app gets no chance
// to quit, which is what a crash or a power cut leaves behind.

import { spawn, spawnSync } from 'node:child_process'
import {
  closeSync,
  cpSync,
  existsSync,
  mkdirSync,
  openSync,
  readFileSync,
  readdirSync,
  rmSync,
  writeFileSync,
} from 'node:fs'
import { join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import JSZip from 'jszip'
import { makeFixtures } from '../make-fixtures.mjs'
import { render } from '../render-scenario.mjs'
import { PROBE_BASE, SPECS, selected } from './specs.mjs'

/** After the last step: the replies of the last calls reach the record */
const SETTLE_MS = 1500
/** A run that is to be killed waits this long for it */
const HOLD_MS = 240000
const DEADLINE_MS = 900000
/** A run that outlives this is killed, whatever it was doing */
const GUARD_MS = 1200000
const POLL_MS = 500
const MARKER = '.seuoffice-e2e-profile'
const SETTINGS = 'app-settings.json'
const EPUB = 'epub.epub'
const PAPERS = 'docs-papers.docx'
const DAMAGE = Buffer.from('SEU-E2E damaged on purpose\n')

/**
 * The expression a `query` step evaluates for an `invoke` step: one `ipc_invoke` frame (the wire
 * form of crates/shell/bridge/wire.ts, no buffers) sent through the page's own document bridge.
 * The bridge drops the reply (no call of its own waits for this id); the shell records it.
 */
export function invokeQuery(request, channel, args) {
  const header = JSON.stringify(JSON.stringify({ id: request, channel, args }))
  return (
    `(async () => { const text = new TextEncoder().encode(${header}); ` +
    'const frame = new Uint8Array(text.length + 8); const view = new DataView(frame.buffer); ' +
    'view.setUint32(0, text.length, true); frame.set(text, 4); ' +
    "await window.__TAURI_INTERNALS__.invoke('ipc_invoke', frame); " +
    `return { request: ${request}, channel: ${JSON.stringify(channel)} } })()`
  )
}

/** The scenario of one run and how many of its steps must be on record before a kill. */
export function compileRun(run, runNumber) {
  const steps = run.steps.map((step, index) =>
    step.op === 'invoke'
      ? {
          op: 'query',
          as: step.as,
          js: invokeQuery(PROBE_BASE + runNumber * 1000 + index, step.channel, step.args),
        }
      : step,
  )
  steps.push({ op: 'wait', ms: SETTLE_MS })
  const awaited = steps.length
  if (run.end === 'kill') steps.push({ op: 'wait', ms: HOLD_MS })
  return { scenario: { deadlineMs: DEADLINE_MS, steps }, awaited }
}

/**
 * A Word document with a second section on another paper, made from one that has a single
 * section: the Word page prints a document of more than one paper size group by group.
 */
export async function twoPaperDocx(source) {
  const zip = await JSZip.loadAsync(source)
  const part = 'word/document.xml'
  const xml = await zip.file(part).async('string')
  const start = xml.lastIndexOf('<w:sectPr')
  const end = xml.indexOf('</w:sectPr>', start) + '</w:sectPr>'.length
  const section = xml.slice(start, end)
  const paper = /<w:pgSz [^>]*\/>/
  if (start < 0 || !paper.test(section)) throw new Error('the Word fixture names no paper')
  const other = section.replace(paper, '<w:pgSz w:w="16838" w:h="11906" w:orient="landscape"/>')
  zip.file(
    part,
    `${xml.slice(0, start)}<w:p><w:pPr>${section}</w:pPr></w:p>` +
      `<w:p><w:r><w:t>A page on another paper.</w:t></w:r></w:p>${other}${xml.slice(end)}`,
  )
  return zip.generateAsync({ type: 'nodebuffer', compression: 'DEFLATE' })
}

/** A one-chapter EPUB 3 book: the repo carries no small one. */
export async function minimalEpub() {
  const zip = new JSZip()
  zip.file('mimetype', 'application/epub+zip', { compression: 'STORE' })
  zip.file(
    'META-INF/container.xml',
    '<?xml version="1.0" encoding="UTF-8"?>\n' +
      '<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">' +
      '<rootfiles><rootfile full-path="OEBPS/content.opf" ' +
      'media-type="application/oebps-package+xml"/></rootfiles></container>\n',
  )
  zip.file(
    'OEBPS/content.opf',
    '<?xml version="1.0" encoding="UTF-8"?>\n' +
      '<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="id">' +
      '<metadata xmlns:dc="http://purl.org/dc/elements/1.1/">' +
      '<dc:identifier id="id">urn:uuid:5e0e2e00-0000-4000-8000-00000000e2e0</dc:identifier>' +
      '<dc:title>SeuOffice e2e</dc:title><dc:language>en</dc:language>' +
      '<meta property="dcterms:modified">2026-01-01T00:00:00Z</meta></metadata>' +
      '<manifest><item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" ' +
      'properties="nav"/><item id="c1" href="chapter1.xhtml" ' +
      'media-type="application/xhtml+xml"/></manifest>' +
      '<spine><itemref idref="c1"/></spine></package>\n',
  )
  const page = (title, body) =>
    '<?xml version="1.0" encoding="UTF-8"?>\n<!DOCTYPE html>\n' +
    '<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops">' +
    `<head><title>${title}</title></head><body>${body}</body></html>\n`
  zip.file(
    'OEBPS/nav.xhtml',
    page(
      'Contents',
      '<nav epub:type="toc"><ol><li><a href="chapter1.xhtml">Chapter 1</a></li></ol></nav>',
    ),
  )
  zip.file(
    'OEBPS/chapter1.xhtml',
    page('Chapter 1', '<h1>Chapter 1</h1><p>SEU-E2E-EPUB first paragraph.</p>'),
  )
  return zip.generateAsync({ type: 'nodebuffer', compression: 'DEFLATE' })
}

/**
 * The recovery copies of the profile (`<kind>-autosave/**`, recovery-store.ts), without their
 * metadata. The launch recovery list offers these and nothing else: a profile without one shows
 * no list.
 */
export function recoveryCopies(userData) {
  const copies = []
  const visit = (folder) => {
    for (const entry of readdirSync(folder, { withFileTypes: true })) {
      const path = join(folder, entry.name)
      if (entry.isDirectory()) visit(path)
      else if (!entry.name.endsWith('.meta.json') && !entry.name.endsWith('.tmp')) copies.push(path)
    }
  }
  if (!existsSync(userData)) return copies
  for (const entry of readdirSync(userData, { withFileTypes: true })) {
    if (entry.isDirectory() && entry.name.endsWith('-autosave')) visit(join(userData, entry.name))
  }
  return copies
}

/**
 * Overwrites every recovery copy of the profile and leaves its metadata, so the launch still
 * offers the copy. Answers the files it overwrote.
 */
export function damageCopies(userData) {
  const damaged = recoveryCopies(userData)
  for (const path of damaged) writeFileSync(path, DAMAGE)
  return damaged
}

/**
 * What a run's `damage` names, overwritten: 'copies' is every recovery copy of the profile, any
 * other name a file of the work folder; a list is each of its entries. Answers the files.
 */
export function damage(what, { userData, work }) {
  return [what ?? []].flat().flatMap((entry) => {
    if (entry === 'copies') return damageCopies(userData)
    writeFileSync(join(work, entry), DAMAGE)
    return [join(work, entry)]
  })
}

const slash = (dir) => dir.replaceAll('\\', '/').replace(/\/+$/, '')
const sleep = (ms) => new Promise((done) => setTimeout(done, ms))

function recordCount(file) {
  if (!existsSync(file)) return 0
  return readFileSync(file, 'utf8')
    .split('\n')
    .filter((line) => line.trim().length > 0).length
}

function killTree(pid) {
  if (process.platform === 'win32') spawnSync('taskkill', ['/F', '/T', '/PID', String(pid)])
  else {
    try {
      process.kill(pid, 'SIGKILL')
    } catch {
      // gone already
    }
  }
}

/** The next run starts from no app at all. */
function sweep() {
  if (process.platform === 'win32') spawnSync('taskkill', ['/F', '/T', '/IM', 'SeuOffice.exe'])
  else spawnSync('pkill', ['-x', 'SeuOffice'])
}

/** Starts the app on one scenario and answers how it ended: by itself, killed on purpose, or late. */
async function runApp({ bin, dir, env, awaited, kill }) {
  const out = join(dir, 'out')
  const log = openSync(join(dir, 'run.log'), 'w')
  const child = spawn(bin, ['--e2e', join(dir, 'scenario.json'), '--out', out], {
    env: { ...process.env, ...env },
    stdio: ['ignore', log, log],
  })
  const exited = new Promise((done) => {
    child.once('exit', (code, signal) => done({ code, signal }))
    child.once('error', (error) => done({ code: null, signal: null, error: String(error) }))
  })
  const started = Date.now()
  let ended = 'exit'
  let over = false
  void exited.then(() => {
    over = true
  })
  while (!over) {
    await sleep(POLL_MS)
    if (over) break
    const late = Date.now() - started > GUARD_MS
    if (late || (kill && recordCount(join(out, 'e2e.jsonl')) >= awaited)) {
      ended = late ? 'timeout' : 'kill'
      killTree(child.pid)
      break
    }
  }
  const status = await exited
  closeSync(log)
  return { ...status, ended, records: recordCount(join(out, 'e2e.jsonl')) }
}

function prepareWork(work, fixtures, copies = {}) {
  mkdirSync(work, { recursive: true })
  for (const name of readdirSync(fixtures)) {
    // sheets-big is not a native-host spec, and the copy is 4 MB a spec
    if (name !== 'big.xlsx') cpSync(join(fixtures, name), join(work, name), { recursive: true })
  }
  for (const [name, source] of Object.entries(copies)) cpSync(join(work, source), join(work, name))
}

/**
 * A spec's `settings` as the profile's settings file, before the first run: what a person set
 * in Settings. `${WORK}` in a value is the work folder, absolute and as the OS spells it (the
 * app takes a default save folder only as an absolute path).
 */
export function seedSettings(userData, settings, work) {
  if (!settings) return
  const fields = render(settings, { WORK: resolve(work) })
  writeFileSync(join(userData, SETTINGS), `${JSON.stringify(fields, null, 2)}\n`)
}

async function runSpec(name, spec, { bin, root, fixtures }) {
  const dir = join(root, name)
  rmSync(dir, { recursive: true, force: true })
  const work = join(dir, 'work')
  const userData = join(dir, 'userdata')
  prepareWork(work, fixtures, spec.copies)
  mkdirSync(userData, { recursive: true })
  writeFileSync(join(userData, MARKER), '')
  seedSettings(userData, spec.settings, work)
  for (const [index, run] of spec.runs.entries()) {
    const number = index + 1
    const runDir = join(dir, `run-${number}`)
    mkdirSync(join(runDir, 'out'), { recursive: true })
    // what the launch of this run has to offer, before any of it is damaged
    const copies = recoveryCopies(userData)
    const damaged = damage(run.damage, { userData, work })
    const { scenario, awaited } = compileRun(run, number)
    const rendered = render(scenario, { FIXTURES: slash(fixtures), WORK: slash(work) })
    writeFileSync(join(runDir, 'scenario.json'), `${JSON.stringify(rendered, null, 2)}\n`)
    const result = await runApp({
      bin,
      dir: runDir,
      awaited,
      kill: run.end === 'kill',
      env: {
        SEUOFFICE_USER_DATA: userData,
        SEUOFFICE_E2E_FIXTURES: slash(fixtures),
        SEUOFFICE_E2E_WORK: slash(work),
        SEUOFFICE_SHELL_TRACE: '1',
      },
    })
    sweep()
    await sleep(2000)
    // a run without a native call still names its record: an empty one
    const record = join(runDir, 'out', 'native-ipc.jsonl')
    if (!existsSync(record)) writeFileSync(record, '')
    const exit = {
      ...result,
      awaited,
      wanted: run.end ?? 'exit',
      copies: copies.map(slash),
      damaged: damaged.map(slash),
    }
    writeFileSync(join(runDir, 'exit.json'), `${JSON.stringify(exit, null, 2)}\n`)
    console.log(
      `${name} run ${number}: ${result.ended} (code ${result.code}), ` +
        `${result.records}/${awaited} steps on record`,
    )
    // a crash that did not happen leaves nothing for the next run to restore
    if (result.ended !== exit.wanted) break
  }
  try {
    rmSync(userData, { recursive: true, force: true, maxRetries: 5, retryDelay: 1000 })
  } catch (error) {
    console.log(`${name}: the profile could not be removed (${error.message})`)
  }
}

async function main(argv) {
  const option = (name) => {
    const at = argv.indexOf(name)
    return at >= 0 ? argv[at + 1] : undefined
  }
  const [bin, root, fixtures] = [option('--bin'), option('--root'), option('--fixtures')]
  if (!bin || !root || !fixtures) {
    console.error(
      'usage: run.mjs --bin <SeuOffice> --root <dir> --fixtures <dir> [--only a,b | all | native-host]',
    )
    return 64
  }
  if (!existsSync(bin)) {
    console.error(`there is no app at ${bin}`)
    return 1
  }
  if (!existsSync(fixtures)) await makeFixtures(resolve(fixtures))
  if (!existsSync(join(fixtures, EPUB))) writeFileSync(join(fixtures, EPUB), await minimalEpub())
  if (!existsSync(join(fixtures, PAPERS)))
    writeFileSync(
      join(fixtures, PAPERS),
      await twoPaperDocx(readFileSync(join(fixtures, 'docs.docx'))),
    )
  mkdirSync(root, { recursive: true })
  for (const name of selected(option('--only'))) {
    await runSpec(name, SPECS[name], { bin, root, fixtures })
  }
  return 0
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  process.exitCode = await main(process.argv.slice(2))
}
