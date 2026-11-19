# Real-app tests (gate G3)

`gate=e2e` of `.github/workflows/seuoffice-shell.yml` runs the real app, built as the full gate
builds it plus every renderer bundle, through the scenarios in `scenarios/` on the GitHub-hosted
macOS and Windows runners, then decides the result with `ci/e2e-check.mjs`. It never runs on a
person's machine, installed app, profile or keychain: the job refuses a runner that is not
GitHub-hosted, every folder is below `RUNNER_TEMP`, and the app itself refuses `--e2e` unless
`SEUOFFICE_USER_DATA` holds the `.seuoffice-e2e-profile` marker the workflow creates.

## Layout

| path                  | what                                                                                     |
| --------------------- | ---------------------------------------------------------------------------------------- |
| `scenarios/*.json`    | one scenario each: `{"deadlineMs"?, "steps": [...]}`, the ops of the `--e2e` contract    |
| `make-fixtures.mjs`   | fixture folder: small repo documents copied, plus the big workbook, 5-page PDF, mind map |
| `render-scenario.mjs` | replaces `${FIXTURES}` and `${WORK}` with forward-slash paths before the app runs it     |
| `stage-resources.mjs` | the installer's resources that this runner builds, as a `--config` (macOS) or a copy     |
| `../ci/e2e-check.mjs` | the checker; `e2e-disk.mjs` reads saved files back; `e2e-check.test.mjs` tests them      |
| `native-host/`        | the native-only IPC channels in the real app: `specs.mjs` (the table), `run.mjs`         |

Per scenario the workflow makes `<root>/<name>/{scenario.json, out/, work/, exit-code.txt,
run.log}`; the checker reads exactly those. Every scenario edits its own copy of the fixtures in
`work/`, never the pristine set.

## Conventions the scenarios rely on

- `type.text`: `\n` is Return, `\t` Tab, `\u001b` Escape, `\b` Backspace; anything else is typed.
- `query` expressions named `prep-*` only move the view (focus, `__seuofficeControl` goto, a view
  tab click) so the next native key lands where a user would have put the caret. They change no
  document. Every other `query` is a read the checker asserts on.
- Reads, per editor: docs and markdown `.ProseMirror` text; sheets and PDF
  `window.__seuofficeControl` (`selection` / `goto`, the CLI's hook); slides the text editor
  overlay `.slide-text-editor` after Tab and Enter; HTML the CodeMirror source pane; mind map the
  `me-tpc` topics; HWP the studio frame's `ieum-agent` plugin `findText`. The draw.io editor is a
  cross-origin frame, so its label is only checked on disk.
- Saved files are checked on disk twice: a plain zip/XML/OLE reader in `ci/e2e-disk.mjs`, and the
  repo's own parser (`@seuoffice/file-parse` through the wasm engine, run under tsx). Disagreement
  is a FAIL; a parser that cannot run is named in the evidence.

## Co-editing

The desktop app has no co-editing. The editors reach a session only through `window.seuCollab`,
which `packages/web-platform/src/collab-page.ts` installs for the web build; no source of
`apps/shell`, `crates/shell` or any app's `main`/`preload` mentions it (the checker re-scans this
on every run and FAILs if that changes, asking for a desktop scenario). The evidence for the item
is the web tests: `apps/web/tests/collab-*.spec.ts`, `packages/collab/tests`,
`packages/embed/tests/collab*.test.ts` and `packages/web-platform/tests/collab-*.test.ts`, run by
the `web` job of `.github/workflows/seuoffice.yml`.

## Adding a scenario

Add `scenarios/<name>.json`, the fixture it opens to `make-fixtures.mjs`, and its checks to
`CHECKS` in `ci/e2e-check.mjs`. The checker test fails when a scenario has no checks or a check
names a query the scenario does not have. Run the checker tests with
`cd crates/shell/ci && npx vitest run`.

## Native-host IPC evidence

The 14 native-only IPC channels (`tools/shell-inventory/native-channels.ts`) have no Electron
handler to compare with, so the IPC comparison of `seuoffice rust` asks for evidence from the real
app. The same `gate=e2e` job produces it after the scenarios:

- With `--e2e` the dispatcher writes every call of those channels to `<out>/native-ipc.jsonl`
  (`src/ipc/native_record.rs`): who sent it, the shape of its arguments, the answer or refusal.
  A text is written as its length only, so no receipt or token is in the file.
- `native-host/specs.mjs` is the table of specs: runs of the app and what each must show. The
  editing steps are read from `scenarios/<kind>-edit.json`. Two things a spec has that a scenario
  has not: an `invoke` step (the active page sends a call through its own document bridge;
  `run.mjs` writes it as a `query`), and `end: 'kill'` (`run.mjs` kills the app from outside once
  the last step is on record, so the next run of the spec meets what a crash leaves).
- `native-host/run.mjs` runs each spec into `<root>/native-host/<name>/run-<n>/`, on one throwaway
  profile per spec. `../ci/native-host-check.mjs` reads those folders and writes
  `<root>/native-host/receipts.json`, the receipts of this OS and commit; they are uploaded with the
  test records, and `seuoffice rust` takes them through its `native_host_run` input.

The workflow's `scenarios` input picks specs too: by name, or `native-host` for all of them. A
required scenario no spec drives is listed with its reason in `UNDRIVEN` of `specs.mjs`; the checker
test fails when the specs and that list stop matching the contracts.

The specs present another view's receipt and the same receipt twice through replay markers
(`{"e2eReplay": ...}` in an `invoke` step's arguments, `src/ipc/native_record/replay.rs`). That
code is compiled only with the cargo feature `e2e-replay`, which only the two builds of this job
turn on; the job checks that its executable has the marker (`ci/replay-guard.sh present`). A
default build hands a marker to the handler as an ordinary object, and the bundle job fails when
an executable it built or installed has the marker's bytes (`ci/replay-guard.sh absent`).

The restore runs answer the launch recovery list with a `click` on its confirm button, on Home (the
runner uses the shell window's own view there). A new profile shows the first-run onboarding over
Home, the list included, so the first run of such a spec starts by clicking its Skip button, and
the checker fails a list click that landed on anything but a button.
