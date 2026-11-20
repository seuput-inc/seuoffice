#!/usr/bin/env node
// Decides the real-app evidence of the native-only IPC channels and writes its receipts.
//
// e2e/native-host/run.mjs ran each spec of e2e/native-host/specs.mjs on the real app; the app
// wrote what its dispatcher saw of those channels to <run>/out/native-ipc.jsonl
// (src/ipc/native_record.rs) and decided nothing. This reads the records beside the steps that
// caused them and prints one line per expectation of a spec:
//
//   PASS     the record shows what the contract scenario asks for
//   FAIL     the scenario was driven and the record shows something else, or nothing
//   MISSING  the spec was asked for and did not run
//
// and writes <root>/receipts.json: `{os, head_sha, receipts, results}`. A receipt is
// `{channel, kind: 'native-host', status, scenarios, artifact}` as the IPC comparison of
// `seuoffice rust` takes it (tools/ipc-diff, `--native-evidence`), plus the OS and the spec;
// `artifact` is the native-ipc.jsonl of the run that decided it, relative to <root>. A scenario
// that failed gets a `failed` receipt; a spec that did not run gets none, which leaves its
// scenarios pending. Receipts are per OS: tools/ipc-diff/native-host-evidence.mjs passes a
// scenario only when the macOS and the Windows receipt both do.
//
// Exit status: 1 when anything FAILs or is MISSING, 0 otherwise.
//
//   node native-host-check.mjs --root <dir> --os <macOS|Windows> --sha <commit> [--only a,b]
//                              [--markdown out.md]

import fs from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'
import {
  LIST_PRESS,
  LIST_SHOWN,
  PROBE_BASE,
  SPECS,
  formOf,
  selected,
  surfaceOf,
} from '../e2e/native-host/specs.mjs'

// ---------------------------------------------------------------------------------- the records

/** Lines of a .jsonl file; a line that is not JSON (a write cut by the kill) is left out. */
export function parseJsonl(text) {
  return text
    .split('\n')
    .filter((line) => line.trim().length > 0)
    .flatMap((line) => {
      try {
        return [JSON.parse(line)]
      } catch {
        return []
      }
    })
}

/** The calls of a native-ipc.jsonl, each with its reply when one was recorded. */
export function joinCalls(lines) {
  const replies = new Map(
    lines.filter((line) => line.event === 'reply').map((line) => [line.seq, line]),
  )
  return lines
    .filter((line) => line.event === 'call')
    .map((call) => ({ ...call, reply: replies.get(call.seq) }))
}

const readIf = (file) => (fs.existsSync(file) ? fs.readFileSync(file, 'utf8') : undefined)

/** What one run of a spec left; `undefined` when the run never started. */
export function loadRun(root, name, number) {
  const dir = path.join(root, name, `run-${number}`)
  const exit = readIf(path.join(dir, 'exit.json'))
  if (exit === undefined) return undefined
  return {
    number,
    artifact: `${name}/run-${number}/out/native-ipc.jsonl`,
    exit: JSON.parse(exit),
    steps: JSON.parse(readIf(path.join(dir, 'scenario.json')) ?? '{"steps":[]}').steps,
    records: parseJsonl(readIf(path.join(dir, 'out', 'e2e.jsonl')) ?? ''),
    calls: joinCalls(parseJsonl(readIf(path.join(dir, 'out', 'native-ipc.jsonl')) ?? '')),
  }
}

// ------------------------------------------------------------------------------------ the rules

const pass = (evidence) => ({ status: 'PASS', evidence })
const fail = (evidence) => ({ status: 'FAIL', evidence })

const clip = (text, max = 160) => (text.length > max ? `${text.slice(0, max - 3)}...` : text)
const where = (call) => `${call.surface} view ${call.view}`
const own = (call, channel) =>
  call.channel === channel && call.request < PROBE_BASE && call.surface === surfaceOf(channel)
/** The answer that hands a restored document over: its path and its receipt */
const isSeed = (reply) =>
  reply?.ok === true &&
  reply.value?.type === 'object' &&
  reply.value.fields?.token?.type === 'string' &&
  reply.value.fields?.path?.type === 'string'
/** A print of the Word page under a save's token that was answered with the pages */
const isPrinted = (call) =>
  call.args?.[3]?.type === 'string' &&
  call.reply?.ok === true &&
  call.reply.value?.fields?.ok?.value === true &&
  call.reply.value.fields.base64?.length > 0

