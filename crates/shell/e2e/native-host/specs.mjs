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
//                   a file name overwrites that file of the work folder, a list does each
//   copies          files of the work folder made from a fixture before the first run
//   settings        fields of the profile's settings file (app-settings.json), written before the
//                   first run; `${WORK}` in a value is the work folder
//   expect          {channel, scenario, rule}: the contract scenario a rule proves
//
// The editing steps are the ones of ../scenarios/<kind>-edit.json, read from there: a recovery
// variant types what the saved-file scenario types, then crashes instead of saving.

import { readFileSync } from 'node:fs'
import { SYNTHETIC_ONLY } from '../../../../tools/shell-inventory/native-waivers.mjs'

/** The request ids of `invoke` steps start here, far above the bridge's own counter. */
export const PROBE_BASE = 4_000_000_000
/** The button of the launch recovery list (apps/shell RecoveryList.tsx); every row starts at Restore. */
const LIST_CONFIRM = '.recovery-confirm'
/** The step that presses it, by its name: the checker reads that step's record. */
export const LIST_PRESS = 'prep-list'
/** The step that says what the page showed of the button afterwards, by its name. */
export const LIST_SHOWN = 'diag-list'
/**
 * The Skip button of the first-run onboarding (apps/shell Onboarding.tsx). A new profile shows it
 * over Home, the launch recovery list included; a spec that answers the list dismisses it in its
 * first run, as a person does before anything else.
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
  const edit = save < 0 ? [] : steps.slice(ready[0] + 1, save)
  // the scenario's own read of the edited page, where it has one
  const before = edit.find((step) => step.op === 'query' && String(step.as).endsWith('-before'))
  return {
    file: steps[0].path.replace('${WORK}/', '').replace('${FIXTURES}/', ''),
    /** open the file and wait for it */
    open: steps.slice(0, ready[0] + 1),
    /** type the marker text, up to the save */
    edit,
    /** after a reopen: bring the text into view and read it */
    read: after < 0 ? [] : steps.slice(ready[1] + 1, after + 1),
    proof:
      after < 0
        ? undefined
        : { query: steps[after].as, marker: typed?.text, ...(before && { before: before.as }) },
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
/**
 * A press of a button of Home, made by the page: the element's own `click()`. Home has no editor
 * page that holds the OS focus, and the runner's native click did not reach it there (run
 * 44446306292: on macOS it only activated the window, on Windows the window was refused the
 * foreground). The press is made only while the button is what lies at its own centre, so what
 * covers it for a person covers it here; the step asks again until then and answers `false`
 * when its time is up.
 */
const press = (as, selector, timeoutMs) => ({
  op: 'query',
  as,
  timeoutMs,
  js:
    `(() => { const button = document.querySelector(${JSON.stringify(selector)}); ` +
    'if (!button || button.disabled) return false; ' +
    'const box = button.getBoundingClientRect(); ' +
    'const top = document.elementFromPoint(box.left + box.width / 2, box.top + box.height / 2); ' +
    'if (top !== button && !button.contains(top)) return false; ' +
    `button.click(); return { pressed: ${JSON.stringify(selector)} } })()`,
})
/**
 * What the page shows of that button once the press has ended, for the checker to quote when
 * the press was not made: which page answered, and whether the button was absent, disabled,
 * covered (and by what) or free. After a press that was made the list is gone, and the page
 * that answers may be the restored tab's, still loading: the step is given little time.
 */
const shown = (as, selector) => ({
  op: 'query',
  as,
  timeoutMs: 2000,
  js:
    `(() => { const button = document.querySelector(${JSON.stringify(selector)}); ` +
    "const name = (node) => !node ? 'nothing' : node.tagName + " +
    "(typeof node.className === 'string' && node.className.trim() ? '.' + node.className.trim().split(/\\s+/)[0] : ''); " +
    "let state = 'absent'; " +
    'if (button) { const box = button.getBoundingClientRect(); ' +
    'const top = document.elementFromPoint(box.left + box.width / 2, box.top + box.height / 2); ' +
    "state = button.disabled ? 'disabled' : top === button || button.contains(top) ? 'free' : 'covered by ' + name(top) } " +
    "return { page: location.href.split('?')[0], button: state } })()",
})
/**
 * The launch recovery list is answered with Restore. The list is made of the profile's recovery
 * copies: a launch without one shows no list and opens the tabs of the last session by itself.
 */
const answerList = [press(LIST_PRESS, LIST_CONFIRM, 60000), shown(LIST_SHOWN, LIST_CONFIRM)]
/** The first launch of a profile, before anything else: the onboarding is skipped. */
const firstLaunch = [press('prep-onboarding', ONBOARDING_SKIP, 30000), wait(1000)]

