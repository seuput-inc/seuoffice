# Dispatcher cancellation provenance — frozen requirements

Source-only; no parser, TLC, mutation runner or Rust execution has run.

1. A public CancelToken.cancel call is always retained, including a call through the same alias **after** expected renderer retirement. First-cause bits must not erase later calls.
2. Expected renderer retirement may cancel the old primary dispatcher token without canceling its independent public-cancellation signal.
3. Removing the old renderer entry must preserve the original view-destruction signal in the same per-view map until actual view_destroyed, whether or not a successor request arrived.
4. Cancellation observation can transfer only for an exact accepted predecessor with dispatcher-bound view/generation provenance. A legacy/synthetic context without it fails closed.
5. Browser loss, reservation failure, independent asset/view-policy loss, and distinct external cancel/pending signals remain effective after transfer.
6. An external cancellation subscriber cannot lose a wakeup whether it subscribes before or after cancellation; its callback may reenter the original alias after source locks have been released.

The finite model checks token/entry/observation semantics, not CEF identity issuance, accepted PDF lineage construction, native actor business effects, callback-lock refinement or arbitrary-N correctness. The actual source callback mutex/reentry and concurrent lazy-subscription boundaries have separate deterministic Rust regression source, all unrun. No runtime cancellation registry is added: Dispatcher extends its existing per-view entry with an independent destruction token and a renderer-entry list.
