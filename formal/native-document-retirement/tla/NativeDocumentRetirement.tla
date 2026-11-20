-------------------- MODULE NativeDocumentRetirement --------------------
EXTENDS Naturals, Sequences, FiniteSets, TLC

CONSTANTS Alias, NotifyBeforeUnlock, InvalidateAfterUnlock, AliasDoubleLock,
          UnwindPoint, DocumentCount, EffectDocument, ObservedDocument, DropSecond
ASSUME /\ Alias \in BOOLEAN
       /\ NotifyBeforeUnlock \in BOOLEAN
       /\ InvalidateAfterUnlock \in BOOLEAN
       /\ AliasDoubleLock \in BOOLEAN
       /\ UnwindPoint \in {"none", "before", "after"}
       /\ DocumentCount \in Nat /\ DocumentCount > 0
       /\ EffectDocument \in 1..DocumentCount
       /\ ObservedDocument \in 1..DocumentCount
       /\ DropSecond \in BOOLEAN

EarlyNotify == NotifyBeforeUnlock
LateRetire == InvalidateAfterUnlock
DuplicateAlias == AliasDoubleLock
DeliverNotification(document) == ~(DropSecond /\ document = 2)

Pair(category) == IF Alias THEN << <<category, 1>> >>
                  ELSE << <<category, 1>>, <<category, 2>> >>
Documents == 1..DocumentCount
DocumentGates == [document \in Documents |-> <<"document", document>>]
OldDoc == <<"document", EffectDocument>>
Canonical == Pair("native") \o Pair("asset") \o Pair("generation")
             \o DocumentGates \o Pair("slides") \o Pair("docs")
             \o Pair("pdf") \o Pair("cancel")
Requested == IF Alias /\ DuplicateAlias
             THEN <<Canonical[1]>> \o Canonical ELSE Canonical
Gates == {Canonical[i] : i \in 1..Len(Canonical)}
ExternalGates == Gates \ {DocumentGates[document] : document \in Documents}
Actors == {"none", "retire", "effect", "observer", "loss"}
Pop(sequence) == SubSeq(sequence, 1, Len(sequence) - 1)

VARIABLES locks, active, rphase, rpos, rheld, attachment, oldActive,
          queued, notified, ncount, ephase, effectAfterReplace,
          ophase, opos, oheld, lphase, lgate, unwound
vars == <<locks, active, rphase, rpos, rheld, attachment, oldActive,
          queued, notified, ncount, ephase, effectAfterReplace,
          ophase, opos, oheld, lphase, lgate, unwound>>

Pending == {document \in queued : ncount[document] = 0}

Init == /\ locks = [gate \in Gates |-> "none"]
        /\ active = [gate \in Gates |-> TRUE]
        /\ rphase = "acquire" /\ rpos = 1 /\ rheld = <<>>
        /\ attachment = "old" /\ oldActive = [document \in Documents |-> TRUE]
        /\ queued = {} /\ notified = {} /\ ncount = [document \in Documents |-> 0]
        /\ ephase = "acquire" /\ effectAfterReplace = FALSE
        /\ ophase = "idle" /\ opos = 1 /\ oheld = <<>>
        /\ lphase = "choose" /\ lgate = OldDoc /\ unwound = FALSE

\* @code crates/shell/src/ipc/native_owner/effects.rs#with_many_owner_generations_transition
\* @code crates/shell-contract/src/runtime.rs#RendererGenerationGatePlan.with_active_retiring
AcquireR == /\ rphase = "acquire" /\ rpos <= Len(Requested)
            /\ locks[Requested[rpos]] = "none"
            /\ locks' = [locks EXCEPT ![Requested[rpos]] = "retire"]
            /\ rheld' = Append(rheld, Requested[rpos]) /\ rpos' = rpos + 1
            /\ UNCHANGED <<active, rphase, attachment, oldActive, queued,
                 notified, ncount, ephase, effectAfterReplace, ophase, opos,
                 oheld, lphase, lgate, unwound>>

