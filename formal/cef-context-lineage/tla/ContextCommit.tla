------------------------- MODULE ContextCommit -------------------------
EXTENDS Naturals
Docs == {1, 2}
VARIABLES latest, pending, committed, expected, queued, granted, lateGrant,
          commitFirstGrant
vars == <<latest, pending, committed, expected, queued, granted, lateGrant,
          commitFirstGrant>>

Init == /\ latest = 1 /\ pending = 1 /\ committed = 0 /\ expected = 1
        /\ queued = {1} /\ granted = {} /\ lateGrant = FALSE
        /\ commitFirstGrant = FALSE

Commit == /\ pending # 0 /\ committed' = pending /\ pending' = 0
          /\ UNCHANGED <<latest, expected, queued, granted, lateGrant,
                         commitFirstGrant>>

Replace == /\ latest = 1 /\ latest' = 2 /\ pending' = 2 /\ expected' = 2
           /\ queued' = queued \cup {2} /\ granted' = {}
           /\ UNCHANGED <<committed, lateGrant, commitFirstGrant>>

Deliver(d) == /\ d \in queued /\ queued' = queued \ {d}
              /\ IF d = latest /\ d = expected /\
                    (d = pending \/ (pending = 0 /\ d = committed))
                   THEN /\ granted' = granted \cup {d}
                        /\ lateGrant' = lateGrant \/ d # latest
                        /\ commitFirstGrant' = commitFirstGrant \/
                                               (d = committed /\ pending = 0)
                   ELSE UNCHANGED <<granted, lateGrant, commitFirstGrant>>
              /\ UNCHANGED <<latest, pending, committed, expected>>

Next == Commit \/ Replace \/ (\E d \in Docs : Deliver(d))
Spec == Init /\ [][Next]_vars
NoRelabeledGrant == ~lateGrant
CommitFirstNeverGrants == ~commitFirstGrant
=============================================================================