/** The record of the first step that matches, with its step */
function stepRecord(run, matches) {
  const index = run.steps.findIndex(matches)
  if (index < 0) return undefined
  return run.records.find((record) => record.step === index)
}

/** The view each `waitReady` step of the run found ready, in order; undefined where it found none */
const readyViews = (run) =>
  run.records
    .filter((record) => record.op === 'waitReady')
    .map((record) => (record.ok ? record.value?.view : undefined))

/** The call an `invoke` step sent, by the request id its query answered with */
function probeCall(run, name) {
  const record = stepRecord(run, (step) => step.op === 'query' && step.as === name)
  if (!record) return { problem: `the step ${name} left no record` }
  if (!record.ok) return { problem: `the step ${name} failed: ${record.error}` }
  const request = record.value?.request
  const call = run.calls.find((candidate) => candidate.request === request)
  if (!call) return { problem: `the call of ${name} (request ${request}) is not on record` }
  if (!call.reply) return { problem: `the call of ${name} was never answered` }
  return { call }
}

/** Whether the proof query of an acknowledged document shows the text the page was handed */
function proofHolds(value, marker) {
  if (value && typeof value === 'object' && typeof value.count === 'number') return value.count > 0
  return JSON.stringify(value ?? null).includes(marker)
}

/**
 * Where a restored page lacks the text, what the edited page of the first run showed before the
 * crash, when its scenario read it. A text that was never in the editor is the runner's input or
 * the editor's loading; a text the editor held and the restored page lacks is the recovery.
 */
function lostWhere(first, { before, marker }) {
  if (!before) return ''
  const edited = stepRecord(first, (step) => step.op === 'query' && step.as === before)
  if (edited?.ok && proofHolds(edited.value, marker))
    return '; the edited page showed it before the crash, so the restore lost it'
  const shown = clip(JSON.stringify(edited?.value ?? edited?.error ?? null), 80)
  return `; the edited page did not show it before the crash either (${shown}), so the edit never reached the document`
}

/**
 * What in the first run kept the edit from reaching a document, as its records show it: a step
 * that failed, a document that was not ready when `waitReady` gave up, a `prep-` query that
 * stayed false, an edited page that did not show the typed text. Empty when they show nothing.
 */
function editTrouble(first) {
  const marker = first.steps.find(
    (step) => step.op === 'type' && String(step.text).startsWith('SEU-E2E-'),
  )?.text
  const found = first.records.flatMap((record) => {
    const step = first.steps[record.step]
    const name = step?.as ?? record.op
    if (!record.ok) return [`${name} failed (${clip(String(record.error), 80)})`]
    if (record.op === 'waitReady' && record.value?.control === 'not_ready')
      return ['the document was not ready when waitReady gave up']
    if (record.op !== 'query') return []
    if (name.startsWith('prep-') && record.value === false) return [`${name} stayed false`]
    if (marker && name.endsWith('-before') && !proofHolds(record.value, marker))
      return [`${name} did not show ${marker} (${clip(JSON.stringify(record.value ?? null), 60)})`]
    return []
  })
  return found.length ? `; in the first run ${found.join(', ')}` : ''
}

/**
 * Why the launch recovery list was not answered. The list is made of the profile's recovery
 * copies, so a profile without one shows none: then the first run is where to look, not Home
 * (validation round S121, Windows: the Docs page of the first run never opened its document,
 * nothing was typed, and no copy was written). With a copy there, the button itself was not to
 * be had, and what the page showed of it afterwards is quoted.
 */
function listUnanswered(run, first) {
  const { copies } = run.exit
  if (copies?.length === 0)
    return (
      'the launch had no recovery list to answer: the profile held no recovery copy ' +
      `when the run began${editTrouble(first)}`
    )
  const seen = stepRecord(run, (step) => step.op === 'query' && step.as === LIST_SHOWN)?.value
  const held = copies
    ? `the profile held ${copies.length} recovery cop${copies.length === 1 ? 'y' : 'ies'}`
    : ''
  const after = seen?.button ? `afterwards ${seen.page} showed the button ${seen.button}` : ''
  const detail = [held, after].filter(Boolean).join('; ')
  return (
    'the button of the launch recovery list was absent or covered until the end' +
    (detail ? ` (${detail})` : '')
  )
}

/**
 * Where a page asked the shell for the document it then failed to acknowledge or show, what
 * that request was answered: a refusal of the request is why no page ever had the document.
 */
