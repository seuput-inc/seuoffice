# Native focus model correspondence

`Gain` and `Loss` represent the focused-state transitions in Windows `FocusState::observe` and macOS `refresh_all`. A descendant transfer that remains inside the observed native view is a stuttering step. A native activation loss or a reparent refresh that leaves the view is `Loss`.

In the bug configuration, `CheckGate` is the alive check and `Invoke` the subsequent handler call. Revocation can interleave between them. In the serialized configuration, `OwnerDeliver` represents the planned UI-owner registration check and handler call as one action. Its correctness depends on every browser revocation and replacement occurring on that same owner. `Replace` represents a distinct browser lifetime with a reused public label or native handle. `DiscardRevoked` rejects a queued event after revocation.

The bounded model does not establish native hook coverage, callback ordering inside AppKit or Win32, reentrant callback behavior, or that runtime code meets the serialization assumption. The matrix and mutations have not been run.
