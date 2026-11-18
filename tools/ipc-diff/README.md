# ipc-diff

The parity gate of stage 3 (the stage 3 shell contract under `docs/seuoffice`, section 8): each IPC
channel's Rust handler (`crates/shell-ipc/<batch>`, run by `crates/ipc-probe` through the shell's
own dispatch in `crates/shell-core`) against the TypeScript `ipcMain` handler it replaces, run on
`tools/main-diff`'s fake Electron.

The source-loaded handlers use the document engines too. On a hosted runner, prepare the
pinned RHWP source with `node tools/build-rhwp-core.mjs --source-only`, then run
`npm run build:wasm` before this gate or its fault probes. The gate starts that real WASM
through `@seuoffice/engine-wasm/node` before loading any group. `SEUOFFICE_WASM` can name
the exact prepared build. Native builds and comparisons stay on hosted runners.

```sh
npm --prefix tools/ipc-diff run gate                       # every group, each in a process of its own
npm --prefix tools/ipc-diff run gate -- --group w/quick-look --show 20
npm --prefix tools/ipc-diff run gate -- --channel quick-look:set-settings
npm --prefix tools/ipc-diff run gate -- --require W        # and the coverage of batch W
node tools/ipc-diff/faults.mjs tools/ipc-diff/faults/w.json
```

## A group

`groups/<batch>/<name>.ts` (the batch folder in lower case) exports `group: Group` (`types.ts`):

- `batch`: `'W'`, `'D1'`, ...
- `load(cx)`: registers the TypeScript handlers for one case. Install what the module needs on
  `cx.electron` (it already has `ipcMain`, `webContents`, `BrowserWindow`, `app`, `dialog`, `shell`,
  `clipboard`, `nativeImage`; add names with `Object.assign`), then load the module that calls
  `ipcMain.handle/on`. `cx.fresh(path)` loads a module afresh for the case, so module state
  (`let deps`, `ipcRegistered`) starts over; `path` is relative to the repository. Configure the
  module as the app's entry (`*-main`, `shell/index`) configures it, with the same file paths under
  `cx.dir`.
- `cases`: each case runs on a fresh world on both sides.
- `single`: `{channel: why}` for a channel whose one answer is all there is.

### A case

```ts
{
  note: 'why the case is there',
  views: [{ view: 1, window: 1, surface: 'Shell' }],   // the default
  files: { 'userData/app-settings.json': '{"lang":"ko"}' },  // main-diff's FileSpec
  hosts: { shared: {...}, w: {...}, d2: {...} },          // what the fake hosts answer
  calls: [{ channel: 'quick-look:set-settings', kind: 'invoke', view: 1, args: [{ keepInTray: true }] }],
}
```

- `surface` is a `Surface` of `crates/shell-contract` (`Shell`, `Docs`, `ShellDocPicker`, ...). A
  call from a surface the channel's spec does not list is refused by the shell; the TypeScript side
  answers the same refusal.
- `args` are written in the wire form (`crates/shell/bridge/wire.ts`): plain JSON, plus `{"$":"u"}`
  for `undefined`, `{"$":"b","k":"u8","i":0}` for bytes (with `buffers: ['00ff']` as hex),
  `{"$":"d","v":ms}` for a date.
- `$DIR` in any string is the case's folder. `app.getPath(name)` is `$DIR/<name>` on both sides
  (`userData`, `temp`, `documents`, ...).

### A trigger

Some events no handler emits: a native menu item's click, a right-click. A `trigger` call runs such
a thing on both sides and compares what it emitted:

```ts
{ channel: 'm/menu:docs:File > Save', kind: 'trigger', name: 'm/menu:docs:File > Save', view: 1, args: [] }
```

- `name` is `<batch>/<what>` (batch in lower case); `args` is plain JSON. `channel` repeats the
  name; a trigger is left out of the coverage of channels, and the events it emits count for the
  coverage of events.
- TypeScript: `Group.triggers[name](cx, view, args)` runs after `load` (`cx` is the load context
  plus `contents(view)`). What it resolves to is the answer, `{value, buffers}`; a throw is
  `{error: String(err)}`, without Electron's remote-method sentence. A name the group lacks answers
  `{thrown: "unknown trigger <name>"}`.
- Rust: a batch crate exports, behind its `probe` feature next to `probe_hosts`,

  ```rust
  pub fn probe_trigger(name: &str, view: ViewId, args: &serde_json::Value,
      services: &Services, provided: &Rc<Provided>, then: Then<IpcValue>) -> bool
  ```

  (false: not its name), and calls `then` with the outcome. ipc-probe runs it on the core thread
  through one `match` arm on the name's prefix in `run_case`; the batch that adds its export adds
  its arm. Today only `m`. The answer is `{value, buffers}` or `{error: String(err)}`, as above.

