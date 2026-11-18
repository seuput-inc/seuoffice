# Frozen native focus properties

- `NoDuplicate`: a lifetime receives no more blur callbacks than observed focus-loss transitions. The source is the assigned exact-once focus-loss contract.
- `NoLateCallback`: a callback is not invoked after its lifetime is revoked or replaced. The source is the assigned stale-callback and reused-ID safety contract.

The properties are not weakened to accommodate the current implementation. They are modeled for two lifetimes and two losses per lifetime. Neither has been checked in this worktree.