\* @code crates/shell/src/ipc/native_owner/effects.rs#with_many_owner_generations_transition
AdmitR == /\ rphase = "acquire" /\ rpos = Len(Requested) + 1
          /\ rphase' = IF (\A document \in Documents : oldActive[document])
                          /\ (\A gate \in Gates : active[gate])
                         THEN "admitted" ELSE "unlock"
          /\ UNCHANGED <<locks, active, rpos, rheld, attachment, oldActive,
               queued, notified, ncount, ephase, effectAfterReplace,
               ophase, opos, oheld, lphase, lgate, unwound>>

\* @code crates/shell-contract/src/runtime.rs#AdmittedDocumentRetirement.retire
\* @code crates/shell-ipc/d1/src/slides/state.rs#AppState.attach_retiring
\* @code crates/shell-ipc/d1/src/slides/open_authority.rs#OpenAuthority.commit
\* The single-view consumer is source-observed; N-view publication is pending
\* consumer correspondence, not established by this atomic model action.
CommitR == /\ rphase = "admitted"
           /\ UnwindPoint # "before"
           /\ attachment' = "new"
           /\ oldActive' = IF LateRetire THEN oldActive ELSE [document \in Documents |-> FALSE]
           /\ queued' = Documents
           /\ notified' = IF EarlyNotify THEN Documents ELSE {}
           /\ ncount' = [document \in Documents |-> IF EarlyNotify THEN 1 ELSE 0]
           /\ rphase' = IF EarlyNotify THEN "waitObserver" ELSE "unlock"
           /\ UNCHANGED <<locks, active, rpos, rheld, ephase, effectAfterReplace,
                ophase, opos, oheld, lphase, lgate, unwound>>

