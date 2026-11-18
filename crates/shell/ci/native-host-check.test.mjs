// Unit tests of the native-host checker and of the specs it decides, on records built here.
//   cd crates/shell/ci && npx vitest run native-host-check.test.mjs

import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'
import JSZip from 'jszip'
import { afterEach, beforeEach, describe, expect, it } from 'vitest'
import { NATIVE_CHANNEL_CONTRACTS } from '../../../tools/shell-inventory/native-channels.ts'
import { compileRun, damageCopies, invokeQuery, minimalEpub } from '../e2e/native-host/run.mjs'
import {
  ACKS,
  LIST_CONFIRM,
  PROBE_BASE,
  SPECS,
  UNDRIVEN,
  selected,
  surfaceOf,
} from '../e2e/native-host/specs.mjs'
import {
  exitCodeOf,
  joinCalls,
  markdown,
  parseJsonl,
  receiptsOf,
  runChecks,
  summary,
} from './native-host-check.mjs'

let root
beforeEach(() => {
  root = fs.mkdtempSync(path.join(os.tmpdir(), 'native-host-check-'))
})
afterEach(() => {
  fs.rmSync(root, { recursive: true, force: true })
})

// ------------------------------------------------------------------- an app that keeps the contract

const SURFACE_OF_EXT = {
  md: 'Markdown',
  html: 'Html',
  docx: 'Docs',
  pptx: 'Slides',
  drawio: 'Diagram',
  xmind: 'Mindmap',
  hwp: 'Hwp',
  pdf: 'Pdf',
  epub: 'Epub',
  xlsx: 'Sheets',
}
const text = (length = 36) => ({ type: 'string', length })
const flag = (value) => ({ type: 'boolean', value })
const object = (fields) => ({ type: 'object', fields })
const ANSWERED = { ok: true, value: { type: 'undefined' } }
const refusal = (error) => ({ ok: false, error })

/** The acknowledgement a page sends on `channel`, as the record shows it */
function ackArgs(channel, verdict) {
  if (channel.endsWith(':saved-input-applied')) {
    const word = verdict ? 'applied' : 'failed'
    return [object({ token: text(), outcome: { ...text(word.length), word } })]
  }
  if (channel.endsWith(':recovery-applied')) return [object({ token: text(), ok: flag(verdict) })]
  if (channel.startsWith('hwp:')) return [text(), object({ ok: flag(verdict) })]
  return [text(), flag(verdict)]
}

/** The channel a restored page of a surface acknowledges on */
const RESTORED = {
  Markdown: 'markdown:recovery-applied',
  Html: 'html:recovery-applied',
  Docs: 'docs:recovery-initialized',
  Slides: 'slides:recovery-initialized',
  Diagram: 'diagram:recovery-initialized',
  Mindmap: 'mindmap:recovery-initialized',
  Hwp: 'hwp:recovery-initialized',
  Pdf: 'pdf:recovery-initialized',
  Epub: 'epub:recovery-initialized',
}

const hasReplay = (value) =>
  value !== null &&
  typeof value === 'object' &&
  ('e2eReplay' in value ? value : Object.values(value).map(hasReplay).find(Boolean))

/**
 * Writes what a spec leaves when the app keeps every contract: each tab acknowledges what it was
 * handed, each probe is refused. `tamper(run number, lines, records)` may change that.
 */
