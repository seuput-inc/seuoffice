------------------------ MODULE PdfSignCommit ------------------------
EXTENDS Naturals, TLC

VARIABLES live, cancelled, staged, target, gate, outcome
vars == <<live, cancelled, staged, target, gate, outcome>>

Init == /\ live = TRUE /\ cancelled = FALSE /\ staged = FALSE
        /\ target = "old" /\ gate = "free" /\ outcome = "pending"

Stage == /\ ~staged /\ outcome = "pending" /\ staged' = TRUE
         /\ UNCHANGED <<live, cancelled, target, gate, outcome>>

Acquire == /\ staged /\ gate = "free" /\ outcome = "pending"
           /\ gate' = "commit"
           /\ UNCHANGED <<live, cancelled, staged, target, outcome>>

Commit == /\ gate = "commit" /\ live /\ ~cancelled
          /\ target' = "signed" /\ outcome' = "success" /\ gate' = "free"
          /\ UNCHANGED <<live, cancelled, staged>>

Refuse == /\ gate = "commit" /\ (~live \/ cancelled)
          /\ outcome' = "refused" /\ gate' = "free"
          /\ UNCHANGED <<live, cancelled, staged, target>>

ReplaceFailure == /\ gate = "commit" /\ live /\ ~cancelled
                  /\ outcome' = "failed" /\ gate' = "free"
                  /\ UNCHANGED <<live, cancelled, staged, target>>

Revoke == /\ gate = "free" /\ live /\ live' = FALSE
          /\ UNCHANGED <<cancelled, staged, target, gate, outcome>>

Cancel == /\ gate = "free" /\ ~cancelled /\ cancelled' = TRUE
          /\ UNCHANGED <<live, staged, target, gate, outcome>>

Next == Stage \/ Acquire \/ Commit \/ Refuse \/ ReplaceFailure \/ Revoke \/ Cancel
Spec == Init /\ [][Next]_vars
PreservesTarget == outcome \in {"failed", "refused"} => target = "old"
SuccessOnlyAfterCommit == (outcome = "success") = (target = "signed")
=============================================================================
