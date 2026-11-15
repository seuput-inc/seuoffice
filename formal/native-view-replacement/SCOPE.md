# Native view replacement model

This unexecuted bounded model covers two concurrent replacement writers, the generation each captures, retirement of that exact generation outside the view-table lock, and rejection when the table no longer contains the captured generation. `NativeViewReplacement.cfg` selects two writers.

It does not model mutex acquisition, deadlock freedom, native context or asset gates, event delivery, callbacks, creator authority, or actual Rust execution. In particular, its invariants are not proof that the implementation is free of deadlock. No TLC or mutation result has been produced for this model.