function simulate(name, spec = SPECS[name], tamper = () => {}) {
  let nextView = 10
  let firstFile
  for (const [index, run] of spec.runs.entries()) {
    const number = index + 1
    const { scenario, awaited } = compileRun(run, number)
    const dir = path.join(root, name, `run-${number}`)
    fs.mkdirSync(path.join(dir, 'out'), { recursive: true })
    const lines = []
    const records = []
    const tabs = new Map()
    const answered = []
    let seq = 0
    let request = 0
    let active
    let pending = number > 1 ? firstFile : undefined
    const send = (tab, channel, id, args, reply, replayed) => {
      seq += 1
      lines.push({
        event: 'call',
        seq,
        channel,
        kind: 'invoke',
        request: id,
        view: tab.view,
        surface: tab.surface,
        native: true,
        args,
        ...(replayed && { replayed: [replayed] }),
      })
      lines.push({ event: 'reply', seq, ...reply })
      if (reply.ok) answered.push({ view: tab.view, channel })
    }
    const ownCall = (tab, channel, args, reply = ANSWERED) =>
      send(tab, channel, (request += 1), args, reply)
    const becomeReady = (file) => {
      const known = tabs.get(file)
      if (known) return known
      const tab = { view: (nextView += 1), surface: SURFACE_OF_EXT[file.split('.').pop()] }
      tabs.set(file, tab)
      const restored = number > 1 && file === firstFile
      const damaged = restored && Boolean(run.damage)
      if (tab.surface === 'Pdf') {
        const seed = object({ path: text(40), token: text() })
        ownCall(tab, 'pdf:consume-recovery-pending', [], {
          ok: true,
          value: restored ? seed : { type: 'null' },
        })
      }
      if (restored) ownCall(tab, RESTORED[tab.surface], ackArgs(RESTORED[tab.surface], !damaged))
      else if (tab.surface === 'Markdown' || tab.surface === 'Html')
        ownCall(
          tab,
          `${tab.surface.toLowerCase()}:saved-input-applied`,
          ackArgs(`${tab.surface.toLowerCase()}:saved-input-applied`, true),
        )
      return tab
    }
    // a restored tab loads by itself, whether or not a step waits for it
    if (pending && !scenario.steps.some((step) => step.op === 'waitReady')) becomeReady(pending)
    for (const [step, spelled] of scenario.steps.entries()) {
      const record = { step, op: spelled.op, ok: true, ms: 1 }
      if (spelled.op === 'open') pending = path.basename(spelled.path)
      if (spelled.op === 'waitReady') {
        active = becomeReady(pending)
        firstFile ??= pending
        record.value = { view: active.view }
      }
      if (spelled.op === 'query') {
        record.as = spelled.as
        const probe = run.steps[step]?.op === 'invoke' ? run.steps[step] : undefined
        if (!probe)
          record.value = `text with ${spec.expect.map((e) => e.rule.proof?.marker).find(Boolean)}`
        else {
          const id = Number(/request: (\d+)/.exec(spelled.js)[1])
          record.value = { request: id, channel: probe.channel }
          const marker = probe.args.map(hasReplay).find(Boolean)
          const source = marker
            ? answered.findLast(
                (call) =>
                  call.channel === probe.channel &&
                  (call.view !== active.view) === (marker.e2eReplay === 'other'),
              )
            : undefined
          const open = active.surface === surfaceOf(probe.channel)
          send(
            active,
            probe.channel,
            id,
            [],
            marker && !source
              ? { ...refusal('e2e replay: nothing answered'), unresolvedReplay: true }
              : !open
                ? refusal(`'${probe.channel}' is not open to ${active.surface}`)
                : probe.channel === 'workbook:saved-open-result'
                  ? refusal(SHEETS_REFUSALS[spelled.as])
                  : probe.channel === 'pdf:consume-recovery-pending'
                    ? { ok: true, value: { type: 'null' } }
                    : refusal('the receipt does not match'),
            source && { of: marker.e2eReplay, view: source.view },
          )
        }
      }
      records.push(record)
    }
    const exit = {
      code: run.end === 'kill' ? null : 0,
      ended: run.end ?? 'exit',
      wanted: run.end ?? 'exit',
      awaited,
      records: records.length,
      damaged: run.damage ? ['a-copy'] : [],
    }
    tamper(number, lines, records, exit)
    const jsonl = (rows) => rows.map((row) => `${JSON.stringify(row)}\n`).join('')
    fs.writeFileSync(path.join(dir, 'scenario.json'), JSON.stringify(scenario))
    fs.writeFileSync(path.join(dir, 'out', 'e2e.jsonl'), jsonl(records))
    fs.writeFileSync(path.join(dir, 'out', 'native-ipc.jsonl'), jsonl(lines))
    fs.writeFileSync(path.join(dir, 'exit.json'), JSON.stringify(exit))
  }
}

const SHEETS_REFUSALS = {
  'sheets-missing-identity': 'The saved workbook acknowledgement has no identity',
  'sheets-missing-result': 'The saved workbook acknowledgement has no result',
  'sheets-inconsistent-error': 'The saved workbook acknowledgement is invalid',
  'sheets-no-opening': 'The saved workbook opening is unavailable',
}

