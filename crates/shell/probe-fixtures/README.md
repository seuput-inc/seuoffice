# probe fixtures

Mock pages for the hosted WebView probes P1-P10 (`crates/shell/src/probe/`, the
`gate=probe` job of `.github/workflows/seuoffice-shell.yml`). No customer
documents.

- `pages/` is copied by the workflow into the folder the shell's app scheme
  serves: `<stage>/<host>/out/renderer/probe/` for the hosts `shell`, `docs`,
  `sheets` and `hwp`, and `crates/shell/out/engine-host/probe/` for the engine
  host. `SEUOFFICE_APPS_DIR=<stage>` points the shell at it. The pages are
  then loaded as `genoffice-app://<host>/probe/<page>`.
- `pages/probe.js` is the one helper every page includes. It reports raw
  records to the `probe-sink` scheme the shell registers in probe mode.
- `p4-*.html` is the print corpus: a Docs-like page, a Sheets-like page with
  `@page`, and a five-page range page, with Hangul text, backgrounds and
  explicit page breaks so the expected page count does not depend on fonts.
- `po-*.html` are the pages of the page-operation probes (P5 on macOS, P6 on
  Windows); `p8-*` and `p9-*` the context menu and popup probes.
- `p3-context.html` and `p3-*.png/json/wasm` routes are also embedded in the
  shell (`src/probe/schemes.rs`) and served by the five stand-in schemes.

## Running

On GitHub, from a branch that has them:

    gh workflow run "seuoffice shell" --ref <branch> -f gate=probe
    gh workflow run "seuoffice shell" --ref <branch> -f gate=probe -f probes=p1,p2,p9

The `probe` job builds the shell on `macos-latest` and `windows-latest`, runs
each probe with its own timeout, runs `ci/probe-check.mjs`, and uploads
`probes-macOS` / `probes-Windows` (the JSON lines, the PDFs and PNGs, the shell
log of each run, `report.json`, `report.md`, `report.txt`) whatever the checker
says. p5 runs on macOS only, p6 on Windows only.

To read a downloaded artifact again, or to run a probe by hand on a built
shell (`SEUOFFICE_APPS_DIR` as the workflow stages it):

    SEUOFFICE_APPS_DIR=<stage> SEUOFFICE_SHELL_PARTIAL=1 SeuOffice --probe p4 --out <dir>
    node crates/shell/ci/probe-check.mjs <root> [--probes p4] [--p10-ratio 5]

`<root>` holds one folder per probe (`<root>/p4/p4.jsonl`, artifacts and
`shell.log` beside it; `<root>/p10/dialog.json` for the measurement). The
checker exits 1 on a violated assertion, 2 when none is violated but one is
unobserved, 0 otherwise.
