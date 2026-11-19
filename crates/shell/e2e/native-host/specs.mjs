// The real-app scenarios of the native-only IPC channels (tools/shell-inventory/native-channels.ts)
// and what each of them must show. run.mjs runs them, ../../ci/native-host-check.mjs reads what
// they left and writes the `native-host` receipts of the `seuoffice rust` IPC comparison.
//
// A spec is one or more runs of the app on one profile and one work folder:
//
//   runs[n].steps   the steps of a `--e2e` scenario, plus `{op: 'invoke', as, channel, args}`: the
//                   active page sends that call through its own bridge (run.mjs writes it as a
//                   `query`). `args` is the wire form; `{e2eReplay: 'self' | 'other', path}` stands
//                   for a value of an answered call, in a build with the `e2e-replay` feature
//                   (crates/shell/src/ipc/native_record/replay.rs).
//   runs[n].end     'kill': the app is killed after the last step, as a crash leaves it
//   runs[n].damage  before the run: 'copies' overwrites the recovery copies the app wrote,
//                   a file name overwrites that file of the work folder
//   copies          files of the work folder made from a fixture before the first run
//   expect          {channel, scenario, rule}: the contract scenario a rule proves
//
// The editing steps are the ones of ../scenarios/<kind>-edit.json, read from there: a recovery
// variant types what the saved-file scenario types, then crashes instead of saving.

import { readFileSync } from 'node:fs'

/** The request ids of `invoke` steps start here, far above the bridge's own counter. */
export const PROBE_BASE = 4_000_000_000
/** The button of the launch recovery list (apps/shell RecoveryList.tsx); every row starts at Restore. */
export const LIST_CONFIRM = '.recovery-confirm'
/**
 * The Skip button of the first-run onboarding (apps/shell Onboarding.tsx). A new profile shows it
 * over Home, the launch recovery list included; a spec that comes back to Home dismisses it in
 * its first run, as a person does before anything else.
 */
const ONBOARDING_SKIP = '.onb-skip'
/** How long after an edit the app is killed: every editor writes its recovery copy each 30 s. */
const COPY_WAIT_MS = 45000
const UNKNOWN = 'seu-e2e-unknown-receipt'

/** The surface a native-only channel is open to, as the record names it. */
export const surfaceOf = (channel) => {
  const app = channel.startsWith('workbook:') ? 'sheets' : channel.split(':')[0]
  return app[0].toUpperCase() + app.slice(1)
}

/** The steps of a saved-file scenario, cut where a recovery variant needs them. */
export function baseSteps(name) {
  const { steps } = JSON.parse(
    readFileSync(new URL(`../scenarios/${name}.json`, import.meta.url), 'utf8'),
  )
  const ready = steps.flatMap((step, index) => (step.op === 'waitReady' ? [index] : []))
  const save = steps.findIndex((step) => step.op === 'shortcut' && step.name === 'save')
  if (steps[0]?.op !== 'open' || ready.length === 0)
    throw new Error(`${name}: the scenario does not start by opening a file`)
  const after =
    ready.length > 1
      ? steps.findIndex(
          (step, index) =>
            index > ready[1] && step.op === 'query' && String(step.as).endsWith('-after'),
        )
      : -1
  const typed = steps.find((step) => step.op === 'type' && step.text.startsWith('SEU-E2E-'))
  return {
    file: steps[0].path.replace('${WORK}/', '').replace('${FIXTURES}/', ''),
    /** open the file and wait for it */
    open: steps.slice(0, ready[0] + 1),
    /** type the marker text, up to the save */
    edit: save < 0 ? [] : steps.slice(ready[0] + 1, save),
    /** after a reopen: bring the text into view and read it */
    read: after < 0 ? [] : steps.slice(ready[1] + 1, after + 1),
    proof: after < 0 ? undefined : { query: steps[after].as, marker: typed?.text },
  }
}