function askedFor(run, { asked }) {
  if (!asked) return ''
  const record = stepRecord(run, (step) => step.op === 'query' && step.as === asked)
  return `; ${asked} was answered ${clip(JSON.stringify(record?.value ?? record?.error ?? null))}`
}

const RULES = {
  /** A page of the channel's own surface sent it and was refused by the handler. */
  refused(run, rule, channel) {
    const { call, problem } = probeCall(run, rule.probe)
    if (problem) return fail(problem)
    if (call.surface !== surfaceOf(channel)) return fail(`it was sent from ${where(call)}`)
    if (call.reply.ok) return fail(`${where(call)} was answered without an error`)
    if (rule.error && !call.reply.error.includes(rule.error))
      return fail(`${where(call)} was refused with another text: ${clip(call.reply.error)}`)
    return pass(`${where(call)} refused: ${clip(call.reply.error)}`)
  },

  /** A page of another surface sent it and the dispatcher refused it there. */
  refusedSurface(run, rule, channel) {
    const { call, problem } = probeCall(run, rule.probe)
    if (problem) return fail(problem)
    if (call.surface === surfaceOf(channel))
      return fail(`it was sent from the channel's own surface (${where(call)})`)
    if (call.reply.ok) return fail(`${where(call)} was answered without an error`)
    if (!call.reply.error.includes(`not open to ${call.surface}`))
      return fail(`${where(call)} was refused with another text: ${clip(call.reply.error)}`)
    return pass(`${where(call)} refused: ${clip(call.reply.error)}`)
  },

  /** The page presented a receipt of an answered call again (its own, or another view's). */
  replayRefused(run, rule, channel) {
    const { call, problem } = probeCall(run, rule.probe)
    if (problem) return fail(problem)
    if (call.reply.unresolvedReplay) return fail(`no receipt to present: ${call.reply.error}`)
    const [replayed] = call.replayed ?? []
    if (!replayed || replayed.of !== rule.of) return fail('the call presented no recorded receipt')
    if ((replayed.view === call.view) !== (rule.of === 'self'))
      return fail(`${where(call)} presented the receipt of view ${replayed.view}`)
    if (call.surface !== surfaceOf(channel)) return fail(`it was sent from ${where(call)}`)
    if (call.reply.ok)
      return fail(`${where(call)} presented view ${replayed.view}'s receipt and was accepted`)
    return pass(
      `${where(call)} presented view ${replayed.view}'s answered receipt, refused: ` +
        clip(call.reply.error),
    )
  },

  /** The app's own page acknowledged with this verdict and the shell accepted it. */
  ack(run, rule, channel, spec, runs) {
    const on = rule.on ?? channel
    if (rule.list) {
      const press = stepRecord(run, (s) => s.op === 'query' && s.as === LIST_PRESS)
      if (!press?.ok)
        return fail(`the launch recovery list was not answered: ${press?.error ?? 'no record'}`)
      // the step presses the button only when nothing lies over it, and says `false` otherwise
      if (!press.value?.pressed) return fail(listUnanswered(run, runs[0]))
    }
    // every part of the damage the spec names was there to damage
    const untouched = [spec.runs[run.number - 1].damage ?? []]
      .flat()
      .find(
        (entry) =>
          !run.exit.damaged.some((file) =>
            entry === 'copies' ? file.includes('-autosave/') : file.endsWith(`/${entry}`),
          ),
      )
    if (untouched) return fail(`there was nothing to damage before the run (${untouched})`)
    let view
    if (rule.view) {
      view = readyViews(run)[rule.view - 1]
      if (view === undefined) return fail(`tab ${rule.view} of the run was never ready`)
    }
    const acknowledged = run.calls.find(
      (call) =>
        own(call, on) &&
        (view === undefined || call.view === view) &&
        formOf(on).verdict(call.args) === rule.verdict &&
        call.reply?.ok === true,
    )
    if (!acknowledged) {
      const seen = run.calls
        .filter((call) => own(call, on))
        .map((call) => `view ${call.view} ${formOf(on).verdict(call.args)}/${call.reply?.ok}`)
      return fail(
        `no accepted ${rule.verdict ? 'success' : 'failure'} acknowledgement on ${on}` +
          `${view === undefined ? '' : ` from view ${view}`} (the page's calls: ${seen.join(', ') || 'none'})` +
          askedFor(run, rule),
      )
    }
    if (rule.after) {
      const handed = run.calls.find(
        (call) =>
          own(call, rule.after) && call.view === acknowledged.view && call.seq < acknowledged.seq,
      )
      if (!isSeed(handed?.reply)) return fail(`no document was handed over on ${rule.after} first`)
    }
    let proof = ''
    if (rule.proof) {
      const { query, marker } = rule.proof
      const read = stepRecord(run, (step) => step.op === 'query' && step.as === query)
      if (!read?.ok || !marker || !proofHolds(read.value, marker))
        return fail(
          `the page does not show ${marker}: ${clip(JSON.stringify(read?.value ?? read?.error ?? null))}` +
            lostWhere(runs[0], rule.proof) +
            askedFor(run, rule),
        )
      proof = `; the page shows ${marker}`
    }
    return pass(`${where(acknowledged)} acknowledged ${rule.verdict}, accepted${proof}`)
  },

  /**
   * The page's own report settled what it reported on, and the same report again finds nothing
   * left to settle. The first half is `ack` with a success, the second `replayRefused` of the
   * page's own receipt; the row fails with the text of the half that failed.
   */
  settledOnce(run, rule, channel, spec, runs) {
    const settled = RULES.ack(run, { ...rule, verdict: true }, channel, spec, runs)
    if (settled.status !== 'PASS') return settled
    const again = RULES.replayRefused(run, { ...rule, of: 'self' }, channel)
    return again.status === 'PASS' ? pass(`${settled.evidence}; then ${again.evidence}`) : again
  },

  /** The app's own call from a tab was answered with nothing, or with a restored document. */
  reply(run, rule, channel) {
    const view = readyViews(run)[rule.view - 1]
    if (view === undefined) return fail(`tab ${rule.view} of the run was never ready`)
    const call = run.calls.find((candidate) => own(candidate, channel) && candidate.view === view)
    if (!call?.reply) return fail(`view ${view} has no answered call of its own`)
    const shown = call.reply.ok ? call.reply.value?.type : `error ${clip(call.reply.error)}`
    const matches =
      rule.value === 'seed'
        ? isSeed(call.reply)
        : call.reply.ok && call.reply.value?.type === rule.value
    return matches
      ? pass(`${where(call)} was answered ${rule.value}`)
      : fail(`${where(call)} was answered ${shown}, not ${rule.value}`)
  },

  /** A second request from the page did not hand the document over again. */
  probeNotSeed(run, rule, channel) {
    const { call, problem } = probeCall(run, rule.probe)
    if (problem) return fail(problem)
    if (call.surface !== surfaceOf(channel)) return fail(`it was sent from ${where(call)}`)
    if (isSeed(call.reply)) return fail(`${where(call)} was handed the document a second time`)
    return pass(
      `${where(call)} got ${call.reply.ok ? call.reply.value?.type : clip(call.reply.error)}`,
    )
  },

  /**
   * A Save As the shell owns left its PDF where the dialog said, and the page that wrote it
   * printed every page group under the token of that save.
   */
  printed(run, _rule, channel) {
    const saved = stepRecord(run, (step) => step.op === 'saveAsOwned')
    if (!saved?.ok)
      return fail(`the Save As the shell owns left no PDF: ${saved?.error ?? 'no record'}`)
    const { view, bytes } = saved.value
    const parts = run.calls.filter((call) => own(call, channel) && call.view === view)
    if (parts.length === 0)
      return fail(`view ${view} wrote the PDF without a print under the token of its save`)
    const lost = parts.find((call) => !isPrinted(call))
    if (lost)
      return fail(
        `${where(lost)} got no pages for a page group: ` +
          clip(lost.reply?.ok === false ? lost.reply.error : JSON.stringify(lost.reply ?? null)),
      )
    return pass(
      `${where(parts[0])} printed ${parts.length} page groups under the token of its save; ` +
        `the PDF has ${bytes} bytes`,
    )
  },
}

