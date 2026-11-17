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
