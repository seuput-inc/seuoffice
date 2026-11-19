// Unit tests of the native-host checker and of the specs it decides, on records built here.
//   cd crates/shell/ci && npx vitest run native-host-check.test.mjs

import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'
import JSZip from 'jszip'
import { afterEach, beforeEach, describe, expect, it } from 'vitest'
import { NATIVE_CHANNEL_CONTRACTS } from '../../../tools/shell-inventory/native-channels.ts'
import {
  compileRun,
  damage,
  damageCopies,
  invokeQuery,
  minimalEpub,
  seedSettings,
  twoPaperDocx,
} from '../e2e/native-host/run.mjs'
import {
  ACKS,
  LIST_PRESS,
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
 * Writes what a spec leaves when the app keeps every contract: a restored tab acknowledges its
 * copy, a tab opened on any other file acknowledges nothing, a copy the Word page had written
 * opens in a tab that acknowledges its text and becomes the active one, a Home button is
 * pressed, and each probe is refused. `tamper(run number, lines, records, exit)` may change that.
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
      return tab
    }
    // the shell writes the copy a page asked for and opens it in a tab of the copy's editor
    const openCopy = (channel) => {
      const tab = { view: (nextView += 1), surface: surfaceOf(channel) }
      ownCall(tab, channel, ackArgs(channel, true))
      return tab
    }
    const asks = new Map(
      spec.expect.flatMap(({ channel, rule }) => (rule.asked ? [[rule.asked, channel]] : [])),
    )
    const shown = new Map(
      spec.expect.flatMap(({ rule }) =>
        rule.proof ? [[rule.proof.query, rule.proof.marker]] : [],
      ),
    )
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
      if (spelled.op === 'saveAsOwned') {
        // the page of the active tab prints each paper's pages under the token of the save
        const printed = object({ ok: flag(true), base64: text(2400) })
        const paper = [{ type: 'number' }, { type: 'number' }, { type: 'number' }, text()]
        for (let group = 0; group < 2; group += 1)
          ownCall(active, 'docs:owned-print-pdf-buffer', paper, { ok: true, value: printed })
        record.value = { path: spelled.path, bytes: 4096, view: active.view }
      }
      if (spelled.op === 'query') {
        record.as = spelled.as
        const probe = run.steps[step]?.op === 'invoke' ? run.steps[step] : undefined
        if (spelled.as.startsWith('prep-'))
          record.value = shown.has(spelled.as)
            ? { text: `text with ${shown.get(spelled.as)}` }
            : { pressed: true }
        else if (asks.has(spelled.as)) {
          // the answer comes once the copy's tab is the active one: the step did not wait for it
          active = openCopy(asks.get(spelled.as))
          record.value = { pending: true }
        } else if (!probe)
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
      damaged: [run.damage ?? []]
        .flat()
        .map((entry) => (entry === 'copies' ? 'userdata/docs-autosave/a-copy' : `work/${entry}`)),
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
      for (const { channel, rule } of spec.expect) {
        const run = spec.runs[rule.run - 1]
        expect(run, `${name}: run ${rule.run}`).toBeDefined()
        if (rule.control) {
          const controls = SPECS[rule.control.spec]?.expect ?? []
          const named = controls.filter(
            (other) => other.channel === channel && other.scenario === rule.control.scenario,
          )
          expect(named, `${name}: ${JSON.stringify(rule.control)}`).toHaveLength(1)
        }
        const has = (matches) =>
          expect(run.steps.some(matches), `${name}: ${JSON.stringify(rule)}`).toBe(true)
        if (rule.probe) has((step) => step.op === 'invoke' && step.as === rule.probe)
        if (rule.list) has((step) => step.op === 'query' && step.as === LIST_PRESS)
        if (rule.asked) has((step) => step.op === 'query' && step.as === rule.asked)
        if (rule.kind === 'printed') has((step) => step.op === 'saveAsOwned')
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

  it('presses a button of Home only while nothing covers it', () => {
    const { js } = SPECS['docs-recovery'].runs[1].steps.find((step) => step.as === LIST_PRESS)
    const press = (document) => new Function('document', `return ${js}`)(document)
    let clicks = 0
    const label = {}
    const button = {
      disabled: false,
      getBoundingClientRect: () => ({ left: 10, top: 20, width: 100, height: 40 }),
      contains: (node) => node === label,
      click: () => (clicks += 1),
    }
    const page = (found, top) => ({
      querySelector: (selector) => (selector === '.recovery-confirm' ? found : null),
      elementFromPoint: (x, y) => (x === 60 && y === 40 ? top : null),
    })
    expect(press(page(null, null))).toBe(false)
    // run 44418489799: the onboarding lay over the list
    expect(press(page(button, { onboarding: true }))).toBe(false)
    expect(press(page({ ...button, disabled: true }, button))).toBe(false)
    expect(clicks).toBe(0)
    expect(press(page(button, button))).toEqual({ pressed: '.recovery-confirm' })
    expect(press(page(button, label))).toEqual({ pressed: '.recovery-confirm' })
    expect(clicks).toBe(2)
  })

  it('types into the HWP editor only once its page shows the document', () => {
    // run 44526047099, Windows: `waitReady` ended before the editor frame was requested
    const phase = (value) => ({
      querySelector: (selector) =>
        selector === '.hwp-app' && value ? { dataset: { phase: value } } : null,
    })
    for (const name of ['hwp-recovery', 'hwp-damaged']) {
      const { steps } = SPECS[name].runs[0]
      const gate = steps.findIndex((step) => step.as === 'prep-hwp-loaded')
      expect(gate, name).toBeGreaterThan(steps.findIndex((step) => step.op === 'waitReady'))
      expect(gate, name).toBeLessThan(steps.findIndex((step) => step.as === 'prep-caret'))
      expect(steps[gate].timeoutMs, name).toBeGreaterThanOrEqual(60000)
      const shown = (document) => new Function('document', `return ${steps[gate].js}`)(document)
      expect(['ready', 'loading', 'booting', 'failed', undefined].map(phase).map(shown)).toEqual([
        true,
        false,
        false,
        false,
        false,
      ])
    }
    const restore = SPECS['hwp-recovery'].runs[1].steps
    const restored = restore.findIndex((step) => step.as === 'prep-hwp-restored')
    expect(restored).toBe(restore.findIndex((step) => step.op === 'waitReady') + 1)
    expect(restored).toBeLessThan(restore.findIndex((step) => step.as === 'hwp-after'))
    // an editor whose page has no such sign gets no gate
    expect(SPECS['docs-recovery'].runs[0].steps.some((step) => /-loaded$/.test(step.as))).toBe(
      false,
    )
  })

  it('has the Word page ask for a copy and reads it only on the page of that copy', async () => {
    const read = (js, window, document) =>
      new Function('window', 'document', `return ${js}`)(window, document)
    let presses = 0
    const views = [{}, {}, { click: () => (presses += 1) }]
    const page = (selector, textContent) => ({
      querySelector: (asked) =>
        asked === selector && textContent !== undefined ? { textContent } : null,
      querySelectorAll: () => views,
    })
    for (const [app, type, api, editor] of [
      ['markdown', 'md', 'markdownApi', '.ProseMirror'],
      ['html', 'html', 'htmlApi', '.cm-content'],
    ]) {
      const spec = SPECS[`${app}-saved-copy`]
      const { steps } = spec.runs[0]
      const marker = `SEU-E2E-saved-${app}`
      const order = [
        `${app}-saved-create`,
        `prep-${app}-saved-shown`,
        `${app}-saved-twice`,
        `${app}-saved-other-renderer`,
      ].map((as) => steps.findIndex((step) => step.as === as))
      expect(order, app).toEqual([...order].sort((a, b) => a - b))
      expect(order[0], app).toBeGreaterThan(steps.findIndex((step) => step.op === 'waitReady'))
      // the receipt is presented from another page only after that page's file was opened
      expect(steps.findLastIndex((step) => step.op === 'waitReady')).toBeGreaterThan(order[2])
      // the copy lands in the work folder, not in the runner's Documents
      expect(spec.settings).toEqual({ defaultSaveDir: '${WORK}' })

      // the request is the Word page's own call; the step says what it was answered, and goes
      // on without the answer when a tab is still opening
      const create = steps[order[0]]
      const requests = []
      const timers = []
      const ask = (createDocument) =>
        new Function('window', 'setTimeout', `return ${create.js}`)(
          { desktop: { createDocument: (request) => (requests.push(request), createDocument()) } },
          (done, ms) => timers.push([done, ms]),
        )
      const answer = { ok: true, path: '/work/copy' }
      expect(await ask(async () => answer)).toEqual({ answer })
      expect(requests).toEqual([
        { type, title: `seu-e2e-saved-${app}`, content: expect.stringContaining(marker) },
      ])
      const refused = Promise.reject(new Error('the Word document owner does not match'))
      expect(await ask(() => refused)).toEqual({
        refused: 'the Word document owner does not match',
      })
      const slow = ask(() => new Promise(() => {}))
      const [late, ms] = timers.at(-1)
      expect(ms, app).toBeLessThan(create.timeoutMs)
      late({ pending: true })
      expect(await slow).toEqual({ pending: true })

      const { js, timeoutMs } = steps[order[1]]
      expect(timeoutMs, app).toBeGreaterThanOrEqual(60000)
      const text = `a title ${marker} and the rest`
      // the Word page has an editor of its own, and its tabs are left alone
      expect(read(js, {}, page(editor, text))).toBe(false)
      expect(presses, app).toBe(0)
      expect(read(js, { [api]: {} }, page(editor, undefined))).toBe(false)
      expect(read(js, { [api]: {} }, page(editor, 'the text of another file'))).toBe(false)
      expect(read(js, { [api]: {} }, page(editor, text))).toEqual({ text })
    }
    // only the HTML page has its source pane behind a view tab
    expect(presses).toBe(3)
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

    // run 44418489799: the onboarding lay over the list for the whole wait
    simulate('markdown-recovery', undefined, (number, _lines, records) => {
      if (number === 2) records[0].value = false
    })
    expect(rowOf(check('markdown-recovery'), channel, 'successful-ack').evidence).toBe(
      'the button of the launch recovery list was absent or covered until the end',
    )

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
    const lost = rowOf(check('markdown-recovery'), channel, 'successful-ack').evidence
    expect(lost).toContain('does not show SEU-E2E-markdown')
    // the editor held the text before the crash: the restore is what lost it
    expect(lost).toContain('the edited page showed it before the crash, so the restore lost it')

    // run 44526047099, Windows: the typed text never reached the editor
    simulate('markdown-recovery', undefined, (number, _lines, records) => {
      const read = number === 1 ? 'markdown-before' : 'markdown-after'
      records.find((record) => record.as === read).value = 'the saved text'
    })
    expect(rowOf(check('markdown-recovery'), channel, 'successful-ack').evidence).toContain(
      'the edited page did not show it before the crash either ("the saved text"), ' +
        'so the edit never reached the document',
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

  it('fail a copy its page did not acknowledge, does not show, or whose receipt was taken again', () => {
    const name = 'markdown-saved-copy'
    const saved = 'markdown:saved-input-applied'
    const own = (lines) =>
      lines.findIndex(
        (line) => line.event === 'call' && line.channel === saved && line.request < PROBE_BASE,
      )
    const verdict = (scenario) => rowOf(check(name), saved, scenario)
    simulate(name)
    expect(verdict('successful-ack').evidence).toMatch(
      /^Markdown view \d+ acknowledged true, accepted; the page shows SEU-E2E-saved-markdown$/,
    )

    simulate(name, undefined, (_, lines) => lines.splice(own(lines), 2))
    expect(verdict('successful-ack')).toMatchObject({
      status: 'FAIL',
      evidence:
        `no accepted success acknowledgement on ${saved} (the page's calls: none); ` +
        'markdown-saved-create was answered {"pending":true}',
    })

    // the shell refused the Word page's request: no page ever had the copy, and the reason is said
    simulate(name, undefined, (_, lines, records) => {
      lines.splice(own(lines), 2)
      records.find((record) => record.as === 'markdown-saved-create').value = {
        refused: 'the Word document owner does not match the target',
      }
    })
    expect(verdict('successful-ack').evidence).toContain(
      'markdown-saved-create was answered {"refused":"the Word document owner does not match the target"}',
    )

    // the page could not apply the text and said so
    simulate(name, undefined, (_, lines) => {
      lines[own(lines)].args[0].fields.outcome = { type: 'string', length: 6, word: 'failed' }
    })
    expect(verdict('successful-ack').evidence).toMatch(/the page's calls: view \d+ false\/true/)

    // the copy's tab never became the active one: the step answered `false` until its time was up
    simulate(name, undefined, (_, _lines, records) => {
      records.find((record) => record.as === 'prep-markdown-saved-shown').value = false
    })
    expect(verdict('successful-ack').evidence).toBe(
      'the page does not show SEU-E2E-saved-markdown: false; ' +
        'markdown-saved-create was answered {"pending":true}',
    )

    simulate(name, undefined, (_, lines) => {
      const twice = lines.find((line) => line.event === 'call' && line.replayed?.[0].of === 'self')
      const reply = lines.find((line) => line.event === 'reply' && line.seq === twice.seq)
      Object.assign(reply, { ok: true, error: undefined, value: { type: 'undefined' } })
    })
    expect(verdict('duplicate-ack')).toMatchObject({
      status: 'FAIL',
      evidence: expect.stringContaining('was accepted'),
    })
    expect(verdict('foreign-renderer').status).toBe('PASS')

    // the probe went out while the Word page was still the active one
    simulate(name, undefined, (_, lines) => {
      lines.find((line) => line.replayed?.[0].of === 'self').surface = 'Docs'
    })
    expect(verdict('duplicate-ack').evidence).toMatch(/^it was sent from Docs view \d+$/)
  })

  it('fail a Save As the shell owns that left no PDF or printed nothing under its token', () => {
    const name = 'docs-owned-print'
    const print = 'docs:owned-print-pdf-buffer'
    const verdict = () => rowOf(check(name), print, 'normal-completion')
    simulate(name)
    expect(verdict().evidence).toMatch(
      /^Docs view \d+ printed 2 page groups under the token of its save; the PDF has 4096 bytes$/,
    )

    simulate(name, undefined, (_, _lines, records) => {
      const saved = records.find((record) => record.op === 'saveAsOwned')
      Object.assign(saved, { ok: false, value: undefined, error: 'no PDF appeared' })
    })
    expect(verdict()).toMatchObject({
      status: 'FAIL',
      evidence: 'the Save As the shell owns left no PDF: no PDF appeared',
    })

    // one paper only: the shell printed the whole document itself, the page asked for no group
    simulate(name, undefined, (_, lines) => lines.splice(0))
    expect(verdict().evidence).toMatch(/^view \d+ wrote the PDF without a print under the token/)

    simulate(name, undefined, (_, lines) => {
      const reply = lines.findLast((line) => line.event === 'reply')
      reply.value = object({ ok: flag(false), error: text(20) })
    })
    expect(verdict().evidence).toContain('got no pages for a page group')

    // a print without the token is the page's ordinary export, not this save's
    simulate(name, undefined, (_, lines) => {
      lines.find((line) => line.event === 'call').args.pop()
    })
    expect(verdict().status).toBe('FAIL')
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
    // Diagram's page fails only when the saved file is unreadable too: both must be damaged
    simulate('diagram-recovery')
    simulate('diagram-damaged')
    expect(check('diagram-damaged')[0].status).toBe('PASS')
    simulate('diagram-damaged', undefined, (number, _lines, _records, exit) => {
      if (number === 2) exit.damaged = exit.damaged.filter((file) => !file.includes('-autosave/'))
    })
    expect(check('diagram-damaged')[0].evidence).toBe(
      'there was nothing to damage before the run (copies)',
    )
  })

  it('count a failed restore as the damage only where the intact restore passes', () => {
    const channel = 'docs:recovery-initialized'
    const verdict = () => rowOf(check('docs-damaged'), channel, 'failed-apply')
    // the damaged restore alone: nothing shows that an intact one works
    simulate('docs-damaged')
    expect(verdict()).toMatchObject({
      status: 'FAIL',
      evidence: expect.stringContaining('docs-recovery did not pass successful-ack'),
    })
    simulate('docs-recovery')
    expect(verdict().status).toBe('PASS')
    // run 44464831843, Windows: Docs opened nothing, so its page reported a failure either way
    simulate('docs-recovery', undefined, (number, lines) => {
      if (number !== 2) return
      const own = lines.find((line) => line.event === 'call' && line.channel === channel)
      own.args[1].value = false
    })
    expect(verdict().evidence).toContain('the failure is not shown to come from the damage')
    // the control decides nothing about its own spec, and is not listed with the other's rows
    expect(check('docs-damaged')).toHaveLength(1)
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
    // each of the two specs also sends the two saved-input probes
    expect(summary(rows)).toMatch(/^6 PASS, 1 FAIL, 7 MISSING of 14 native-host scenarios$/)
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
    // a run's damage: the copies, a file of the work folder, or a list of both
    const work = path.join(root, 'work')
    fs.mkdirSync(work)
    fs.writeFileSync(path.join(work, 'map.xmind'), 'saved')
    const places = { userData: root, work }
    expect(damage(undefined, places)).toEqual([])
    expect(damage(['copies', 'map.xmind'], places).map((file) => path.basename(file))).toEqual(
      expect.arrayContaining(['abc.docx', 'id.docx', 'map.xmind']),
    )
    expect(fs.readFileSync(path.join(work, 'map.xmind'), 'utf8')).toContain('damaged')
  })

  it("write a spec's settings into the profile, with the work folder spelled in full", () => {
    seedSettings(root, undefined, 'work')
    expect(fs.existsSync(path.join(root, 'app-settings.json'))).toBe(false)
    seedSettings(root, SPECS['html-saved-copy'].settings, path.join(root, 'none', '..', 'work'))
    expect(JSON.parse(fs.readFileSync(path.join(root, 'app-settings.json'), 'utf8'))).toEqual({
      defaultSaveDir: path.join(root, 'work'),
    })
  })

  it('give a Word document a second section on another paper', async () => {
    const source = fs.readFileSync(
      new URL('../../../fixtures/generated/simple.docx', import.meta.url),
    )
    const zip = await JSZip.loadAsync(await twoPaperDocx(source))
    const xml = await zip.file('word/document.xml').async('string')
    const papers = [...xml.matchAll(/<w:pgSz w:w="(\d+)" w:h="(\d+)"/g)].map(([, w, h]) => [w, h])
    expect(papers).toEqual([
      ['11906', '16838'],
      ['16838', '11906'],
    ])
    // the first paper ends in a paragraph of its own, the second closes the body
    expect(xml).toMatch(/<w:p><w:pPr><w:sectPr>.*?<\/w:sectPr><\/w:pPr><\/w:p>/)
    expect(xml.endsWith('</w:sectPr></w:body></w:document>')).toBe(true)
    expect(Object.keys(zip.files).sort()).toEqual(
      Object.keys((await JSZip.loadAsync(source)).files).sort(),
    )
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