const invoke = (as, channel, args) => ({ op: 'invoke', as, channel, args })
const wait = (ms) => ({ op: 'wait', ms })
const openFile = (file, timeoutMs = 60000) => [
  { op: 'open', path: `\${WORK}/${file}` },
  { op: 'waitReady', timeoutMs },
  wait(1000),
]
const replay = (of, ...path) => ({ e2eReplay: of, path })
/** The first launch of a profile, before anything else: the onboarding is skipped. */
const firstLaunch = [{ op: 'click', selector: ONBOARDING_SKIP, timeoutMs: 30000 }, wait(1000)]

/** A recorded flag (`{type: 'boolean', value}`), or undefined when the argument is something else. */
const flag = (shape) => (shape?.type === 'boolean' ? shape.value : undefined)

/**
 * How each acknowledgement carries its receipt and its verdict: the arguments without a receipt
 * and with one, where the receipt is in them, and the verdict as the record shows it.
 */
const FORMS = {
  /** `(token, ok, error?)` */
  positional: {
    missing: () => [],
    with: (token) => [token, true],
    path: [0],
    verdict: (args) => flag(args[1]),
  },
  /** `(token, {ok, error?})` */
  hwp: {
    missing: () => [],
    with: (token) => [token, { ok: true }],
    path: [0],
    verdict: (args) => flag(args[1]?.fields?.ok),
  },
  /** `({token, ok, error?})` */
  result: {
    missing: () => [{ ok: true }],
    with: (token) => [{ token, ok: true }],
    path: [0, 'token'],
    verdict: (args) => flag(args[0]?.fields?.ok),
  },
  /** `({token, outcome, error?})`: `applied` and `recovered` are successes, `failed` is not */
  outcome: {
    missing: () => [{ outcome: 'applied' }],
    with: (token) => [{ token, outcome: 'applied' }],
    path: [0, 'token'],
    verdict: (args) => {
      const word = args[0]?.fields?.outcome?.word
      return word === undefined ? undefined : word !== 'failed'
    },
  },
}

/** The acknowledgement channels: the form of each, by channel. */
export const ACKS = {
  'docs:recovery-initialized': FORMS.positional,
  'slides:recovery-initialized': FORMS.positional,
  'diagram:recovery-initialized': FORMS.positional,
  'mindmap:recovery-initialized': FORMS.positional,
  'epub:recovery-initialized': FORMS.positional,
  'pdf:recovery-initialized': FORMS.positional,
  'hwp:recovery-initialized': FORMS.hwp,
  'html:recovery-applied': FORMS.result,
  'markdown:recovery-applied': FORMS.result,
  'html:saved-input-applied': FORMS.outcome,
  'markdown:saved-input-applied': FORMS.outcome,
}

const refused = (run, probe, error) => ({ kind: 'refused', run, probe, ...(error && { error }) })

/** The probes one page can send on its own acknowledgement channel without a receipt of its own. */
function tokenProbes(id, channel, names) {
  const form = ACKS[channel]
  return {
    steps: [
      invoke(`${id}-missing`, channel, form.missing()),
      invoke(`${id}-unknown`, channel, form.with(UNKNOWN)),
    ],
    expect: [
      { channel, scenario: names.missing, rule: refused(1, `${id}-missing`) },
      { channel, scenario: names.foreign, rule: refused(1, `${id}-unknown`) },
    ],
  }
}

const RECEIPT = { missing: 'missing-receipt', foreign: 'foreign-receipt' }
const TOKEN = { missing: 'missing-token', foreign: 'foreign-token' }

/**
 * An editor that keeps recovery copies: the document is edited and the app killed; the next run
 * answers the launch list with Restore, and the restored page acknowledges its copy.
 */