// ------------------------------------------------------------------------------------- the run

/** One row per expectation of a spec, in the order of its `expect` */
function checkSpec(root, name, spec) {
  const runs = spec.runs.map((_, index) => loadRun(root, name, index + 1))
  return spec.expect.map(({ channel, scenario, rule }) => {
    const run = runs[rule.run - 1]
    const row = { spec: name, channel, scenario }
    if (!runs[0]) return { ...row, status: 'MISSING', evidence: 'the spec was not run' }
    if (!run) {
      const before = runs[rule.run - 2]?.exit
      return {
        ...row,
        ...fail(
          `run ${rule.run} never started (run ${rule.run - 1} ended by ${before?.ended ?? 'nothing'}, not ${before?.wanted ?? 'at all'})`,
        ),
        artifact: runs.findLast(Boolean).artifact,
      }
    }
    return { ...row, ...RULES[rule.kind](run, rule, channel, spec, runs), artifact: run.artifact }
  })
}

/**
 * One row per expectation of each named spec. A rule with a `control` passes only where that
 * other spec's scenario on the same channel passes too, whether or not that spec was named.
 */
export function runChecks({ root, names, specs = SPECS }) {
  const decided = new Map()
  const rowsOf = (name) => {
    if (!decided.has(name)) decided.set(name, checkSpec(root, name, specs[name]))
    return decided.get(name)
  }
  return names.flatMap((name) =>
    rowsOf(name).map((row, index) => {
      const { control } = specs[name].expect[index].rule
      if (!control || row.status !== 'PASS') return row
      const other = rowsOf(control.spec).find(
        (candidate) => candidate.channel === row.channel && candidate.scenario === control.scenario,
      )
      if (other?.status === 'PASS') return row
      return {
        ...row,
        ...fail(
          `${row.evidence}, but ${control.spec} did not pass ${control.scenario} ` +
            `(${other?.evidence ?? 'no such expectation'}): the failure is not shown to come from the damage`,
        ),
      }
    }),
  )
}

