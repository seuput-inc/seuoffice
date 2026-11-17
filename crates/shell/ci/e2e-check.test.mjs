// Unit tests of the gate=e2e checker on synthetic e2e.jsonl inputs and files built here.
//   cd crates/shell/ci && npx vitest run e2e-check.test.mjs

import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'
import { deflateRawSync, deflateSync } from 'node:zlib'
import CFB from 'cfb'
import JSZip from 'jszip'
import { afterEach, beforeEach, describe, expect, it } from 'vitest'
import { bigWorkbook, mindMap, multiPagePdf } from '../e2e/make-fixtures.mjs'
import { render } from '../e2e/render-scenario.mjs'
import { drawioText, hwpBinaryText, independentText, xmlText } from './e2e-disk.mjs'
import {
  CHECKS,
  coeditCheck,
  exitCodeOf,
  formatRow,
  loadRun,
  markdown,
  parseJsonl,
  realIo,
  runChecks,
  summary,
} from './e2e-check.mjs'

let root
beforeEach(() => {
  root = fs.mkdtempSync(path.join(os.tmpdir(), 'e2e-check-'))
})
afterEach(() => {
  fs.rmSync(root, { recursive: true, force: true })
})

const engineFinds = (text) => async () => ({ ok: true, text })
const engineGone = async () => ({ ok: false, unavailable: true, error: 'tsx is not installed' })

/** A real PNG with `colors` distinct stripes (decodePng does not verify checksums). */
function png(width, height, colors) {
  const rows = []
  for (let y = 0; y < height; y += 1) {
    const row = Buffer.alloc(1 + width * 3)
    for (let x = 0; x < width; x += 1) {
      const shade = Math.floor((x * colors) / width) * 20
      row.set([shade, 255 - shade, 128], 1 + x * 3)
    }
    rows.push(row)
  }
  const chunk = (type, body) => {
    const head = Buffer.alloc(8)
    head.writeUInt32BE(body.length, 0)
    head.write(type, 4, 'latin1')
    return Buffer.concat([head, body, Buffer.alloc(4)])
  }
  const header = Buffer.alloc(13)
  header.writeUInt32BE(width, 0)
  header.writeUInt32BE(height, 4)
  header.set([8, 2, 0, 0, 0], 8)
  return Buffer.concat([
    Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]),
    chunk('IHDR', header),
    chunk('IDAT', deflateSync(Buffer.concat(rows))),
    chunk('IEND', Buffer.alloc(0)),
  ])
}

async function docx(text) {
  const zip = new JSZip()
  zip.file(
    'word/document.xml',
    `<w:document><w:body><w:p><w:r><w:t>${text.slice(0, 5)}</w:t></w:r><w:r><w:t>${text.slice(5)}</w:t></w:r></w:p></w:body></w:document>`,
  )
  return zip.generateAsync({ type: 'nodebuffer' })
}

/** The scenario's steps as the checker expects to meet them, and a log of `done` records. */
function layout(name, { steps, records, exit = 0, files = {}, shots = {} }) {
  const dir = path.join(root, name)
  fs.mkdirSync(path.join(dir, 'out'), { recursive: true })
  fs.mkdirSync(path.join(dir, 'work'), { recursive: true })
  fs.writeFileSync(path.join(dir, 'scenario.json'), JSON.stringify({ steps }))
  if (records) {
    fs.writeFileSync(
      path.join(dir, 'out', 'e2e.jsonl'),
      records.map((record) => JSON.stringify(record)).join('\n') + '\n',
    )
  }
  if (exit !== null) fs.writeFileSync(path.join(dir, 'exit-code.txt'), `${exit}\n`)
  for (const [file, content] of Object.entries(files))
    fs.writeFileSync(path.join(dir, 'work', file), content)
  for (const [file, content] of Object.entries(shots))
    fs.writeFileSync(path.join(dir, 'out', file), content)
  return dir
}

const ok = (step, op, extra = {}) => ({ step, op, ok: true, ms: 10, ...extra })

const DOCS_STEPS = [
  { op: 'open', path: 'x' },
  { op: 'type', text: 'SEU-E2E-docs' },
  { op: 'query', as: 'docs-before', js: '1' },
  { op: 'query', as: 'docs-after', js: '1' },
  { op: 'exportPdf', path: 'x' },
  { op: 'screenshot', name: 'docs-after' },
]
const docsChecks = CHECKS.filter((check) => check.scenario === 'docs-edit')
const rowsOf = (rows) => Object.fromEntries(rows.map((row) => [row.id, row]))