function recoveryVariant(kind, { channel, second, extra = () => ({}) }) {
  const base = baseSteps(`${kind}-edit`)
  const form = ACKS[channel]
  const probes = tokenProbes(kind, channel, RECEIPT)
  const more = { first: [], expect: [], ...extra(base) }
  const ready = base.open.at(-1).timeoutMs
  return {
    about: `${kind}: edit, crash, restore the recovery copy`,
    copies: { [second]: base.file },
    runs: [
      {
        end: 'kill',
        steps: [
          ...firstLaunch,
          ...base.open,
          ...base.edit,
          ...probes.steps,
          ...more.first,
          wait(COPY_WAIT_MS),
        ],
      },
      {
        steps: [
          { op: 'click', selector: LIST_CONFIRM, timeoutMs: 60000 },
          { op: 'waitReady', timeoutMs: ready },
          ...base.read,
          invoke(`${kind}-twice`, channel, form.with(replay('self', ...form.path))),
          ...openFile(second, ready),
          invoke(`${kind}-other-renderer`, channel, form.with(replay('other', ...form.path))),
        ],
      },
    ],
    expect: [
      ...probes.expect,
      {
        channel,
        scenario: 'successful-ack',
        rule: { kind: 'ack', run: 2, view: 1, verdict: true, list: true, proof: base.proof },
      },
      {
        channel,
        scenario: 'duplicate-ack',
        rule: { kind: 'replayRefused', run: 2, probe: `${kind}-twice`, of: 'self' },
      },
      {
        channel,
        scenario: 'foreign-renderer',
        rule: { kind: 'replayRefused', run: 2, probe: `${kind}-other-renderer`, of: 'other' },
      },
      ...more.expect,
    ],
  }
}

/** The same crash, with the recovery copy overwritten before the restore. */
function damagedVariant(kind, channel) {
  const base = baseSteps(`${kind}-edit`)
  return {
    about: `${kind}: edit, crash, restore a damaged recovery copy`,
    runs: [
      { end: 'kill', steps: [...firstLaunch, ...base.open, ...base.edit, wait(COPY_WAIT_MS)] },
      {
        damage: 'copies',
        steps: [{ op: 'click', selector: LIST_CONFIRM, timeoutMs: 60000 }, wait(20000)],
      },
    ],
    expect: [
      {
        channel,
        scenario: 'failed-apply',
        rule: { kind: 'ack', run: 2, verdict: false, list: true },
      },
    ],
  }
}

/**
 * Docs' Save As to another format writes a copy under the authority of the save itself: the
 * menu's Save As is a save the shell owns. A Markdown or HTML copy is published and opened in
 * its own editor, whose page acknowledges the text it was handed (`<app>:saved-input-applied`).
 */
function savedCopy(app, ext, other) {
  const channel = `${app}:saved-input-applied`
  const form = ACKS[channel]
  const base = baseSteps('docs-edit')
  const copy = `docs-copy.${ext}`
  const probes = tokenProbes(`${app}-saved`, channel, RECEIPT)
  return {
    about: `docs: Save As a ${ext} copy, which opens in the ${app} editor`,
    runs: [
      {
        steps: [
          ...base.open,
          ...base.edit,
          { op: 'shortcut', name: 'saveAs' },
          { op: 'dialog', answer: `path:\${WORK}/${copy}`, timeoutMs: 30000 },
          wait(8000),
          ...openFile(copy),
          ...probes.steps,
          invoke(`${app}-saved-twice`, channel, form.with(replay('self', ...form.path))),
          ...openFile(other),
          invoke(`${app}-saved-other`, channel, form.with(replay('other', ...form.path))),
        ],
      },
    ],
    expect: [
      { channel, scenario: 'successful-ack', rule: { kind: 'ack', run: 1, verdict: true } },
      ...probes.expect,
      {
        channel,
        scenario: 'duplicate-ack',
        rule: { kind: 'replayRefused', run: 1, probe: `${app}-saved-twice`, of: 'self' },
      },
      {
        channel,
        scenario: 'foreign-renderer',
        rule: { kind: 'replayRefused', run: 1, probe: `${app}-saved-other`, of: 'other' },
      },
    ],
  }
}

