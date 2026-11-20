------------------------ MODULE NativeSubscription ------------------------
EXTENDS Naturals
VARIABLES live, subscribed, claimed, cancelled, notified, callbackCount, missed
vars == <<live, subscribed, claimed, cancelled, notified, callbackCount, missed>>

Init == /\ live = TRUE /\ subscribed = FALSE /\ claimed = FALSE
        /\ cancelled = FALSE /\ notified = FALSE /\ callbackCount = 0
        /\ missed = FALSE

\* Subscribe and revoke serialize on the registry lock. Contention is modeled
\* as a refused subscription which callers treat as owner loss.
Subscribe == /\ ~subscribed /\ ~cancelled
             /\ IF live THEN subscribed' = TRUE
                        ELSE /\ subscribed' = FALSE /\ notified' = TRUE
             /\ IF live THEN UNCHANGED notified ELSE TRUE
             /\ UNCHANGED <<live, claimed, cancelled, callbackCount, missed>>

Drop == /\ subscribed /\ ~cancelled
        /\ cancelled' = TRUE
        /\ UNCHANGED <<live, subscribed, claimed, notified, callbackCount, missed>>

Revoke == /\ live /\ live' = FALSE
          /\ claimed' = (subscribed /\ ~cancelled)
          /\ missed' = missed \/ (subscribed /\ ~cancelled /\ ~claimed')
          /\ UNCHANGED <<subscribed, cancelled, notified, callbackCount>>

\* The claimed callback runs outside the lock. Drop after claim does not
\* prevent a callback already claimed from running.
Notify == /\ claimed /\ ~notified
          /\ notified' = TRUE /\ callbackCount' = callbackCount + 1
          /\ UNCHANGED <<live, subscribed, claimed, cancelled, missed>>

Next == Subscribe \/ Drop \/ Revoke \/ Notify
Spec == Init /\ [][Next]_vars
AtMostOneCallback == callbackCount <= 1
NoMissedClaim == ~missed
=============================================================================