async function docsRun(overrides = {}) {
  const records = overrides.records ?? [
    ok(0, 'open'),
    ok(1, 'type'),
    ok(2, 'query', { value: 'Hello SEU-E2E-docs world' }),
    ok(3, 'query', { value: 'SEU-E2E-docs' }),
    ok(4, 'exportPdf'),
    ok(5, 'screenshot'),
  ]
  layout('docs-edit', {
    steps: DOCS_STEPS,
    records,
    exit: overrides.exit ?? 0,
    files: {
      'docs.docx': await docx('SEU-E2E-docs'),
      'docs-export.pdf': await multiPagePdf(1),
    },
    shots: { 'docs-after.png': png(320, 240, 12) },
  })
  const io = { ...realIo, engineText: overrides.engine ?? engineFinds('SEU-E2E-docs') }
  return runChecks({ root, scenarios: ['docs-edit'], io, repoRoot: root, checks: docsChecks })
}

describe('a scenario that did everything', () => {
  it('passes every check, with evidence', async () => {
    const rows = await docsRun()
    expect(rows.map((row) => row.status)).toEqual(Array(rows.length).fill('PASS'))
    const byId = rowsOf(rows)
    expect(byId['docs-edit/run'].evidence).toMatch(/6 steps ran/)
    expect(byId['docs-edit/disk:docs.docx'].evidence).toMatch(
      /independent read found .*SEU-E2E-docs.*repo parser found it too/,
    )
    expect(byId['docs-edit/pdf:docs-export.pdf'].evidence).toMatch(/1 page/)
    expect(byId['docs-edit/screenshot:docs-after'].evidence).toMatch(/320x240, 12 distinct/)
    expect(exitCodeOf(rows)).toBe(0)
  })

  it('accepts Korean text composed in a decomposed form', async () => {
    layout('docs-ime', {
      steps: [{ op: 'query', as: 'ime-before', js: '1' }],
      records: [ok(0, 'query', { value: '한글'.normalize('NFD') })],
    })
    const rows = await runChecks({
      root,
      scenarios: ['docs-ime'],
      io: realIo,
      repoRoot: root,
      checks: CHECKS.filter((check) => check.scenario === 'docs-ime' && check.as === 'ime-before'),
    })
    expect(rows[1].status).toBe('PASS')
  })
})