/** The same Save As to a PDF copy: the page prints it with the token of that save. */
function savedPdfCopy() {
  const base = baseSteps('docs-edit')
  return {
    about: 'docs: Save As a PDF copy, printed under the save',
    runs: [
      {
        steps: [
          ...base.open,
          ...base.edit,
          { op: 'shortcut', name: 'saveAs' },
          { op: 'dialog', answer: 'path:${WORK}/docs-copy.pdf', timeoutMs: 30000 },
          wait(15000),
        ],
      },
    ],
    expect: [
      {
        channel: 'docs:owned-print-pdf-buffer',
        scenario: 'normal-completion',
        rule: { kind: 'completed', run: 1, view: 1 },
      },
    ],
  }
}

/** Docs also answers the PDF of a closing save: without its pending save token it refuses. */
const ownedPrint = () => {
  const channel = 'docs:owned-print-pdf-buffer'
  return {
    first: [
      invoke('docs-print-missing', channel, [11906, 16838, 1]),
      invoke('docs-print-unknown', channel, [11906, 16838, 1, UNKNOWN]),
    ],
    expect: [
      { channel, scenario: 'missing-token', rule: refused(1, 'docs-print-missing') },
      { channel, scenario: 'foreign-token', rule: refused(1, 'docs-print-unknown') },
    ],
  }
}

/** A work file under its own name, unless the fixture already has that name. */
const fixtureCopy = (file, fixture) => (file === fixture ? {} : { [file]: fixture })

/**
 * A viewer without recovery copies (PDF as opened here, EPUB): after a crash its tab comes back
 * by itself, and the page acknowledges the receipt of that restored open.
 */
function restoredVariant(kind, file, { fixture, channel, names, extra = () => ({}) }) {
  const form = ACKS[channel]
  const second = `${kind}-b.${file.split('.').pop()}`
  const probes = tokenProbes(kind, channel, names)
  const more = { second: [], expect: [], ...extra() }
  return {
    about: `${kind}: open, crash, the tab comes back`,
    copies: { ...fixtureCopy(file, fixture), [second]: fixture },
    runs: [
      { end: 'kill', steps: [...firstLaunch, ...openFile(file), ...probes.steps, wait(3000)] },
      {
        steps: [
          { op: 'waitReady', timeoutMs: 90000 },
          wait(1500),
          ...more.second,
          invoke(`${kind}-twice`, channel, form.with(replay('self', ...form.path))),
          ...openFile(second),
          invoke(`${kind}-other-renderer`, channel, form.with(replay('other', ...form.path))),
        ],
      },
    ],
    expect: [
      ...probes.expect,
      {
        channel,
        scenario: 'successful-ack',
        rule: { kind: 'ack', run: 2, view: 1, verdict: true },
      },
      {
        channel,
        scenario: 'duplicate-ack',
        rule: { kind: 'replayRefused', run: 2, probe: `${kind}-twice`, of: 'self' },
      },
      {
        channel,
        scenario: 'foreign-renderer',
        rule: { kind: 'replayRefused', run: 2, probe: `${kind}-other-renderer`, of: 'other' },
      },
      ...more.expect,
    ],
  }
}

/** The restored open of a file that no longer parses: the page reports the failure. */
function unreadableVariant(kind, file, { fixture, channel, scenario }) {
  return {
    about: `${kind}: open, crash, the tab comes back on a damaged file`,
    copies: fixtureCopy(file, fixture),
    runs: [
      { end: 'kill', steps: [...firstLaunch, ...openFile(file), wait(3000)] },
      { damage: file, steps: [wait(30000)] },
    ],
    expect: [{ channel, scenario, rule: { kind: 'ack', run: 2, verdict: false } }],
  }
}