const check = (name, specs = SPECS) => runChecks({ root, names: [name], specs })
const rowOf = (rows, channel, scenario) =>
  rows.find((row) => row.channel === channel && row.scenario === scenario)

// --------------------------------------------------------------------------------------- the specs

describe('the specs against the contracts', () => {
  const driven = new Map()
  for (const spec of Object.values(SPECS)) {
    for (const { channel, scenario } of spec.expect) {
      driven.set(channel, (driven.get(channel) ?? new Set()).add(scenario))
    }
  }

  it('drives or names as undriven every required scenario of every contract, and nothing else', () => {
    for (const contract of NATIVE_CHANNEL_CONTRACTS) {
      const covered = [...(driven.get(contract.name) ?? [])]
      const undriven = Object.keys(UNDRIVEN[contract.name] ?? {})
      expect(covered.filter((scenario) => undriven.includes(scenario))).toEqual([])
      expect([...covered, ...undriven].sort()).toEqual([...contract.requiredScenarios].sort())
    }
    const names = NATIVE_CHANNEL_CONTRACTS.map((contract) => contract.name)
    expect([...driven.keys(), ...Object.keys(UNDRIVEN)].filter((n) => !names.includes(n))).toEqual(
      [],
    )
  })

  it('names the surface of each channel as its contract does', () => {
    for (const contract of NATIVE_CHANNEL_CONTRACTS) {
      expect(contract.surfaces.map((s) => s[0].toUpperCase() + s.slice(1))).toEqual([
        surfaceOf(contract.name),
      ])
    }
    const acknowledgements = NATIVE_CHANNEL_CONTRACTS.map((contract) => contract.name).filter(
      (channel) => /:(recovery-initialized|recovery-applied|saved-input-applied)$/.test(channel),
    )
    expect(Object.keys(ACKS).sort()).toEqual(acknowledgements.sort())
  })

  it('gives every rule the step it reads', () => {
    for (const [name, spec] of Object.entries(SPECS)) {
      for (const { rule } of spec.expect) {
        const run = spec.runs[rule.run - 1]
        expect(run, `${name}: run ${rule.run}`).toBeDefined()
        const has = (matches) =>
          expect(run.steps.some(matches), `${name}: ${JSON.stringify(rule)}`).toBe(true)
        if (rule.probe) has((step) => step.op === 'invoke' && step.as === rule.probe)
        if (rule.list) has((step) => step.op === 'click' && step.selector === LIST_CONFIRM)
        if (rule.proof) {
          has((step) => step.op === 'query' && step.as === rule.proof.query)
          expect(rule.proof.marker, name).toMatch(/^SEU-E2E-/)
        }
        if (rule.view)
          expect(run.steps.filter((step) => step.op === 'waitReady').length).toBeGreaterThanOrEqual(
            rule.view,
          )
      }
      for (const run of spec.runs) {
        const names = run.steps.map((step) => step.as).filter(Boolean)
        expect(new Set(names).size, `${name}: step names repeat`).toBe(names.length)
      }
    }
  })

  it('passes every expectation when the app keeps the contracts', () => {
    for (const name of Object.keys(SPECS)) {
      simulate(name)
      const rows = check(name)
      expect(rows.filter((row) => row.status !== 'PASS')).toEqual([])
      expect(rows).toHaveLength(SPECS[name].expect.length)
    }
  })
})

describe('an invoke step', () => {
  it('becomes a query that sends one framed call and answers with its request id', async () => {
    const sent = []
    const page = {
      __TAURI_INTERNALS__: { invoke: async (command, frame) => void sent.push([command, frame]) },
    }
    const args = [{ token: { e2eReplay: 'self', path: [0, 'token'] }, ok: true }]
    const js = invokeQuery(PROBE_BASE + 7, 'html:recovery-applied', args)
    expect(js.trim().endsWith(';')).toBe(false)
    const value = await new Function('window', `return ${js}`)(page)
    expect(value).toEqual({ request: PROBE_BASE + 7, channel: 'html:recovery-applied' })
    const [[command, frame]] = sent
    expect(command).toBe('ipc_invoke')
    const view = new DataView(frame.buffer)
    const length = view.getUint32(0, true)
    expect(frame.length).toBe(length + 8)
    expect(view.getUint32(4 + length, true)).toBe(0)
    expect(JSON.parse(new TextDecoder().decode(frame.subarray(4, 4 + length)))).toEqual({
      id: PROBE_BASE + 7,
      channel: 'html:recovery-applied',
      args,
    })
  })

  it('keeps its step index, and a killed run waits after its last awaited step', () => {
    const run = {
      end: 'kill',
      steps: [
        { op: 'wait', ms: 1 },
        { op: 'invoke', as: 'p', channel: 'c', args: [] },
      ],
    }
    const { scenario, awaited } = compileRun(run, 2)
    expect(scenario.steps.map((step) => step.op)).toEqual(['wait', 'query', 'wait', 'wait'])
    expect(scenario.steps[1].js).toContain(`request: ${PROBE_BASE + 2001}`)
    expect(awaited).toBe(3)
    expect(compileRun({ steps: run.steps }, 1).scenario.steps).toHaveLength(3)
  })
})

