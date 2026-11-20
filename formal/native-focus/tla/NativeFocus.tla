------------------------- MODULE NativeFocus -------------------------
EXTENDS Naturals, FiniteSets

CONSTANT SerializedDelivery
Gen == {1, 2}
VARIABLES current, alive, focused, pending, checked, losses, emitted, stale
vars == <<current, alive, focused, pending, checked, losses, emitted, stale>>

Init ==
  /\ current = 1
  /\ alive = {1}
  /\ focused = FALSE
  /\ pending = 0
  /\ checked = 0
  /\ losses = [g \in Gen |-> 0]
  /\ emitted = [g \in Gen |-> 0]
  /\ stale = FALSE

\* @code crates/vendor/tauri-runtime-cef/src/platform/windows/focus.rs#FocusState::observe
Gain ==
  /\ current \in alive /\ ~focused /\ losses[current] < 2
  /\ focused' = TRUE
  /\ UNCHANGED <<current, alive, pending, checked, losses, emitted, stale>>

\* @code crates/vendor/tauri-runtime-cef/src/platform/windows/focus.rs#FocusState::observe
\* @code crates/vendor/tauri-runtime-cef/src/platform/macos/focus.rs#refresh_all
Loss ==
  /\ current \in alive /\ focused /\ pending = 0 /\ checked = 0
  /\ focused' = FALSE
  /\ pending' = current
  /\ losses' = [losses EXCEPT ![current] = @ + 1]
  /\ UNCHANGED <<current, alive, checked, emitted, stale>>

\* Bug mode models the earlier direct native gate followed by handler invocation.
CheckGate ==
  /\ ~SerializedDelivery /\ pending # 0 /\ checked = 0 /\ pending \in alive
  /\ checked' = pending
  /\ pending' = 0
  /\ UNCHANGED <<current, alive, focused, losses, emitted, stale>>

\* A different thread may revoke the old lifetime after the gate read.
Invoke ==
  /\ ~SerializedDelivery /\ checked # 0
  /\ emitted' = [emitted EXCEPT ![checked] = @ + 1]
  /\ stale' = stale \/ checked \notin alive \/ checked # current
  /\ checked' = 0
  /\ UNCHANGED <<current, alive, focused, pending, losses>>
\* @code crates/vendor/tauri-runtime-cef/src/browser_lifetime.rs#BrowserLifetimes::is_attached

\* UI owner processes the captured native event and lifecycle revocation serially.
OwnerDeliver ==
  /\ SerializedDelivery /\ pending # 0 /\ pending \in alive /\ pending = current
  /\ emitted' = [emitted EXCEPT ![pending] = @ + 1]
  /\ stale' = stale \/ pending \notin alive \/ pending # current
  /\ pending' = 0
  /\ UNCHANGED <<current, alive, focused, checked, losses>>

\* @code crates/vendor/tauri-runtime-cef/src/browser_lifetime.rs#BrowserLifetimes::closing
Revoke ==
  /\ current \in alive
  /\ alive' = alive \ {current}
  /\ focused' = FALSE
  /\ UNCHANGED <<current, pending, checked, losses, emitted, stale>>

\* A replacement may reuse a public label or OS handle, never the lifetime token.
Replace ==
  /\ current = 1 /\ 1 \notin alive
  /\ current' = 2
  /\ alive' = alive \cup {2}
  /\ focused' = FALSE
  /\ UNCHANGED <<pending, checked, losses, emitted, stale>>

DiscardRevoked ==
  /\ pending # 0 /\ pending \notin alive
  /\ pending' = 0
  /\ UNCHANGED <<current, alive, focused, checked, losses, emitted, stale>>

Next == Gain \/ Loss \/ CheckGate \/ Invoke \/ OwnerDeliver \/ Revoke \/ Replace \/ DiscardRevoked
Spec == Init /\ [][Next]_vars

TypeOK ==
  /\ current \in Gen /\ alive \subseteq Gen /\ focused \in BOOLEAN
  /\ pending \in Gen \cup {0} /\ checked \in Gen \cup {0}
  /\ losses \in [Gen -> 0..2] /\ emitted \in [Gen -> 0..2]
  /\ stale \in BOOLEAN

\* FROZEN PROPERTIES
NoDuplicate == \A g \in Gen : emitted[g] <= losses[g]
NoLateCallback == ~stale
\* END FROZEN PROPERTIES
SanityNeverFocused == ~focused
=============================================================================