describe('what makes a check FAIL', () => {
  it('a renderer that does not show the typed text', async () => {
    const rows = await docsRun({
      records: [
        ok(0, 'open'),
        ok(1, 'type'),
        ok(2, 'query', { value: 'only the old text' }),
        ok(3, 'query', { value: 'SEU-E2E-docs' }),
        ok(4, 'exportPdf'),
        ok(5, 'screenshot'),
      ],
    })
    const byId = rowsOf(rows)
    expect(byId['docs-edit/query:docs-before'].status).toBe('FAIL')
    expect(byId['docs-edit/query:docs-before'].evidence).toMatch(
      /lacks "SEU-E2E-docs".*only the old text/,
    )
    expect(byId['docs-edit/query:docs-after'].status).toBe('PASS')
    expect(exitCodeOf(rows)).toBe(1)
  })

  it('a missing step, and the checks that needed it', async () => {
    const rows = await docsRun({ records: [ok(0, 'open'), ok(1, 'type')], exit: 3 })
    const byId = rowsOf(rows)
    expect(byId['docs-edit/run'].status).toBe('FAIL')
    expect(byId['docs-edit/run'].evidence).toMatch(/missing steps 2\.\.5 \(2 of 6 records\)/)
    expect(byId['docs-edit/run'].evidence).toMatch(/exit status 3 \(a step could not run\)/)
    expect(byId['docs-edit/query:docs-after'].status).toBe('FAIL')
    expect(byId['docs-edit/query:docs-after'].evidence).toMatch(/step 3 never ran/)
    expect(exitCodeOf(rows)).toBe(1)
  })

  it('a step that ran and failed', async () => {
    const rows = await docsRun({
      records: [
        ok(0, 'open'),
        ok(1, 'type'),
        { step: 2, op: 'query', ok: false, ms: 5, error: 'ReferenceError: nope' },
        ok(3, 'query', { value: 'SEU-E2E-docs' }),
        ok(4, 'exportPdf'),
        ok(5, 'screenshot'),
      ],
    })
    const byId = rowsOf(rows)
    expect(byId['docs-edit/run'].evidence).toMatch(/step 2 query failed: ReferenceError: nope/)
    expect(byId['docs-edit/query:docs-before'].evidence).toMatch(/failed: ReferenceError/)
  })

  it('an app that exited with the deadline status', async () => {
    const rows = await docsRun({ exit: 124 })
    expect(rowsOf(rows)['docs-edit/run'].evidence).toMatch(
      /exit status 124 \(passed its deadline\)/,
    )
  })

  it('a scenario that was never run', async () => {
    const rows = await runChecks({
      root,
      scenarios: ['docs-edit'],
      io: realIo,
      repoRoot: root,
      checks: docsChecks,
    })
    expect(rows.every((row) => row.status === 'FAIL')).toBe(true)
    expect(rows[0].evidence).toMatch(/not run/)
  })

  it('a saved file without the marker, though the renderer showed it', async () => {
    layout('docs-edit', {
      steps: DOCS_STEPS,
      records: [
        ok(0, 'open'),
        ok(1, 'type'),
        ok(2, 'query', { value: 'SEU-E2E-docs' }),
        ok(3, 'query', { value: 'SEU-E2E-docs' }),
        ok(4, 'exportPdf'),
        ok(5, 'screenshot'),
      ],
      files: { 'docs.docx': await docx('something else') },
    })
    const rows = await runChecks({
      root,
      scenarios: ['docs-edit'],
      io: { ...realIo, engineText: engineFinds('SEU-E2E-docs') },
      repoRoot: root,
      checks: docsChecks.filter((check) => check.kind === 'disk'),
    })
    expect(rows[1].status).toBe('FAIL')
    expect(rows[1].evidence).toMatch(/lacks "SEU-E2E-docs" in an independent read/)
  })

  it('a repo parser that disagrees with the independent read', async () => {
    const rows = await docsRun({ engine: engineFinds('nothing') })
    expect(rowsOf(rows)['docs-edit/disk:docs.docx'].status).toBe('FAIL')
    expect(rowsOf(rows)['docs-edit/disk:docs.docx'].evidence).toMatch(/repo parser's text lacks it/)
  })

  it('a blank screenshot', async () => {
    await docsRun()
    fs.writeFileSync(path.join(root, 'docs-edit', 'out', 'docs-after.png'), png(320, 240, 1))
    const rows = await runChecks({
      root,
      scenarios: ['docs-edit'],
      io: { ...realIo, engineText: engineFinds('SEU-E2E-docs') },
      repoRoot: root,
      checks: docsChecks,
    })
    expect(rowsOf(rows)['docs-edit/screenshot:docs-after'].status).toBe('FAIL')
    expect(rowsOf(rows)['docs-edit/screenshot:docs-after'].evidence).toMatch(/blank/)
  })
})

describe('what is UNVERIFIED, not PASS', () => {
  it('a repo parser that could not run does not fail the file, and says so', async () => {
    const rows = await docsRun({ engine: engineGone })
    const byId = rowsOf(rows)
    expect(byId['docs-edit/disk:docs.docx'].status).toBe('PASS')
    expect(byId['docs-edit/disk:docs.docx'].evidence).toMatch(
      /repo parser did not run \(tsx is not installed\)/,
    )
    expect(byId['docs-edit/pdfText:docs-export.pdf'].status).toBe('UNVERIFIED')
    expect(exitCodeOf(rows)).toBe(0)
    expect(exitCodeOf(rows, true)).toBe(2)
  })

  it('a reader that does not exist', async () => {
    layout('diagram-edit', {
      steps: [{ op: 'open', path: 'x' }],
      records: [ok(0, 'open')],
      files: {
        'diagram.drawio':
          '<mxfile><diagram><mxGraphModel><root><mxCell value="SEU-E2E-diagram"/></root></mxGraphModel></diagram></mxfile>',
      },
      shots: { 'diagram-after.png': png(300, 300, 9) },
    })
    const rows = await runChecks({
      root,
      scenarios: ['diagram-edit'],
      io: { ...realIo, engineText: engineFinds('SEU-E2E-diagram') },
      repoRoot: root,
    })
    const byId = rowsOf(rows)
    expect(byId['diagram-edit/unverified'].status).toBe('UNVERIFIED')
    expect(byId['diagram-edit/unverified'].evidence).toMatch(/cross-origin/)
    expect(byId['diagram-edit/disk:diagram.drawio'].status).toBe('PASS')
  })
})

describe('numbers and values inside a query', () => {
  const steps = [
    { op: 'waitReady', timeoutMs: 1 },
    { op: 'query', as: 'pdf-pages', js: '1' },
    { op: 'query', as: 'pdf-current', js: '1' },
    { op: 'waitReady', timeoutMs: 1 },
  ]
  const checks = [
    ...CHECKS.filter(
      (check) => check.scenario === 'pdf-view' && ['pdf-pages', 'pdf-current'].includes(check.as),
    ),
    {
      scenario: 'pdf-view',
      kind: 'stepMs',
      op: 'waitReady',
      nth: 1,
      max: 1000,
      title: 'first ready in time',
    },
    {
      scenario: 'pdf-view',
      kind: 'stepMs',
      op: 'waitReady',
      nth: 2,
      max: 1000,
      title: 'second ready in time',
    },
  ]
  it('compares a nested value exactly, and bounds the time of a step', async () => {
    layout('pdf-view', {
      steps,
      records: [
        ok(0, 'waitReady', { ms: 999 }),
        ok(1, 'query', { value: { status: 'error', error: { detail: { valid_range: '1-5' } } } }),
        ok(2, 'query', { value: { status: 'ok', result: { page: 2 } } }),
        ok(3, 'waitReady', { ms: 1001 }),
      ],
    })
    const rows = await runChecks({
      root,
      scenarios: ['pdf-view'],
      io: realIo,
      repoRoot: root,
      checks,
    })
    const byId = rowsOf(rows)
    expect(byId['pdf-view/query:pdf-pages'].status).toBe('PASS')
    expect(byId['pdf-view/query:pdf-current'].status).toBe('FAIL')
    expect(byId['pdf-view/query:pdf-current'].evidence).toMatch(/= 2, wanted 3/)
    expect(byId['pdf-view/stepMs:waitReady1'].status).toBe('PASS')
    expect(byId['pdf-view/stepMs:waitReady2'].status).toBe('FAIL')
    expect(byId['pdf-view/stepMs:waitReady2'].evidence).toMatch(/took 1001 ms, bound 1000 ms/)
  })
})

describe('co-editing', () => {
  it('is UNVERIFIED with the web tests as evidence when only the web platform installs it', () => {
    const write = (file, text) => {
      fs.mkdirSync(path.dirname(path.join(root, file)), { recursive: true })
      fs.writeFileSync(path.join(root, file), text)
    }
    write('packages/web-platform/src/collab-page.ts', 'window.seuCollab = {}')
    write('apps/shell/src/main/index.ts', 'export {}')
    write('apps/web/tests/collab-docs.spec.ts', '')
    write('packages/collab/tests/server.test.ts', '')
    const result = coeditCheck(root)
    expect(result.status).toBe('UNVERIFIED')
    expect(result.evidence).toMatch(/web-only.*apps\/web\/tests\/collab-\*\.spec\.ts \(1 files\)/)
  })

  it('FAILs when a desktop source reaches the session', () => {
    fs.mkdirSync(path.join(root, 'apps/docs/src/preload'), { recursive: true })
    fs.writeFileSync(
      path.join(root, 'apps/docs/src/preload/index.ts'),
      "import '@seuoffice/collab/client'",
    )
    fs.mkdirSync(path.join(root, 'packages/web-platform/src'), { recursive: true })
    fs.writeFileSync(
      path.join(root, 'packages/web-platform/src/collab-page.ts'),
      'window.seuCollab = {}',
    )
    expect(coeditCheck(root).status).toBe('FAIL')
  })

  it('holds for this repository', () => {
    const result = coeditCheck(
      path.resolve(path.dirname(new URL(import.meta.url).pathname), '../../..'),
    )
    expect(result.evidence).not.toMatch(/reference co-editing/)
    expect(result.status).toBe('UNVERIFIED')
  })
})

describe('readers of the saved files', () => {
  const write = (name, content) => {
    const file = path.join(root, name)
    fs.writeFileSync(file, content)
    return file
  }

  it('joins a marker split over runs of one paragraph, but not across paragraphs', () => {
    expect(
      xmlText('<w:p><w:t>SEU-E2E</w:t><w:t>-docs</w:t></w:p><w:p><w:t>x&amp;y</w:t></w:p>'),
    ).toBe('SEU-E2E-docs\nx&y\n')
  })

  it('reads a docx, an xlsx (shared and inline strings) and a pptx', async () => {
    expect(await independentText(write('a.docx', await docx('SEU-E2E-docs')))).toContain(
      'SEU-E2E-docs',
    )
    const xlsx = new JSZip()
    xlsx.file('xl/sharedStrings.xml', '<sst><si><t>한글</t></si></sst>')
    xlsx.file(
      'xl/worksheets/sheet1.xml',
      '<worksheet><sheetData><row><c t="inlineStr"><is><t>SEU-E2E-sheets</t></is></c></row></sheetData></worksheet>',
    )
    const text = await independentText(
      write('b.xlsx', await xlsx.generateAsync({ type: 'nodebuffer' })),
    )
    expect(text).toContain('한글')
    expect(text).toContain('SEU-E2E-sheets')
    const pptx = new JSZip()
    pptx.file(
      'ppt/slides/slide1.xml',
      '<p:sld><a:p><a:r><a:t>SEU-E2E-slides</a:t></a:r></a:p></p:sld>',
    )
    expect(
      await independentText(write('c.pptx', await pptx.generateAsync({ type: 'nodebuffer' }))),
    ).toContain('SEU-E2E-slides')
  })

  it('reads a compressed and a plain HWP body, and an HWPX section', async () => {
    const hwp = (compressed, text) => {
      const header = Buffer.alloc(256)
      header.writeUInt32LE(compressed ? 1 : 0, 36)
      const body = Buffer.from(`\u0000\u0000${text}\r`, 'utf16le')
      const cfb = CFB.utils.cfb_new()
      CFB.utils.cfb_add(cfb, '/FileHeader', header)
      CFB.utils.cfb_add(cfb, '/BodyText/Section0', compressed ? deflateRawSync(body) : body)
      return Buffer.from(CFB.write(cfb, { type: 'buffer' }))
    }
    expect(hwpBinaryText(hwp(true, 'SEU-E2E-hwp 한글'))).toContain('SEU-E2E-hwp 한글')
    expect(hwpBinaryText(hwp(false, 'plain'))).toContain('plain')
    expect(await independentText(write('d.hwp', hwp(true, 'SEU-E2E-hwp')))).toContain('SEU-E2E-hwp')
    const hwpx = new JSZip()
    hwpx.file(
      'Contents/section0.xml',
      '<hs:sec><hp:p><hp:run><hp:t>SEU-E2E-hwpx</hp:t></hp:run></hp:p></hs:sec>',
    )
    expect(
      await independentText(write('e.hwpx', await hwpx.generateAsync({ type: 'nodebuffer' }))),
    ).toContain('SEU-E2E-hwpx')
  })

  it('opens a compressed draw.io diagram', () => {
    const xml = '<mxGraphModel><root><mxCell value="SEU-E2E-diagram"/></root></mxGraphModel>'
    const packed = deflateRawSync(Buffer.from(encodeURIComponent(xml))).toString('base64')
    expect(drawioText(`<mxfile><diagram id="1">${packed}</diagram></mxfile>`)).toContain(
      'SEU-E2E-diagram',
    )
  })

  it('reads a mind map, Markdown and HTML', async () => {
    expect(await independentText(write('f.xmind', await mindMap()))).toContain('Branch one')
    expect(await independentText(write('g.md', '# SEU-E2E-markdown'))).toContain('SEU-E2E-markdown')
    expect(await independentText(write('h.html', '<p>SEU-E2E-html</p>'))).toContain('SEU-E2E-html')
  })

  it('refuses what it cannot read instead of guessing', async () => {
    await expect(independentText(write('i.docx', 'not a zip'))).rejects.toThrow()
    await expect(independentText(write('j.bin', 'x'))).rejects.toThrow(/no independent reader/)
  })
})

describe('the fixtures the scenarios open', () => {
  it('builds the big workbook with the cells the far-cell scenario edits', async () => {
    const template = path.resolve(
      path.dirname(new URL(import.meta.url).pathname),
      '../../../apps/shell/build/shell-new/blank.xlsx',
    )
    const book = await JSZip.loadAsync(await bigWorkbook(template, 120, 10))
    const sheet = await book.file('xl/worksheets/sheet1.xml').async('string')
    expect(sheet).toContain('<dimension ref="A1:J121"/>')
    expect(sheet).toContain('<c r="J121"><v>1089</v></c>')
  })

  it('builds a five-page PDF', async () => {
    const file = path.join(root, 'multipage.pdf')
    fs.writeFileSync(file, await multiPagePdf())
    const { pdfPageCount } = await import('./e2e-disk.mjs')
    expect(await pdfPageCount(file)).toBe(5)
  })

  it('fills the placeholders of a scenario with forward-slash paths', () => {
    const scenario = {
      steps: [
        { op: 'open', path: '${WORK}/a.docx' },
        { op: 'open', path: '${FIXTURES}/b.pdf' },
        { op: 'wait', ms: 5 },
      ],
    }
    expect(
      render(scenario, { WORK: 'C:/w', FIXTURES: 'C:/f' }).steps.map((step) => step.path),
    ).toEqual(['C:/w/a.docx', 'C:/f/b.pdf', undefined])
  })

  it('names, in every scenario file, only checks the checker knows', () => {
    const dir = path.resolve(path.dirname(new URL(import.meta.url).pathname), '../e2e/scenarios')
    const names = fs.readdirSync(dir).map((file) => file.replace(/\.json$/, ''))
    for (const check of CHECKS.filter((candidate) => candidate.scenario !== '*')) {
      expect(names, `${check.scenario} has no scenario file`).toContain(check.scenario)
      if (check.kind === 'query') {
        const steps = JSON.parse(
          fs.readFileSync(path.join(dir, `${check.scenario}.json`), 'utf8'),
        ).steps
        expect(
          steps.some((step) => step.op === 'query' && step.as === check.as),
          `${check.scenario} has no query ${check.as}`,
        ).toBe(true)
      }
    }
    for (const name of names) {
      expect(
        CHECKS.some((check) => check.scenario === name),
        `${name} has no checks`,
      ).toBe(true)
    }
  })
})

describe('the report', () => {
  it('prints one line per check and a count', async () => {
    const rows = await docsRun()
    expect(formatRow(rows[0])).toMatch(/^PASS\s+docs-edit\/run\s+every step ran/)
    expect(summary(rows)).toBe(`${rows.length} PASS, 0 FAIL, 0 UNVERIFIED of ${rows.length} checks`)
    expect(markdown(rows).split('\n')[0]).toBe(summary(rows))
  })

  it('reads a log with a broken line as a failure of the run, not a crash', () => {
    layout('docs-edit', { steps: DOCS_STEPS, records: null })
    fs.writeFileSync(
      path.join(root, 'docs-edit', 'out', 'e2e.jsonl'),
      '{"step":0,"op":"open","ok":true,"ms":1}\n{broken\n',
    )
    const run = loadRun(root, 'docs-edit', realIo)
    expect(run.records).toHaveLength(1)
    expect(run.unreadable).toBe(1)
    expect(parseJsonl('').records).toEqual([])
  })
})

describe('the staging of resources', () => {
  it('keeps what was built, falls back to the host target, and lists the rest', async () => {
    const { splitResources } = await import('../e2e/stage-resources.mjs')
    const built = new Set(['docs/out', 'sidecar/target/release/xlsx-sidecar.exe'])
    const exists = (file) => [...built].some((suffix) => file.endsWith(suffix))
    const { kept, skipped } = splitResources(
      {
        'docs/out': 'modules/docs',
        'sidecar/target/x86_64-pc-windows-gnu/release/xlsx-sidecar.exe': 'native/xlsx-sidecar.exe',
        'cli/dist/seuoffice.cjs': 'cli/seuoffice.cjs',
      },
      exists,
    )
    expect(Object.keys(kept)).toEqual([
      'docs/out',
      'sidecar/target/x86_64-pc-windows-gnu/release/xlsx-sidecar.exe',
    ])
    expect(kept['sidecar/target/x86_64-pc-windows-gnu/release/xlsx-sidecar.exe'].found).toMatch(
      /sidecar\/target\/release\/xlsx-sidecar\.exe$/,
    )
    expect(skipped).toEqual(['cli/dist/seuoffice.cjs'])
  })
})
