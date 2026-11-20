------------------------ MODULE ContextLineage ------------------------
EXTENDS Naturals, FiniteSets
CONSTANT NativeLineage
Docs == {1, 2}
Processes == {1, 2}
VARIABLES current, live, process, alive, sent, pending, admitted, stale,
          activated, pendingAck
vars == <<current, live, process, alive, sent, pending, admitted, stale,
          activated, pendingAck>>

Init == /\ current = 1 /\ live = {1}
        /\ process \in [Docs -> Processes] /\ alive = Processes
        /\ sent = {} /\ pending = {} /\ pendingAck = {}
        /\ admitted = {} /\ stale = FALSE /\ activated = {}

\* A registration captures its document and originating process, not a label.
CreateRegistration(d) ==
  /\ d \in live /\ d \notin sent /\ process[d] \in alive
  /\ sent' = sent \cup {d}
  /\ pending' = pending \cup {<<d, process[d]>>}
  /\ UNCHANGED <<current, live, process, alive, admitted, stale,
                 activated, pendingAck>>

\* Navigation revokes the previous document even when the RFH is reused.
Navigate ==
  /\ current = 1 /\ 1 \in live
  /\ current' = 2 /\ live' = {2}
  /\ admitted' = {} /\ activated' = {}
  /\ UNCHANGED <<process, alive, sent, pending, stale, pendingAck>>

\* The old renderer may terminate before or after a replacement is created.
Terminate(p) ==
  /\ p \in alive /\ alive' = alive \ {p}
  /\ live' = {d \in live : process[d] # p}
  /\ admitted' = {d \in admitted : process[d] # p}
  /\ activated' = {d \in activated : process[d] # p}
  /\ UNCHANGED <<current, process, sent, pending, stale, pendingAck>>

DropRegistration(m) ==
  /\ m \in pending /\ pending' = pending \ {m}
  /\ UNCHANGED <<current, live, process, alive, sent, admitted, stale,
                 activated, pendingAck>>

\* The TRUE guard is the proposed native receiver+pending-document authority;
\* it is not established by stock CEF, the draft Rust code, or either patch.
DeliverRegistration(m) ==
  /\ m \in pending
  /\ pending' = pending \ {m}
  /\ IF NativeLineage /\ (m[1] # current \/ m[1] \notin live \/
                           m[2] # process[m[1]] \/ m[2] \notin alive)
       THEN UNCHANGED <<admitted, stale, pendingAck>>
       ELSE /\ admitted' = admitted \cup {m[1]}
            /\ pendingAck' = pendingAck \cup {m}
            /\ stale' = stale \/ m[1] # current \/ m[1] \notin live \/
                        m[2] # process[m[1]] \/ m[2] \notin alive
  /\ UNCHANGED <<current, live, process, alive, sent, activated>>

\* A late activation acknowledgement cannot resurrect revoked authority.
DeliverAck(m) ==
  /\ m \in pendingAck
  /\ pendingAck' = pendingAck \ {m}
  /\ IF m[1] = current /\ m[1] \in live /\ m[1] \in admitted /\
        m[2] = process[m[1]] /\ m[2] \in alive
       THEN activated' = activated \cup {m[1]}
       ELSE UNCHANGED activated
  /\ UNCHANGED <<current, live, process, alive, sent, pending, admitted, stale>>

\* Late release is scoped to the original context, never the current frame.
Release(d) ==
  /\ d \in Docs /\ (d \in live \/ d \in admitted \/ d \in activated)
  /\ live' = live \ {d}
  /\ admitted' = admitted \ {d}
  /\ activated' = activated \ {d}
  /\ UNCHANGED <<current, process, alive, sent, pending, stale, pendingAck>>

Next == Navigate \/ (\E p \in Processes : Terminate(p)) \/
        (\E d \in Docs : CreateRegistration(d) \/ Release(d)) \/
        (\E m \in Docs \X Processes :
          DropRegistration(m) \/ DeliverRegistration(m) \/ DeliverAck(m))
Spec == Init /\ [][Next]_vars

TypeOK == /\ current \in Docs /\ live \subseteq Docs
          /\ process \in [Docs -> Processes] /\ alive \subseteq Processes
          /\ sent \subseteq Docs /\ pending \subseteq Docs \X Processes
          /\ pendingAck \subseteq Docs \X Processes /\ admitted \subseteq Docs
          /\ activated \subseteq Docs /\ stale \in BOOLEAN
\* FROZEN PROPERTIES
NoStaleAdmission == ~stale
NoStaleActivation == activated \subseteq live /\ activated \subseteq {current}
\* END FROZEN PROPERTIES
SanityNeverActivated == activated = {}
=============================================================================
