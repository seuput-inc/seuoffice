----------------------- MODULE NativeEvalOrdering -----------------------
EXTENDS Naturals
VARIABLES browserLive, rendererLive, evalPending, revokePending, resultPending,
          effectAfterRevoke, commitAfterRevoke
vars == <<browserLive, rendererLive, evalPending, revokePending, resultPending,
          effectAfterRevoke, commitAfterRevoke>>

Init == /\ browserLive = TRUE /\ rendererLive = TRUE
        /\ evalPending = FALSE /\ revokePending = FALSE /\ resultPending = FALSE
        /\ effectAfterRevoke = FALSE /\ commitAfterRevoke = FALSE

Authorize == /\ browserLive /\ ~evalPending /\ ~resultPending
             /\ evalPending' = TRUE
             /\ UNCHANGED <<browserLive, rendererLive, revokePending,
                            resultPending, effectAfterRevoke, commitAfterRevoke>>

BrowserRevoke == /\ browserLive /\ browserLive' = FALSE /\ revokePending' = TRUE
                 /\ UNCHANGED <<rendererLive, evalPending, resultPending,
                                effectAfterRevoke, commitAfterRevoke>>

DeliverRevoke == /\ revokePending /\ revokePending' = FALSE /\ rendererLive' = FALSE
                 /\ UNCHANGED <<browserLive, evalPending, resultPending,
                                effectAfterRevoke, commitAfterRevoke>>

\* An exact context may remain physically valid until revoke reaches its
\* renderer. Its JavaScript can have side effects before returning a result.
Execute == /\ evalPending /\ rendererLive
           /\ evalPending' = FALSE /\ resultPending' = TRUE
           /\ effectAfterRevoke' = effectAfterRevoke \/ ~browserLive
           /\ UNCHANGED <<browserLive, rendererLive, revokePending, commitAfterRevoke>>

\* Results, unlike already-executed JavaScript, can be gated at the browser.
CommitResult == /\ resultPending /\ browserLive /\ resultPending' = FALSE
                /\ commitAfterRevoke' = commitAfterRevoke \/ ~browserLive
                /\ UNCHANGED <<browserLive, rendererLive, evalPending,
                               revokePending, effectAfterRevoke>>

Next == Authorize \/ BrowserRevoke \/ DeliverRevoke \/ Execute \/ CommitResult
Spec == Init /\ [][Next]_vars
NoRendererEffectAfterRevoke == ~effectAfterRevoke
NoResultCommitAfterRevoke == ~commitAfterRevoke
=============================================================================
