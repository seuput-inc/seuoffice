# Native document retirement

## Verdict

**Not checked.** Model/configuration/mutation source is prepared. No local or hosted SANY, TLC, mutation runner, Rust build/test, installer, dispatch, or stamp was run. There are no state counts, depths, passing verdicts, reproduced counterexamples, or evidence logs to cite. No manifest/workflow changes were made.

## Promise and harm

One admitted replacement publishes the new attachment set and logically retires every captured old document under the same complete authority admission. Old-document-only workers must then reject. Loss observers, old Attachment/Session Drop, and accepted UI execution must occur after the emitting operation releases every authority gate, including unwinding. Otherwise users can lose a replacement to an old worker or hang the application on a reentrant native/document gate.

Tests can force selected schedules; this bounded model enumerates the separate acquisition, effect, loss, unwind, release, and observer steps. It is not a proof that the Rust code refines those steps.

## Bounds

- Two work participants: one retiring operation and one old-document-only effect worker.
- Two captured owner positions, either identical identities or distinct identities within every category.
- Two old document generations by default; one larger positive configuration has three. The production API accepts an uncapped slice; this finite model does **not** prove arbitrary-N correctness.
- One optional independent external authority loss; one observer which reenters all gate categories.
- One replacement attempt and one effect attempt; unwind point is none, before retirement, or after retirement.
- Canonical categories: native, asset, renderer/generic document, Slides, Docs, PDF, cancellation. Concrete identities/paths/bytes are abstracted away.
- Twelve matrix scenarios: eight expected-pass schedules/bounds and four expected counterexamples. Eight isolated text mutations cover publication, stale effects, held notification, alias self-wait, all/second-document-only notification loss, and terminal progress. All expectations remain **unobserved**.

Queued and delivered notifications are sets of exact document identities. Per-document `ncount` records each deferred token consumed; conservation requires that consumed-token count equal delivered status for **every** document. `Pending` is the set of queued documents not yet consumed. One reentrant observer is bound to `ObservedDocument`; positive rows select the second/third document too. A dedicated matrix row and isolated text mutation discard **only document two's** notification after consuming it. Thus one successful wakeup cannot mask another document's loss. This strengthens the original aggregate notification abstraction to the already-required per-retired-document contract; no passing execution motivated or justified a property change.

## Frozen requirements

See Properties.md. Multi-document migration generalized AtomicReplace from one old document to **all** captured old documents; it did not weaken that property. No state constraint, symmetry reduction, or implementation-specific success escape is present. Weak fairness of Next excludes scheduler-only infinite stuttering in this finite acyclic protocol; it does not resolve a blocked self-wait. Deadlock witness configurations disable TLC's early deadlock stop so EventuallyTerminal can report the terminal-progress violation.

## Exclusions and refinement obligations

This does not prove complete CEF authentication, filesystem atomicity, format conversion, native platform/UI behavior, native registry panic handling, arbitrary callback behavior, allocator safety, or arbitrary-N/subset-alias topology. Owners and epochs are assumed authentically captured before work. Source lookup/rebinding is not modeled as authority. A consumer must prevalidate/stage the entire participant set and all successors before retiring anything; all actual mutations must fit the modeled admitted section. Panic before or after that section is modeled, not an allocator failure midway through an otherwise falsely assumed atomic state algorithm.

The single-view Slides consumer was source-observed returning RetiredBinding from the gated callback and dropping it after with_owned_transition returns. The N-view sharedsession consumer remains a **pending correspondence obligation** until its owner lands and the parent checks its actual preflight, swaps, and outside-gate Drops. The model must not be used to declare that consumer complete.

## Hosted execution preparation

Existing convention: `.github/workflows/seuoffice-formal.yml` downloads checksum-pinned tla2tools 1.8.0 to `$RUNNER_TEMP/tla2tools.jar`, uses Java 21 and `tools/formal/run-matrix.sh`. That workflow only accepts the `deep` boolean and selects entries from formal/manifest.json; this unregistered model is NOT runnable by inventing an `only` workflow input. Parent owns eventual registration/dispatch.

On an authorized hosted runner with the existing jar already checksum-verified, from the checkout root:

```sh
model="$PWD/formal/native-document-retirement/tla"
(cd "$model" && java -cp "$RUNNER_TEMP/tla2tools.jar" tla2sany.SANY NativeDocumentRetirement.tla)
bash tools/formal/run-matrix.sh "$model" NativeDocumentRetirement.tla "$model/matrix.json" "$RUNNER_TEMP/tla2tools.jar"
```

The JSON text mutations follow ompdesk-formal-verify's mutate.ts contract. On a hosted runner where that skill is explicitly available at `$FORMAL_SKILL_DIR`:

```sh
bun "$FORMAL_SKILL_DIR/scripts/mutate.ts" "$model/NativeDocumentRetirement.tla" "$model/mutations.json"
```

No tool installation or local execution is authorized. Preserve every run's raw logs before making a bounded verdict; inspect/reproduce counterexamples before treating them as product defects. Existing run-matrix.sh only checks expected log verdict strings, so its compatibility with the actual TLC temporal diagnostics must be observed, not assumed.