/** PDF hands its restored document over on a second channel, which the page consumes once. */
const pdfConsume = () => {
  const channel = 'pdf:consume-recovery-pending'
  const applied = {
    kind: 'ack',
    run: 2,
    view: 1,
    verdict: true,
    on: 'pdf:recovery-initialized',
    after: channel,
  }
  return {
    second: [invoke('pdf-consume-twice', channel, [])],
    expect: [
      {
        channel,
        scenario: 'ordinary-empty',
        rule: { kind: 'reply', run: 1, view: 1, value: 'null' },
      },
      {
        channel,
        scenario: 'captured-consume',
        rule: { kind: 'reply', run: 2, view: 1, value: 'seed' },
      },
      {
        channel,
        scenario: 'duplicate-consume',
        rule: { kind: 'probeNotSeed', run: 2, probe: 'pdf-consume-twice' },
      },
      {
        channel,
        scenario: 'foreign-renderer',
        rule: { kind: 'reply', run: 2, view: 2, value: 'null' },
      },
      { channel, scenario: 'renderer-apply', rule: applied },
      { channel: 'pdf:recovery-initialized', scenario: 'renderer-apply', rule: applied },
    ],
  }
}

/**
 * Every channel whose contract asks for it, sent from a page of another surface: the dispatcher
 * refuses it there. (The workbook report's contract has no such scenario.)
 */
function surfaces() {
  const fromMarkdown = [
    'docs:owned-print-pdf-buffer',
    'pdf:consume-recovery-pending',
    ...Object.keys(ACKS).filter((channel) => !channel.startsWith('markdown:')),
  ]
  const fromHtml = Object.keys(ACKS).filter((channel) => channel.startsWith('markdown:'))
  const argsOf = (channel) => ACKS[channel]?.with(UNKNOWN) ?? []
  const probe = (channel) => `surface-${channel.replace(':', '-')}`
  return {
    about: 'the native-only channels from a page of another surface',
    runs: [
      {
        steps: [
          ...openFile('markdown.md'),
          ...fromMarkdown.map((channel) => invoke(probe(channel), channel, argsOf(channel))),
          ...openFile('html.html'),
          ...fromHtml.map((channel) => invoke(probe(channel), channel, argsOf(channel))),
        ],
      },
    ],
    expect: [...fromMarkdown, ...fromHtml].map((channel) => ({
      channel,
      scenario: 'unauthorized-surface',
      rule: { kind: 'refusedSurface', run: 1, probe: probe(channel) },
    })),
  }
}

/** The report of a saved workbook's first range, malformed or with no opening to report. */
function savedWorkbook() {
  const channel = 'workbook:saved-open-result'
  const id = 'seu-e2e-load'
  const cases = [
    ['missing-identity', [{ ok: true }], 'has no identity'],
    ['missing-result', [{ id, ok: 'yes' }], 'has no result'],
    ['inconsistent-error', [{ id, ok: true, error: 'late' }], 'is invalid'],
    ['no-opening', [{ id, ok: true }], 'opening'],
  ]
  return {
    about: 'sheets: malformed and unmatched reports of a saved workbook opening',
    runs: [
      {
        steps: [
          ...baseSteps('sheets-edit').open,
          wait(1500),
          ...cases.map(([scenario, args]) => invoke(`sheets-${scenario}`, channel, args)),
        ],
      },
    ],
    expect: cases.map(([scenario, , error]) => ({
      channel,
      scenario,
      rule: refused(1, `sheets-${scenario}`, error),
    })),
  }
}