// ------------------------------------------------------------------------------------- the rules

describe('the rules', () => {
  const channel = 'markdown:recovery-applied'

  it('fail a probe that was answered, sent from elsewhere, or never recorded', () => {
    simulate('markdown-recovery', undefined, (number, lines) => {
      if (number !== 1) return
      const answer = (as, change) => {
        const call = lines.find((line) => line.event === 'call' && line.channel === channel)
        Object.assign(
          lines.find((line) => line.event === 'reply' && line.seq === call.seq + as),
          change,
        )
      }
      answer(0, { ok: true, error: undefined, value: { type: 'undefined' } })
    })
    const rows = check('markdown-recovery')
    expect(rowOf(rows, channel, 'missing-receipt')).toMatchObject({
      status: 'FAIL',
      evidence: expect.stringContaining('answered without an error'),
    })
    expect(rowOf(rows, channel, 'foreign-receipt').status).toBe('PASS')

    simulate('surfaces', undefined, (_, lines) => {
      for (const line of lines) if (line.event === 'call') line.surface = surfaceOf(line.channel)
    })
    expect(check('surfaces').every((row) => row.status === 'FAIL')).toBe(true)

    simulate('sheets-saved-open', undefined, (_, lines) => lines.splice(0))
    expect(check('sheets-saved-open')[0]).toMatchObject({
      status: 'FAIL',
      evidence: expect.stringContaining('is not on record'),
    })
  })

  it('want the text of a refusal where the contract fixes it', () => {
    simulate('sheets-saved-open', undefined, (_, lines) => {
      for (const line of lines) if (line.event === 'reply') line.error = 'some other refusal'
    })
    const rows = check('sheets-saved-open')
    expect(rows.map((row) => row.status)).toEqual(['FAIL', 'FAIL', 'FAIL', 'FAIL'])
    expect(rows[0].evidence).toContain('another text')
  })

  it('fail a restore whose list was not answered, whose page shows no text, or that was not acknowledged', () => {
    simulate('markdown-recovery', undefined, (number, _lines, records) => {
      if (number !== 2) return
      Object.assign(records[0], { ok: false, error: 'there is no active tab' })
    })
    expect(rowOf(check('markdown-recovery'), channel, 'successful-ack')).toMatchObject({
      status: 'FAIL',
      evidence: expect.stringContaining('launch recovery list was not answered'),
    })

    // the restored tab never became ready: the second tab's acknowledgement is not taken for it
    simulate('markdown-recovery', undefined, (number, _lines, records) => {
      if (number !== 2) return
      const restored = records.find((record) => record.op === 'waitReady')
      Object.assign(restored, { ok: false, value: undefined, error: 'not ready' })
    })
    expect(rowOf(check('markdown-recovery'), channel, 'successful-ack').evidence).toBe(
      'tab 1 of the run was never ready',
    )

    simulate('markdown-recovery', undefined, (number, _lines, records) => {
      if (number === 2)
        records.find((record) => record.as === 'markdown-after').value = 'the saved text'
    })
    expect(rowOf(check('markdown-recovery'), channel, 'successful-ack').evidence).toContain(
      'does not show SEU-E2E-markdown',
    )

    simulate('markdown-recovery', undefined, (number, lines) => {
      if (number !== 2) return
      const own = lines.find((line) => line.event === 'call' && line.channel === channel)
      own.args[0].fields.ok.value = false
    })
    const rows = check('markdown-recovery')
    expect(rowOf(rows, channel, 'successful-ack')).toMatchObject({
      status: 'FAIL',
      evidence: expect.stringContaining('no accepted success acknowledgement'),
    })
  })

  it('fail a replay that found no receipt or was accepted', () => {
    simulate('docs-recovery', undefined, (number, lines) => {
      if (number !== 2) return
      // the restored page never acknowledged: nothing to present twice
      const own = lines.findIndex((line) => line.event === 'call' && line.request < PROBE_BASE)
      lines.splice(own, 2)
      const twice = lines.find((line) => line.event === 'call' && line.replayed?.[0].of === 'self')
      delete twice.replayed
      Object.assign(
        lines.find((line) => line.event === 'reply' && line.seq === twice.seq),
        {
          error: 'e2e replay: this view has no answered call',
          unresolvedReplay: true,
        },
      )
    })
    const docs = 'docs:recovery-initialized'
    expect(rowOf(check('docs-recovery'), docs, 'duplicate-ack')).toMatchObject({
      status: 'FAIL',
      evidence: expect.stringContaining('no receipt to present'),
    })

    simulate('docs-recovery', undefined, (number, lines) => {
      if (number !== 2) return
      const other = lines.find((line) => line.event === 'call' && line.replayed?.[0].of === 'other')
      const reply = lines.find((line) => line.event === 'reply' && line.seq === other.seq)
      Object.assign(reply, { ok: true, value: { type: 'undefined' } })
    })
    expect(rowOf(check('docs-recovery'), docs, 'foreign-renderer')).toMatchObject({
      status: 'FAIL',
      evidence: expect.stringContaining('was accepted'),
    })
  })

  it('tell a handed-over document from an empty answer', () => {
    simulate('pdf-restore', undefined, (number, lines) => {
      if (number !== 2) return
      const consume = lines.find((line) => line.event === 'call')
      lines.find((line) => line.event === 'reply' && line.seq === consume.seq).value = {
        type: 'null',
      }
    })
    const rows = check('pdf-restore')
    const consume = 'pdf:consume-recovery-pending'
    expect(rowOf(rows, consume, 'captured-consume').evidence).toContain('answered null, not seed')
    expect(rowOf(rows, consume, 'renderer-apply').evidence).toContain('no document was handed over')
    expect(rowOf(rows, consume, 'ordinary-empty').status).toBe('PASS')
    expect(rowOf(rows, 'pdf:recovery-initialized', 'successful-ack').status).toBe('PASS')
  })

  it('want something damaged before a damaged restore counts', () => {
    simulate('docs-damaged', undefined, (number, _lines, _records, exit) => {
      if (number === 2) exit.damaged = []
    })
    expect(check('docs-damaged')[0].evidence).toContain('nothing to damage')
  })

  it('say when a run never started and when a spec was not run', () => {
    simulate('docs-recovery')
    fs.rmSync(path.join(root, 'docs-recovery', 'run-2'), { recursive: true })
    const exit = path.join(root, 'docs-recovery', 'run-1', 'exit.json')
    fs.writeFileSync(exit, JSON.stringify({ ended: 'exit', wanted: 'kill', damaged: [] }))
    const rows = check('docs-recovery')
    const late = rowOf(rows, 'docs:recovery-initialized', 'successful-ack')
    expect(late).toMatchObject({
      status: 'FAIL',
      artifact: 'docs-recovery/run-1/out/native-ipc.jsonl',
    })
    expect(late.evidence).toContain('run 2 never started (run 1 ended by exit, not kill)')
    expect(check('slides-recovery').every((row) => row.status === 'MISSING')).toBe(true)
    expect(exitCodeOf(check('slides-recovery'))).toBe(1)
  })
})

