# Frozen properties

## NoStaleAdmission

`NoStaleAdmission == ~stale`.

A registration from a released or replaced native document cannot be admitted. Source: the current assignment's exact-context and old-request requirements, and the pinned CEF `CefFrame::SendProcessMessage` contract which does not guarantee delivery during navigation. Protocol-level safety invariant. The named property is frozen between the banners in `tla/ContextLineage.tla`. Result not run.

## NoStaleActivation

`NoStaleActivation == activated \subseteq live /\ activated \subseteq {current}`.
An acknowledgement arriving after release, replacement or process termination cannot reactivate the old context. This models the selected handshake; the implementation has not established the correspondence. Result not run.