- The fakes a trigger needs: `Menu` (`buildFromTemplate`, `popup` recorded as the last popped menu,
  `setApplicationMenu`, `getApplicationMenu`), `app.on('web-contents-created')` replayed for each
  view's contents after `load`, and `webContents.on/once/emit`.

### Deferred invokes and triggers

A handler or a trigger that waits for another call of the renderer (a close check answered by
`close-check-result`, a save request answered by its ack) is compared on its full path: give the
invoke (or the trigger) `defer: true`, then the renderer's answering call, then
`{ channel: '', args: [], await: <index of the deferred call> }`. The deferred call's own answer is
`{deferred: true}`; the await step's answer is the invoke's reply (`{timeout: true}` after 20 s);
every step records its events as a call does.

When the renderer's reply must echo a request identity, use `eventArg(step, channel, view, arg,
occurrence)` from `event-refs.ts` as an argument in the later call. Each side captures its own
actual emitted wire value and supplies it to its handler; a missing event or argument fails the
case. Only that referenced identity is normalized in the comparison, including its type. Use a
call's `capture: [eventArg(...)]` when an emitted identity must be compared but is deliberately
not acknowledged, such as a timeout or destroyed view. Keep all other event fields and stale or
wrong-view replies in the comparison.
`nativeEventArg(...)` (same parameters) names a correlation token that only the Rust shell adds, as
the last argument of an event the Electron main sends without one (the recovery flush requests of
the editors: the shell's flush replies name the request they answer, so a reply without its token
settles nothing). The Rust side must emit it and its reply carries it; the Electron side emits no
argument and its reply carries `undefined`. The token is left out of the compared event on both
sides, so the rest of that event is still compared; a token that is not the event's last argument
fails the case.
Set `expectAnswer` on a call or await step when parity alone cannot prove correctness. The gate
requires that exact wire-form answer on each side, so matching incorrect successes still fail.

### A side that never finishes

A handler that waits for an event that never comes holds the whole step, so each side of a case has
60 s (`CASE_TIMEOUT_MS` in `main.ts`). A side still running after that is a hang, not a pass: the
gate prints a `HANG` line that names the side, the group, its batch and the case, records the case
as `HANG` among the differences, and ends the group at that case. A hung Rust side carries the last
of ipc-probe's stderr, and the probe is killed. A full run prints `GROUP <name> start` before each
group (a group's own output reaches the log when it ends) and kills a group process that has run for
10 minutes (`GROUP_TIMEOUT_MS`), the bound for a hang outside any case.

### What is compared

For every call: the answer (`{value, buffers}`, `{error}` with Electron's
`Error invoking remote method '<ch>': ...` sentence, `{}` or `{thrown}` for a send), and the events
it emitted (`{view, channel, args, buffers}`, one per view reached, in order). Then the host log of
the case and the folder after the last call (kind, mode, contents). Each side's folder is written
`$DIR` (in lower case too, as a case-folding path key writes it); the temporary file of an atomic
write is written `.$PID.$UUID.tmp`.

### Coverage (`--require <batch>`)

Every Electron-derived invoke, send and sendSync channel of the batch is called by some case;
every one called with arguments gave two distinct answers (or is in `single`); every
Electron-derived event channel of the batch was emitted by some call.

### Native-only contracts

`tools/shell-inventory/native-channels.ts` is the explicit native-only overlay. Its rows are
generated as `native_spec` in the same Rust `CHANNELS` table as the original `spec` rows.
Both remain subject to actual handler registration, required-batch completeness and permitted
surface checks. The namespace does not grant access.

`docs:owned-print-pdf-buffer` belongs to D2/Docs and resolves an existing closing-export save
request. The original `docs:print-pdf-buffer` stays in M and in the Electron comparison set.
The native route has no Electron oracle. A differential group that calls it is rejected rather
than supplied a fake TypeScript handler or a coverage waiver.

A full gate writes `native-contracts.json` separately from the unchanged Electron group reports.
Missing receipts are **pending**, including when regression source exists. Synthetic behavior
does not prove native ownership, navigation, platform effects or the installed ABI. The full
gate stays non-green until every declared native contract has separate successful synthetic
and actual native-host evidence for all required scenarios. `--group` remains a scoped
Electron comparison, not an overall native-contract verdict.

`--native-evidence <file>` accepts a JSON array of current executed-evidence receipts:

```json
[
  {
    "channel": "docs:owned-print-pdf-buffer",
    "kind": "synthetic",
    "status": "passed",
    "scenarios": ["missing-token", "foreign-token"],
    "artifact": "path/to/downloaded-hosted-regression-log"
  }
]
```

This illustrates the receipt format, not executed evidence. `kind` is `synthetic` or
`native-host`; `status` is `passed` or `failed`. Artifacts must exist relative to the repository
(absolute paths are also accepted). Receipts name executed behavior, not source inspection.
The gate validates receipt metadata and required scenario coverage; it does not independently
attest an artifact's contents. Evidence producers and reviewers own that provenance. A failed
receipt remains a failure; supply receipts for the current run rather than an accumulated history.

The native-host receipts come from the real app: the `gate=e2e` job of `seuoffice shell` drives the
channels (`crates/shell/e2e/native-host`) and `crates/shell/ci/native-host-check.mjs` writes one
`receipts.json` per OS. `native-host-evidence.ts` (`npm run native-host`) takes the two downloaded
artifacts of one such run into the evidence file: it refuses receipts of another commit or OS,
passes a scenario only where the macOS and the Windows receipt both pass it, and keeps every failed
receipt. `seuoffice rust` runs it when its `native_host_run` input names that run.

## Hosts in the probe

`crates/ipc-probe` builds a fresh `Services` per case: a real core thread, a UI thread, workers it
waits for, the case's views, `CasePaths` under the folder, an `EventSink` that records, and `Hosts`:

- **Shared hosts** (W, `crates/ipc-probe/src/fakes.rs`, mirrored by `fakes.ts`): `DialogsHost`,
  `OsShellHost`, `ClipboardHost`, `ContentsHost`, `MainWindowHost`, and the process's one
  `Arc<Mutex<SharedRecoveryState>>`. They answer from `hosts.shared`:

  | key               | shape                                                                                                                                                |
  | ----------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------- |
  | `dialogs.open`    | list of answers, one per `showOpenDialog`: a list of paths, or `null` (cancelled); cancelled when the list runs out                                  |
  | `dialogs.save`    | the same for `showSaveDialog`: a path or `null`                                                                                                      |
  | `dialogs.message` | `[{response, checkboxChecked}]` per `showMessageBox`; `{response: 0}` when it runs out                                                               |
  | `osShell`         | `{openPath: "<error text>", openExternalError, trashError}`; all succeed by default                                                                  |
  | `clipboard`       | `{text, html, imagePng: hex, formats: [...]}` before the case                                                                                        |
  | `contents`        | `{capturePng: hex, pdf: hex, print: bool, zoom: {"<view>": factor}}`                                                                                 |
  | `mainWindow`      | `{view, window}`; the first `Shell` view by default                                                                                                  |
  | `media`           | TypeScript only (no Rust host yet): `{screen, microphone: "granted"\|"denied", sources: [{id, name}]}` for `systemPreferences` and `desktopCapturer` |
  | `recoveryRunId`   | the run id of the recovery state, `0000000000000000` by default                                                                                      |

  Each call is one log line named after the Electron call it fakes:
  `["dialog.showOpenDialog", parentWindowId|null, options]`, `["shell.openPath", path]`,
  `["clipboard.writeText", text]`, `["webContents.printToPDF", view, options]`, ... Options are
  written with Electron's keys, absent and empty fields left out.

- **Batch hosts.** Each batch crate exports, behind its `probe` cargo feature,

  ```rust
  #[cfg(feature = "probe")]
  pub fn probe_hosts(hosts: &mut shell_contract::Hosts, case: &serde_json::Value)
  ```

  which inserts the fakes of its own hosts, answering from `case`: the case's `hosts.<batch>` value
  (`hosts.d1` for D1; `Null` when absent). A fake that logs takes the case's log with
  `hosts.get::<Arc<Mutex<Vec<serde_json::Value>>>>()` (a type alias of the same type in your crate
  names the same entry), inserted before any `probe_hosts` runs; push one JSON array per call, named
  after the Electron or app call it fakes, and make the TypeScript side of the group log the same
  line (`cx.electron.log.push(...)`).

ipc-probe registers all six batches as they stand (a missing handler answers
`No handler registered`, as Electron would), then runs the calls one after another through the
dispatch, waiting after each until the workers are idle and the core and UI queues are drained.

The native shell attaches to a call of a document view the context its owned effects captured for
that view's renderer (its loss token and one origin per renderer). For the D1 groups (slides,
sheets, mind maps, diagrams) the probe attaches the same, made by its synthetic owned-effects
host: a fixture of that lifetime, never native authentication. The TypeScript has no such notion,
so a group's cases only name the views; a handler that refuses a call without it fails the case.

## Ledger and faults

- `divergences.json`: `{id, group, case, typescript, rust, reason}`, the two records as the gate
  printed them. A listed difference that no longer reproduces fails the gate.
- `faults/<batch>.json`: `{name, file, find, replace, groups}`; `file` is under `crates/`, `find`
  occurs once. The first entry is the positive control. Every fault must be CAUGHT.