describe('the receipts', () => {
  it('are one per spec, channel, verdict and run, and leave a spec that did not run out', () => {
    simulate('markdown-recovery', undefined, (number, lines) => {
      if (number !== 2) return
      const own = lines.find(
        (line) => line.event === 'call' && line.channel === 'markdown:recovery-applied',
      )
      own.args[0].fields.ok.value = false
    })
    const rows = [...check('markdown-recovery'), ...check('html-recovery')]
    const receipts = receiptsOf(rows, 'macOS')
    expect(receipts.every((receipt) => receipt.spec === 'markdown-recovery')).toBe(true)
    const recovery = receipts.filter((receipt) => receipt.channel === 'markdown:recovery-applied')
    expect(recovery).toEqual([
      {
        channel: 'markdown:recovery-applied',
        kind: 'native-host',
        status: 'passed',
        scenarios: ['missing-receipt', 'foreign-receipt'],
        artifact: 'markdown-recovery/run-1/out/native-ipc.jsonl',
        os: 'macOS',
        spec: 'markdown-recovery',
      },
      expect.objectContaining({ status: 'failed', scenarios: ['successful-ack'] }),
      expect.objectContaining({
        status: 'passed',
        scenarios: ['duplicate-ack', 'foreign-renderer'],
        artifact: 'markdown-recovery/run-2/out/native-ipc.jsonl',
      }),
    ])
    for (const receipt of receipts) {
      expect(fs.existsSync(path.join(root, receipt.artifact))).toBe(true)
      const contract = NATIVE_CHANNEL_CONTRACTS.find((c) => c.name === receipt.channel)
      expect(receipt.scenarios.every((s) => contract.requiredScenarios.includes(s))).toBe(true)
    }
    expect(summary(rows)).toMatch(/^\d+ PASS, 1 FAIL, 10 MISSING of 20 native-host scenarios$/)
    expect(markdown(rows)).toContain('| FAIL | markdown-recovery | markdown:recovery-applied |')
    expect(exitCodeOf(rows)).toBe(1)
  })
})