\* Environment: panic before or after logical retirement; guards unwind.
UnwindR == /\ ~unwound
           /\ (\/ /\ UnwindPoint = "before" /\ rphase = "admitted"
               \/ /\ UnwindPoint = "after" /\ queued # {} /\ rphase \in {"unlock", "waitObserver"})
           /\ Len(rheld) > 0
           /\ unwound' = TRUE /\ rphase' = "unlock"
           /\ UNCHANGED <<locks, active, rpos, rheld, attachment, oldActive,
                queued, notified, ncount, ephase, effectAfterReplace,
                ophase, opos, oheld, lphase, lgate>>

\* Mutation-only synchronous callback waits while the emitter owns gates.
ReturnObserver == /\ rphase = "waitObserver" /\ ophase = "done"
                  /\ rphase' = "unlock"
                  /\ UNCHANGED <<locks, active, rpos, rheld, attachment,
                       oldActive, queued, notified, ncount, ephase,
                       effectAfterReplace, ophase, opos, oheld, lphase,
                       lgate, unwound>>

\* @code crates/shell/src/ipc/native_owner/effects.rs#with_many_owner_generations_transition
\* Rust guards drop in reverse nesting order, including unwinding.
UnlockR == /\ rphase = "unlock" /\ Len(rheld) > 0
           /\ (UnwindPoint # "after" \/ queued = {} \/ unwound)
           /\ locks' = [locks EXCEPT ![rheld[Len(rheld)]] = "none"]
           /\ rheld' = Pop(rheld)
           /\ UNCHANGED <<active, rphase, rpos, attachment, oldActive, queued,
                notified, ncount, ephase, effectAfterReplace, ophase, opos,
                oheld, lphase, lgate, unwound>>

\* @code crates/shell/src/ipc/native_owner/effects.rs#with_many_owner_generations_transition
FinishR == /\ rphase = "unlock" /\ rheld = <<>>
           /\ oldActive' = IF attachment = "new" /\ LateRetire
                            THEN [document \in Documents |-> FALSE] ELSE oldActive
           /\ rphase' = IF Pending # {} THEN "notify" ELSE "done"
           /\ UNCHANGED <<locks, active, rpos, rheld, attachment, queued,
                notified, ncount, ephase, effectAfterReplace, ophase, opos,
                oheld, lphase, lgate, unwound>>

\* @code crates/shell-contract/src/runtime.rs#DocumentRetirementPlan.drop
NotifyR == /\ rphase = "notify"
           /\ \E document \in Pending :
                /\ notified' = IF DeliverNotification(document) THEN notified \cup {document} ELSE notified
                /\ ncount' = [ncount EXCEPT ![document] = @ + 1]
                /\ rphase' = IF Pending \ {document} = {} THEN "done" ELSE "notify"
           /\ UNCHANGED <<locks, active, rpos, rheld, attachment, oldActive,
                queued, ephase, effectAfterReplace, ophase, opos, oheld,
                lphase, lgate, unwound>>

\* @code crates/shell-contract/src/runtime.rs#RendererGeneration.with_active
AcquireE == /\ ephase = "acquire" /\ locks[OldDoc] = "none"
            /\ locks' = [locks EXCEPT ![OldDoc] = "effect"]
            /\ ephase' = "effect"
            /\ UNCHANGED <<active, rphase, rpos, rheld, attachment, oldActive,
                 queued, notified, ncount, effectAfterReplace, ophase, opos,
                 oheld, lphase, lgate, unwound>>

\* @code crates/shell-contract/src/runtime.rs#RendererGeneration.with_active
EffectE == /\ ephase = "effect"
           /\ effectAfterReplace' = effectAfterReplace \/ (oldActive[EffectDocument] /\ attachment = "new")
           /\ ephase' = "release"
           /\ UNCHANGED <<locks, active, rphase, rpos, rheld, attachment,
                oldActive, queued, notified, ncount, ophase, opos, oheld,
                lphase, lgate, unwound>>

\* @code crates/shell-contract/src/runtime.rs#RendererGeneration.with_active
ReleaseE == /\ ephase = "release"
            /\ locks' = [locks EXCEPT ![OldDoc] = "none"] /\ ephase' = "done"
            /\ UNCHANGED <<active, rphase, rpos, rheld, attachment, oldActive,
                 queued, notified, ncount, effectAfterReplace, ophase, opos,
                 oheld, lphase, lgate, unwound>>

\* Environment: a loss observer reenters each category in canonical order.
StartO == /\ ObservedDocument \in notified /\ ophase = "idle" /\ ophase' = "acquire"
          /\ UNCHANGED <<locks, active, rphase, rpos, rheld, attachment,
               oldActive, queued, notified, ncount, ephase, effectAfterReplace,
               opos, oheld, lphase, lgate, unwound>>
AcquireO == /\ ophase = "acquire" /\ opos <= Len(Canonical)
            /\ locks[Canonical[opos]] = "none"
            /\ locks' = [locks EXCEPT ![Canonical[opos]] = "observer"]
            /\ oheld' = Append(oheld, Canonical[opos]) /\ opos' = opos + 1
            /\ UNCHANGED <<active, rphase, rpos, rheld, attachment, oldActive,
                 queued, notified, ncount, ephase, effectAfterReplace,
                 ophase, lphase, lgate, unwound>>
VisitO == /\ ophase = "acquire" /\ opos = Len(Canonical) + 1
          /\ ophase' = "release"
          /\ UNCHANGED <<locks, active, rphase, rpos, rheld, attachment,
               oldActive, queued, notified, ncount, ephase, effectAfterReplace,
               opos, oheld, lphase, lgate, unwound>>
ReleaseO == /\ ophase = "release" /\ Len(oheld) > 0
            /\ locks' = [locks EXCEPT ![oheld[Len(oheld)]] = "none"]
            /\ oheld' = Pop(oheld)
            /\ UNCHANGED <<active, rphase, rpos, rheld, attachment, oldActive,
                 queued, notified, ncount, ephase, effectAfterReplace,
                 ophase, opos, lphase, lgate, unwound>>
FinishO == /\ ophase = "release" /\ oheld = <<>> /\ ophase' = "done"
           /\ UNCHANGED <<locks, active, rphase, rpos, rheld, attachment,
                oldActive, queued, notified, ncount, ephase, effectAfterReplace,
                opos, oheld, lphase, lgate, unwound>>

\* Environment: one independent authority loss, or no loss, before completion.
ChooseLoss == /\ lphase = "choose"
              /\ (\/ /\ lphase' = "done" /\ UNCHANGED lgate
                  \/ /\ lphase' = "acquire" /\ lgate' \in ExternalGates)
              /\ UNCHANGED <<locks, active, rphase, rpos, rheld, attachment,
                   oldActive, queued, notified, ncount, ephase,
                   effectAfterReplace, ophase, opos, oheld, unwound>>
AcquireLoss == /\ lphase = "acquire" /\ locks[lgate] = "none"
               /\ locks' = [locks EXCEPT ![lgate] = "loss"] /\ lphase' = "invalidate"
               /\ UNCHANGED <<active, rphase, rpos, rheld, attachment,
                    oldActive, queued, notified, ncount, ephase,
                    effectAfterReplace, ophase, opos, oheld, lgate, unwound>>
InvalidateLoss == /\ lphase = "invalidate"
                  /\ active' = [active EXCEPT ![lgate] = FALSE] /\ lphase' = "release"
                  /\ UNCHANGED <<locks, rphase, rpos, rheld, attachment,
                       oldActive, queued, notified, ncount, ephase,
                       effectAfterReplace, ophase, opos, oheld, lgate, unwound>>
ReleaseLoss == /\ lphase = "release"
               /\ locks' = [locks EXCEPT ![lgate] = "none"] /\ lphase' = "done"
               /\ UNCHANGED <<active, rphase, rpos, rheld, attachment,
                    oldActive, queued, notified, ncount, ephase,
                    effectAfterReplace, ophase, opos, oheld, lgate, unwound>>

Next == AcquireR \/ AdmitR \/ CommitR \/ UnwindR \/ ReturnObserver \/ UnlockR
        \/ FinishR \/ NotifyR \/ AcquireE \/ EffectE \/ ReleaseE
        \/ StartO \/ AcquireO \/ VisitO \/ ReleaseO \/ FinishO
        \/ ChooseLoss \/ AcquireLoss \/ InvalidateLoss \/ ReleaseLoss

\* ================= FROZEN PROTOCOL PROPERTIES =================
TypeOK == /\ locks \in [Gates -> Actors] /\ active \in [Gates -> BOOLEAN]
          /\ attachment \in {"old", "new"} /\ oldActive \in [Documents -> BOOLEAN]
          /\ queued \subseteq Documents /\ notified \subseteq Documents /\ unwound \in BOOLEAN
          /\ effectAfterReplace \in BOOLEAN /\ ncount \in [Documents -> 0..1]
          /\ rpos \in 1..(Len(Requested) + 1) /\ opos \in 1..(Len(Canonical) + 1)
          /\ rheld \in Seq(Gates) /\ oheld \in Seq(Gates) /\ lgate \in Gates
          /\ rphase \in {"acquire", "admitted", "waitObserver", "unlock", "notify", "done"}
          /\ ephase \in {"acquire", "effect", "release", "done"}
          /\ ophase \in {"idle", "acquire", "release", "done"}
          /\ lphase \in {"choose", "acquire", "invalidate", "release", "done"}
AtomicReplace == attachment = "new" => (\A document \in Documents : ~oldActive[document])
NoStaleEffects == ~effectAfterReplace
ObserverOutsideGates == notified # {} => rheld = <<>>
NoAliasDoubleAcquire == IF rphase = "acquire" /\ rpos <= Len(Requested)
                       THEN locks[Requested[rpos]] # "retire" ELSE TRUE
NotificationConservation == /\ notified \subseteq queued
                           /\ \A document \in Documents :
                                ncount[document] = IF document \in notified THEN 1 ELSE 0
                           /\ (rphase = "done" => queued = notified)
Terminal == /\ rphase = "done" /\ ephase = "done" /\ lphase = "done"
            /\ (queued = {} \/ (ObservedDocument \in notified /\ ophase = "done"))
EventuallyTerminal == <>Terminal
\* ================= END FROZEN PROPERTIES ======================

\* Every nonstuttering step consumes a finite protocol stage; no retries/cycles.
\* WF(Next) excludes scheduler-only infinite stuttering, not blocked deadlocks.
Spec == Init /\ [][Next]_vars /\ WF_vars(Next)
=============================================================================
