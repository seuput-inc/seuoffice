---------------- MODULE NativeViewReplacement ----------------
EXTENDS Naturals, FiniteSets, TLC
CONSTANT Writers, Initial, None
ASSUME /\ Initial \notin Writers /\ None \notin Writers /\ Initial # None
VARIABLES current, live, phase, captured, revoked, rejected, committed
vars == <<current, live, phase, captured, revoked, rejected, committed>>

Init == /\ current = Initial
        /\ live = {Initial}
        /\ phase = [w \in Writers |-> "idle"]
        /\ captured = [w \in Writers |-> None]
        /\ revoked = {}
        /\ rejected = {}
        /\ committed = {}

\* @code crates/shell/src/views.rs add_view: snapshot the existing generation under the table lock.
Capture(w) == /\ phase[w] = "idle"
              /\ captured' = [captured EXCEPT ![w] = current]
              /\ phase' = [phase EXCEPT ![w] = "captured"]
              /\ UNCHANGED <<current, live, revoked, rejected, committed>>

\* @code crates/shell/src/views.rs add_view: retire only the captured generation outside the table lock.
Revoke(w) == /\ phase[w] = "captured"
             /\ live' = live \ {captured[w]}
             /\ revoked' = revoked \cup {<<w, captured[w]>>}
             /\ phase' = [phase EXCEPT ![w] = "revoked"]
             /\ UNCHANGED <<current, captured, rejected, committed>>

\* @code crates/shell/src/views.rs add_view: compare the captured identity before replacing.
Commit(w) == /\ phase[w] = "revoked"
             /\ IF current = captured[w]
                   THEN /\ current' = w
                        /\ live' = live \cup {w}
                        /\ committed' = committed \cup {w}
                        /\ UNCHANGED rejected
                   ELSE /\ rejected' = rejected \cup {w}
                        /\ UNCHANGED <<current, live, committed>>
             /\ phase' = [phase EXCEPT ![w] = "done"]
             /\ UNCHANGED <<captured, revoked>>

Next == \E w \in Writers : Capture(w) \/ Revoke(w) \/ Commit(w)
Spec == Init /\ [][Next]_vars
TypeOK == /\ current \in Writers \cup {Initial}
          /\ live \subseteq Writers \cup {Initial}
          /\ rejected \subseteq Writers /\ committed \subseteq Writers
NoRejectedCommit == rejected \cap committed = {}
OnlyCapturedRevoked == \A pair \in revoked : pair[2] = captured[pair[1]]
===============================================================