describe('the helpers', () => {
  it('read records up to a line the kill cut, and join each call with its reply', () => {
    const lines = parseJsonl(
      '{"event":"call","seq":1}\n{"event":"reply","seq":1,"ok":true}\n{"event":"ca',
    )
    expect(joinCalls(lines)).toEqual([{ event: 'call', seq: 1, reply: lines[1] }])
    expect(joinCalls(parseJsonl('{"event":"call","seq":2}\n'))[0].reply).toBeUndefined()
  })

  it('pick specs by name, or all of them', () => {
    expect(selected(undefined)).toEqual(Object.keys(SPECS))
    expect(selected('all')).toEqual(Object.keys(SPECS))
    expect(selected('docs-edit,native-host')).toEqual(Object.keys(SPECS))
    expect(selected(' surfaces , docs-recovery,nope')).toEqual(['surfaces', 'docs-recovery'])
    expect(selected('docs-edit,sheets-big')).toEqual([])
  })

  it('damage recovery copies and leave their metadata', () => {
    const copies = path.join(root, 'docs-autosave', 'untitled')
    fs.mkdirSync(copies, { recursive: true })
    fs.mkdirSync(path.join(root, 'Cache'))
    fs.writeFileSync(path.join(root, 'docs-autosave', 'abc.docx'), 'copy')
    fs.writeFileSync(path.join(root, 'docs-autosave', 'abc.docx.meta.json'), '{}')
    fs.writeFileSync(path.join(copies, 'id.docx'), 'copy')
    fs.writeFileSync(path.join(root, 'Cache', 'kept'), 'kept')
    expect(
      damageCopies(root)
        .map((file) => path.basename(file))
        .sort(),
    ).toEqual(['abc.docx', 'id.docx'])
    expect(fs.readFileSync(path.join(root, 'docs-autosave', 'abc.docx'), 'utf8')).toContain(
      'damaged',
    )
    expect(fs.readFileSync(path.join(root, 'docs-autosave', 'abc.docx.meta.json'), 'utf8')).toBe(
      '{}',
    )
    expect(fs.readFileSync(path.join(root, 'Cache', 'kept'), 'utf8')).toBe('kept')
    expect(damageCopies(path.join(root, 'none'))).toEqual([])
  })

  it('build an EPUB whose first entry is its uncompressed mimetype', async () => {
    const bytes = await minimalEpub()
    expect(bytes.subarray(30, 38).toString()).toBe('mimetype')
    expect(bytes.subarray(38, 58).toString()).toBe('application/epub+zip')
    const zip = await JSZip.loadAsync(bytes)
    expect(await zip.file('META-INF/container.xml').async('string')).toContain('OEBPS/content.opf')
    expect(await zip.file('OEBPS/chapter1.xhtml').async('string')).toContain('SEU-E2E-EPUB')
  })
})