/**
 * What an editor's page shows once the opened document is in it, where `waitReady` cannot tell.
 * `waitReady` ends when the page is registered and its network has been quiet for a moment; the
 * HWP page asks for its editor frame after that (run 44526047099, Windows: the frame was
 * requested 3.6 s after the page had loaded, `waitReady` had ended, and the text was typed into
 * an editor that held no document). The step asks again until the page shows it.
 */
const LOADED = {
  hwp: "document.querySelector('.hwp-app')?.dataset.phase === 'ready'",
}
const loaded = (kind, as, timeoutMs) =>
  LOADED[kind] ? [{ op: 'query', as: `prep-${as}`, timeoutMs, js: LOADED[kind] }] : []

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

/**
 * The report of a saved workbook's first range, `({id, ok, error?})`: the page hands back the
 * identity the shell gave it with the workbook. It is not among the acknowledgements above: its
 * contract has no receipt probes and no scenario of another surface.
 */
const REPORT = {
  with: (id) => [{ id, ok: true }],
  path: [0, 'id'],
  verdict: (args) => flag(args[0]?.fields?.ok),
}

/** How a page's own call on `channel` carries its verdict and what it was handed. */
export const formOf = (channel) =>
  ACKS[channel] ?? (channel === 'workbook:saved-open-result' ? REPORT : undefined)

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
          ...loaded(kind, `${kind}-loaded`, ready),
          ...base.edit,
          ...probes.steps,
          ...more.first,
          wait(COPY_WAIT_MS),
        ],
      },
      {
        steps: [
          ...answerList,
          { op: 'waitReady', timeoutMs: ready },
          ...loaded(kind, `${kind}-restored`, ready),
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

/**
 * A failure a page reports counts as the failure of a damaged restore only where the same
 * restore, undamaged, succeeds on that OS: otherwise the page may fail for any other reason
 * (run 44464831843, Windows: Docs could not open any file and "passed" this way).
 */
const intact = (spec) => ({ spec, scenario: 'successful-ack' })

/**
 * The same crash, with the recovery copy overwritten before the restore. Diagram and Mindmap
 * read the copy in the shell and open the saved file instead when the copy does not parse
 * (run 44487876611: both pages acknowledged a success). Their page reports a failure only when
 * the saved file does not parse either, so `both` overwrites that too.
 */
function damagedVariant(kind, channel, { both = false } = {}) {
  const base = baseSteps(`${kind}-edit`)
  const ready = base.open.at(-1).timeoutMs
  return {
    about: `${kind}: edit, crash, restore a damaged recovery copy`,
    runs: [
      {
        end: 'kill',
        steps: [
          ...firstLaunch,
          ...base.open,
          ...loaded(kind, `${kind}-loaded`, ready),
          ...base.edit,
          wait(COPY_WAIT_MS),
        ],
      },
      {
        damage: both ? ['copies', base.file] : 'copies',
        steps: [...answerList, wait(20000)],
      },
    ],
    expect: [
      {
        channel,
        scenario: 'failed-apply',
        rule: {
          kind: 'ack',
          run: 2,
          verdict: false,
          list: true,
          control: intact(`${kind}-recovery`),
        },
      },
    ],
  }
}

/**
 * A Markdown or HTML page also acknowledges a text the shell handed it as the output of a save
 * (`<app>:saved-input-applied`). An ordinary tab was handed none: without a receipt, or with one
 * the shell never issued, it is refused.
 */
const savedInput = (app) => () => {
  const probes = tokenProbes(`${app}-saved`, `${app}:saved-input-applied`, RECEIPT)
  return { first: probes.steps, expect: probes.expect }
}

/**
 * The copies the shell writes and hands to their editor: the format the Word page asks for, the
 * text of the copy, and what the copy's page shows of it, asked until it does. The answer is
 * `false` while another page is the active one: the copy's tab becomes active only once its
 * page acknowledged the text (shell-ipc/w, finish_saved_creation). The HTML page is read in
 * its source pane, as html-edit.json reads it.
 */
const SAVED_COPIES = {
  markdown: {
    type: 'md',
    second: 'markdown.md',
    content: (marker) => `# ${marker}\n\nA copy the shell wrote.\n`,
    shown: (marker) =>
      "(() => { if (typeof window.markdownApi !== 'object') return false; " +
      "const text = document.querySelector('.ProseMirror')?.textContent ?? ''; " +
      `return text.includes(${JSON.stringify(marker)}) ? { text } : false })()`,
  },
  html: {
    type: 'html',
    second: 'html.html',
    content: (marker) =>
      '<!doctype html>\n<html>\n<head><meta charset="utf-8"><title>SeuOffice e2e</title></head>\n' +
      `<body><p>${marker}</p></body>\n</html>\n`,
    shown: (marker) =>
      "(() => { if (typeof window.htmlApi !== 'object') return false; " +
      'document.querySelectorAll(\'button.rb-view[role="tab"]\')[2]?.click(); ' +
      "const text = document.querySelector('.cm-content')?.textContent ?? ''; " +
      `return text.includes(${JSON.stringify(marker)}) ? { text } : false })()`,
  },
}

/** How long the Word page's request for a copy is waited for before the run goes on without its answer */
const ASK_WAIT_MS = 4000

/**
 * A page asks for a new document, as its AI's create_document tool does: the Word page through
 * `window.desktop.createDocument` (`docs:create-document`), the Sheets page through
 * `window.desktopApi.createDocument` (`workbook:create-document`). The answer comes once the new
 * tab is the active one, so the step waits a moment for it and no longer: it answers what the
 * shell said (`{answer}`, or `{refused}` with the reason), or `{pending: true}`.
 */
const askForCopy = (request, api = 'desktop') =>
  `(() => { const asked = window.${api}.createDocument(${JSON.stringify(request)}).then(` +
  '(answer) => ({ answer }), (error) => ({ refused: String(error?.message ?? error) })); ' +
  'const late = new Promise((done) => setTimeout(() => done({ pending: true }), ' +
  `${ASK_WAIT_MS})); return Promise.race([asked, late]) })()`

/**
 * A copy the shell wrote and handed to its editor. On the Word page's request for a Markdown or
 * HTML document the shell writes the file as a save it owns, into the default save folder, and
 * opens it from the bytes it kept. The page of the new tab acknowledges that text; the same
 * receipt again, and from the page of another file, is refused.
 */
function savedCopyVariant(app) {
  const channel = `${app}:saved-input-applied`
  const form = ACKS[channel]
  const copy = SAVED_COPIES[app]
  const marker = `SEU-E2E-saved-${app}`
  const asked = `${app}-saved-create`
  const shown = `prep-${app}-saved-shown`
  const request = { type: copy.type, title: `seu-e2e-saved-${app}`, content: copy.content(marker) }
  return {
    about: `${app}: the Word page has a copy written, the shell hands it to its editor`,
    settings: { defaultSaveDir: '${WORK}' },
    runs: [
      {
        steps: [
          ...baseSteps('docs-edit').open,
          wait(1500),
          { op: 'query', as: asked, timeoutMs: ASK_WAIT_MS + 10000, js: askForCopy(request) },
          { op: 'query', as: shown, timeoutMs: 90000, js: copy.shown(marker) },
          wait(1500),
          invoke(`${app}-saved-twice`, channel, form.with(replay('self', ...form.path))),
          ...openFile(copy.second),
          invoke(`${app}-saved-other-renderer`, channel, form.with(replay('other', ...form.path))),
        ],
      },
    ],
    expect: [
      {
        channel,
        scenario: 'successful-ack',
        rule: { kind: 'ack', run: 1, verdict: true, asked, proof: { query: shown, marker } },
      },
      {
        channel,
        scenario: 'duplicate-ack',
        rule: { kind: 'replayRefused', run: 1, probe: `${app}-saved-twice`, of: 'self' },
      },
      {
        channel,
        scenario: 'foreign-renderer',
        rule: { kind: 'replayRefused', run: 1, probe: `${app}-saved-other-renderer`, of: 'other' },
      },
    ],
  }
}

/**
 * A workbook the shell wrote and opened as a saved document. On the Sheets page's request for an
 * `xlsx` the shell writes the file as a save it owns, into the default save folder, and opens it
 * in a new tab from the bytes it kept. That opening ends only when the page of the new tab has
 * read its first range and reports it (`workbook:saved-open-result`) with the identity the shell
 * handed it in the workbook; until then the tab is not shown. So the tab that shows the
 * workbook, and its accepted report, are the opening having finished.
 *
 * The contract of the report has no scenario of a success. What this proves of it is its
 * `no-opening`, where an opening was: the same report again, and from the page of another
 * workbook, finds no opening and is refused. Before the shell handed the identity over (and
 * before it told a saved tab's page that a workbook was waiting), no page ever reported, the
 * tab never showed, and the request failed after six minutes.
 */
function savedWorkbookCopy() {
  const channel = 'workbook:saved-open-result'
  const form = formOf(channel)
  const base = baseSteps('sheets-edit')
  const second = 'sheets-b.xlsx'
  const marker = 'SEU-E2E-saved-sheets'
  const asked = 'sheets-saved-create'
  const shown = 'prep-sheets-saved-shown'
  const twice = 'sheets-saved-twice'
  const other = 'sheets-saved-other-renderer'
  const request = {
    type: 'xlsx',
    title: 'seu-e2e-saved-sheets',
    content: `${marker},1\n`,
    sheetName: 'Sheet1',
  }
  // the first cell of the active tab's workbook, through the page's own control hook: only the
  // copy has the text, and the tab the request came from stays the active one until it opens
  const shows =
    "(async () => { const control = window.__seuofficeControl; if (typeof control !== 'function') " +
    "return false; try { await control({ cmd: 'goto', target: { kind: 'range', range: 'A1' } }); " +
    "const text = JSON.stringify((await control({ cmd: 'selection' })) ?? null); " +
    `return text.includes(${JSON.stringify(marker)}) ? { text } : false } catch { return false } })()`
  return {
    about: 'sheets: the Sheets page has a workbook written, the shell opens it as a saved document',
    settings: { defaultSaveDir: '${WORK}' },
    copies: { [second]: base.file },
    runs: [
      {
        steps: [
          ...base.open,
          wait(1500),
          {
            op: 'query',
            as: asked,
            timeoutMs: ASK_WAIT_MS + 10000,
            js: askForCopy(request, 'desktopApi'),
          },
          { op: 'query', as: shown, timeoutMs: 90000, js: shows },
          wait(1500),
          invoke(twice, channel, form.with(replay('self', ...form.path))),
          ...openFile(second),
          invoke(other, channel, form.with(replay('other', ...form.path))),
        ],
      },
    ],
    expect: [
      {
        channel,
        scenario: 'no-opening',
        rule: { kind: 'settledOnce', run: 1, asked, proof: { query: shown, marker }, probe: twice },
      },
      {
        channel,
        scenario: 'no-opening',
        rule: { kind: 'replayRefused', run: 1, probe: other, of: 'other' },
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

/**
 * The PDF of a Save As the shell owns. The Word page prints a document of more than one paper
 * size group by group itself (apps/docs file-actions.ts, printGroupsMerged), under the token of
 * the save when the shell owns it. No menu, key or page call makes the shell ask a Word page
 * for a Save As it owns (the menu's Save As is the page's own save), so the runner asks
 * (`saveAsOwned`); the dialog of the page's Save As takes the PDF path, and the rest is the
 * page's own save.
 */
function ownedPrintCopy() {
  const channel = 'docs:owned-print-pdf-buffer'
  return {
    about: 'docs: a Save As the shell owns writes the PDF of a document on two papers',
    runs: [
      {
        steps: [
          ...openFile('docs-papers.docx'),
          { op: 'saveAsOwned', path: '${WORK}/docs-owned.pdf', timeoutMs: 120000 },
        ],
      },
    ],
    expect: [{ channel, scenario: 'normal-completion', rule: { kind: 'printed', run: 1 } }],
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
      { end: 'kill', steps: [...openFile(file), ...probes.steps, wait(3000)] },
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
      { end: 'kill', steps: [...openFile(file), wait(3000)] },
      { damage: file, steps: [wait(30000)] },
    ],
    expect: [
      {
        channel,
        scenario,
        rule: { kind: 'ack', run: 2, verdict: false, control: intact(`${kind}-restore`) },
      },
    ],
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
  'sheets-saved-copy': savedWorkbookCopy(),
  'docs-recovery': recoveryVariant('docs', {
    channel: 'docs:recovery-initialized',
    second: 'docs-b.docx',
    extra: ownedPrint,
  }),
  'docs-owned-print': ownedPrintCopy(),
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
    extra: savedInput('markdown'),
  }),
  'html-recovery': recoveryVariant('html', {
    channel: 'html:recovery-applied',
    second: 'html-b.html',
    extra: savedInput('html'),
  }),
  'markdown-saved-copy': savedCopyVariant('markdown'),
  'html-saved-copy': savedCopyVariant('html'),
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
  'diagram-damaged': damagedVariant('diagram', 'diagram:recovery-initialized', { both: true }),
  'mindmap-damaged': damagedVariant('mindmap', 'mindmap:recovery-initialized', { both: true }),
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
 * Required scenarios no spec drives, with the reason: the waiver of the native-host proof
 * (tools/shell-inventory/native-waivers.mjs), which is where they are listed. The checker writes
 * no receipt for them and the IPC comparison asks only their synthetic proof; the checker's test
 * fails when this list and `expect` together stop matching the contracts.
 */
export const UNDRIVEN = SYNTHETIC_ONLY