export const SPECS = {
  surfaces: surfaces(),
  'sheets-saved-open': savedWorkbook(),
  'docs-recovery': recoveryVariant('docs', {
    channel: 'docs:recovery-initialized',
    second: 'docs-b.docx',
    extra: ownedPrint,
  }),
  'slides-recovery': recoveryVariant('slides', {
    channel: 'slides:recovery-initialized',
    second: 'slides-b.pptx',
  }),
  'diagram-recovery': recoveryVariant('diagram', {
    channel: 'diagram:recovery-initialized',
    second: 'diagram-b.drawio',
  }),
  'mindmap-recovery': recoveryVariant('mindmap', {
    channel: 'mindmap:recovery-initialized',
    second: 'mindmap-b.xmind',
  }),
  'hwp-recovery': recoveryVariant('hwp', {
    channel: 'hwp:recovery-initialized',
    second: 'hwp-b.hwp',
  }),
  'markdown-recovery': recoveryVariant('markdown', {
    channel: 'markdown:recovery-applied',
    second: 'markdown-b.md',
  }),
  'html-recovery': recoveryVariant('html', {
    channel: 'html:recovery-applied',
    second: 'html-b.html',
  }),
  'docs-copy-pdf': savedPdfCopy(),
  'docs-copy-markdown': savedCopy('markdown', 'md', 'markdown.md'),
  'docs-copy-html': savedCopy('html', 'html', 'html.html'),
  'pdf-restore': restoredVariant('pdf', 'pdf.pdf', {
    fixture: 'multipage.pdf',
    channel: 'pdf:recovery-initialized',
    names: TOKEN,
    extra: pdfConsume,
  }),
  'epub-restore': restoredVariant('epub', 'epub.epub', {
    fixture: 'epub.epub',
    channel: 'epub:recovery-initialized',
    names: RECEIPT,
  }),
  'docs-damaged': damagedVariant('docs', 'docs:recovery-initialized'),
  'slides-damaged': damagedVariant('slides', 'slides:recovery-initialized'),
  'diagram-damaged': damagedVariant('diagram', 'diagram:recovery-initialized'),
  'mindmap-damaged': damagedVariant('mindmap', 'mindmap:recovery-initialized'),
  'hwp-damaged': damagedVariant('hwp', 'hwp:recovery-initialized'),
  'pdf-damaged': unreadableVariant('pdf', 'pdf.pdf', {
    fixture: 'multipage.pdf',
    channel: 'pdf:recovery-initialized',
    scenario: 'parse-failure',
  }),
  'epub-damaged': unreadableVariant('epub', 'epub.epub', {
    fixture: 'epub.epub',
    channel: 'epub:recovery-initialized',
    scenario: 'failed-apply',
  }),
}

/**
 * The specs `--only` picks, in the table's order. It is the workflow's `scenarios` input, shared
 * with the saved-file scenarios: `all` and `native-host` mean every spec, other names pick
 * the specs so named, and a list of scenario names alone picks none.
 */
export function selected(only, names = Object.keys(SPECS)) {
  const wanted = (only ?? 'all')
    .split(',')
    .map((name) => name.trim())
    .filter(Boolean)
  const every = wanted.includes('all') || wanted.includes('native-host')
  return every ? names : names.filter((name) => wanted.includes(name))
}

/**
 * Required scenarios no spec drives yet, with the reason. The checker writes no receipt for
 * them, so the IPC comparison keeps them pending; its test fails when this list and `expect`
 * together stop matching the contracts.
 */
export const UNDRIVEN = {
  'docs:owned-print-pdf-buffer': {
    'owner-loss': 'needs the tab closed while the copy is being printed',
    'effect-failure': 'needs the print of the page itself to fail',
  },
  'pdf:consume-recovery-pending': {
    'preparation-loss': 'needs the restored tab revoked while its document is being prepared',
  },
  'pdf:recovery-initialized': {
    'revision-loss':
      'needs the document revision replaced between the hand-over and the acknowledgement',
  },
  'workbook:saved-open-result': {
    'missing-native-authority':
      'a page of the real app cannot send a call without its native context',
    'closed-tab': 'a closed tab has no page left to send the report from',
  },
  'html:recovery-applied': { 'failed-apply': 'no text makes the editor fail to apply a copy' },
  'html:saved-input-applied': { 'failed-apply': 'no text makes the editor fail to apply a file' },
  'markdown:recovery-applied': { 'failed-apply': 'no text makes the editor fail to apply a copy' },
  'markdown:saved-input-applied': {
    'failed-apply': 'no text makes the editor fail to apply a file',
  },
}