/** The receipts of the rows: per spec, channel, verdict and deciding run */
export function receiptsOf(rows, os) {
  const receipts = new Map()
  for (const row of rows) {
    if (row.status === 'MISSING') continue
    const status = row.status === 'PASS' ? 'passed' : 'failed'
    const key = [row.spec, row.channel, status, row.artifact].join('\n')
    const receipt = receipts.get(key) ?? {
      channel: row.channel,
      kind: 'native-host',
      status,
      scenarios: [],
      artifact: row.artifact,
      os,
      spec: row.spec,
    }
    if (!receipt.scenarios.includes(row.scenario)) receipt.scenarios.push(row.scenario)
    receipts.set(key, receipt)
  }
  return [...receipts.values()]
}

export const formatRow = (row) =>
  `${row.status.padEnd(8)} ${row.spec.padEnd(18)} ${row.channel.padEnd(30)} ${row.scenario} | ${row.evidence}`

export function summary(rows) {
  const count = (status) => rows.filter((row) => row.status === status).length
  return `${count('PASS')} PASS, ${count('FAIL')} FAIL, ${count('MISSING')} MISSING of ${rows.length} native-host scenarios`
}

export function markdown(rows) {
  const cell = (text) => text.replaceAll('|', '\\|').replaceAll('\n', ' ')
  return [
    summary(rows),
    '',
    '| status | spec | channel | scenario | evidence |',
    '| --- | --- | --- | --- | --- |',
    ...rows.map(
      (row) =>
        `| ${row.status} | ${row.spec} | ${row.channel} | ${row.scenario} | ${cell(row.evidence)} |`,
    ),
  ].join('\n')
}

export const exitCodeOf = (rows) => (rows.every((row) => row.status === 'PASS') ? 0 : 1)

function main(argv) {
  const option = (name) => {
    const at = argv.indexOf(name)
    return at >= 0 ? argv[at + 1] : undefined
  }
  const [root, os, sha] = [option('--root'), option('--os'), option('--sha')]
  if (!root || !os || !sha) {
    console.error(
      'usage: native-host-check.mjs --root <dir> --os <macOS|Windows> --sha <commit> [--only a,b] [--markdown out.md]',
    )
    return 64
  }
  const rows = runChecks({ root, names: selected(option('--only')) })
  for (const row of rows) console.log(formatRow(row))
  console.log(summary(rows))
  fs.mkdirSync(root, { recursive: true })
  const report = { os, head_sha: sha, receipts: receiptsOf(rows, os), results: rows }
  fs.writeFileSync(path.join(root, 'receipts.json'), `${JSON.stringify(report, null, 2)}\n`)
  if (option('--markdown')) fs.writeFileSync(option('--markdown'), `${markdown(rows)}\n`)
  return exitCodeOf(rows)
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  process.exitCode = main(process.argv.slice(2))
}
