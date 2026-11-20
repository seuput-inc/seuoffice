-------------------------- MODULE NativePromise --------------------------
EXTENDS Naturals, FiniteSets
Contexts == {1, 2}
Requests == {1, 2}
CONSTANT Owner
OwnerMap == [r \in Requests |-> r]
VARIABLES live, phase, replies, success, badSuccess
vars == <<live, phase, replies, success, badSuccess>>

Init == /\ live = Contexts
        /\ phase = [r \in Requests |-> "pending"]
        /\ replies = [r \in Requests |-> 0]
        /\ success = {} /\ badSuccess = FALSE

\* The native handler marks a nonce as settling before invoking serialization;
\* a reentrant resolve/reject for the same nonce cannot claim it again.
Claim(r) == /\ phase[r] = "pending" /\ Owner[r] \in live
            /\ phase' = [phase EXCEPT ![r] = "settling"]
            /\ UNCHANGED <<live, replies, success, badSuccess>>

\* Serialization may run script and reenter release/cancellation.
Complete(r) == /\ phase[r] = "settling"
               /\ phase' = [phase EXCEPT ![r] = "finished"]
               /\ replies' = [replies EXCEPT ![r] = @ + 1]
               /\ IF Owner[r] \in live
                    THEN /\ success' = success \cup {r}
                         /\ badSuccess' = badSuccess \/ Owner[r] \notin live
                    ELSE UNCHANGED <<success, badSuccess>>
               /\ UNCHANGED live

Release(c) == /\ c \in live /\ live' = live \ {c}
              /\ phase' = [r \in Requests |->
                   IF Owner[r] = c /\ phase[r] # "finished"
                   THEN "finished" ELSE phase[r]]
              /\ replies' = [r \in Requests |->
                   IF Owner[r] = c /\ phase[r] # "finished"
                   THEN replies[r] + 1 ELSE replies[r]]
              /\ UNCHANGED <<success, badSuccess>>

Cancel(r) == /\ phase[r] # "finished"
             /\ phase' = [phase EXCEPT ![r] = "finished"]
             /\ replies' = [replies EXCEPT ![r] = @ + 1]
             /\ UNCHANGED <<live, success, badSuccess>>

Next == (\E r \in Requests : Claim(r) \/ Complete(r) \/ Cancel(r)) \/
        (\E c \in Contexts : Release(c))
Spec == Init /\ [][Next]_vars
AtMostOneReply == \A r \in Requests : replies[r] <= 1
NoSuccessAfterRelease == ~badSuccess
=============================================================================
