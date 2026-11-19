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
  ACKS,
  LIST_CONFIRM,
  PROBE_BASE,
  SPECS,
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

/** Whether the proof query of a restored document shows the text that was never saved */
function proofHolds(value, marker) {
  if (value && typeof value === 'object' && typeof value.count === 'number') return value.count > 0
  return JSON.stringify(value ?? null).includes(marker)
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
  ack(run, rule, channel, spec) {
    const on = rule.on ?? channel
    if (rule.list) {
      const click = stepRecord(run, (s) => s.op === 'click' && s.selector === LIST_CONFIRM)
      if (!click?.ok)
        return fail(`the launch recovery list was not answered: ${click?.error ?? 'no record'}`)
      // the page says what is at the clicked point: something over the list takes the click
      if (!String(click.value?.hit).startsWith('BUTTON'))
        return fail(`the click on the launch recovery list landed on ${click.value?.hit}`)
    }
    if (spec.runs[run.number - 1].damage && run.exit.damaged.length === 0)
      return fail('there was nothing to damage before the run')
    let view
    if (rule.view) {
      view = readyViews(run)[rule.view - 1]
      if (view === undefined) return fail(`tab ${rule.view} of the run was never ready`)
    }
    const acknowledged = run.calls.find(
      (call) =>
        own(call, on) &&
        (view === undefined || call.view === view) &&
        ACKS[on].verdict(call.args) === rule.verdict &&
        call.reply?.ok === true,
    )
    if (!acknowledged) {
      const seen = run.calls
        .filter((call) => own(call, on))
        .map((call) => `view ${call.view} ${ACKS[on].verdict(call.args)}/${call.reply?.ok}`)
      return fail(
        `no accepted ${rule.verdict ? 'success' : 'failure'} acknowledgement on ${on}` +
          `${view === undefined ? '' : ` from view ${view}`} (the page's calls: ${seen.join(', ') || 'none'})`,
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
          `the restored page does not show ${marker}: ${clip(JSON.stringify(read?.value ?? read?.error ?? null))}`,
        )
      proof = `; the page shows ${marker}`
    }
    return pass(`${where(acknowledged)} acknowledged ${rule.verdict}, accepted${proof}`)
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

  /** Every call the page made from a tab was answered without an error, and it made one. */
  completed(run, rule, channel) {
    const view = readyViews(run)[rule.view - 1]
    if (view === undefined) return fail(`tab ${rule.view} of the run was never ready`)
    const calls = run.calls.filter((call) => own(call, channel) && call.view === view)
    if (calls.length === 0) return fail(`view ${view} made no call of its own`)
    const failed = calls.find((call) => call.reply?.ok !== true)
    if (failed)
      return fail(
        `${where(failed)} was answered ${failed.reply ? clip(failed.reply.error) : 'nothing'}`,
      )
    return pass(
      `${where(calls[0])}: ${calls.length} call(s) answered ${calls[0].reply.value?.type}`,
    )
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
}

// ------------------------------------------------------------------------------------- the run

/** One row per expectation of each named spec */
export function runChecks({ root, names, specs = SPECS }) {
  const rows = []
  for (const name of names) {
    const spec = specs[name]
    const runs = spec.runs.map((_, index) => loadRun(root, name, index + 1))
    for (const { channel, scenario, rule } of spec.expect) {
      const run = runs[rule.run - 1]
      const row = { spec: name, channel, scenario }
      if (!runs[0]) rows.push({ ...row, status: 'MISSING', evidence: 'the spec was not run' })
      else if (!run) {
        const before = runs[rule.run - 2]?.exit
        rows.push({
          ...row,
          ...fail(
            `run ${rule.run} never started (run ${rule.run - 1} ended by ${before?.ended ?? 'nothing'}, not ${before?.wanted ?? 'at all'})`,
          ),
          artifact: runs.findLast(Boolean).artifact,
        })
      } else {
        rows.push({ ...row, ...RULES[rule.kind](run, rule, channel, spec), artifact: run.artifact })
      }
    }
  }
  return rows
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
