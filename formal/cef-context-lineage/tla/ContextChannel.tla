------------------------- MODULE ContextChannel -------------------------
EXTENDS Naturals
VARIABLES rendererLive, registerPending, closePending, browserRegistered,
          browserRevoked, stalePhysical, effectAfterRevoke
vars == <<rendererLive, registerPending, closePending, browserRegistered,
          browserRevoked, stalePhysical, effectAfterRevoke>>

Init == /\ rendererLive = TRUE /\ registerPending = FALSE
        /\ closePending = FALSE /\ browserRegistered = FALSE
        /\ browserRevoked = FALSE /\ stalePhysical = FALSE
        /\ effectAfterRevoke = FALSE

SendRegister == /\ rendererLive /\ ~registerPending /\ ~browserRegistered
                /\ registerPending' = TRUE
                /\ UNCHANGED <<rendererLive, closePending, browserRegistered,
                               browserRevoked, stalePhysical, effectAfterRevoke>>

ReleaseLocally == /\ rendererLive
                  /\ rendererLive' = FALSE /\ closePending' = TRUE
                  /\ UNCHANGED <<registerPending, browserRegistered,
                                 browserRevoked, stalePhysical, effectAfterRevoke>>

\* The browser may logically authorize the unchanged document after a physical
\* renderer release it has not observed. That is not an effect after browser
\* revocation. Exact V8 evaluation has a separate, stronger liveness obligation.
ReceiveRegister == /\ registerPending
                   /\ registerPending' = FALSE
                   /\ IF browserRevoked
                        THEN UNCHANGED <<browserRegistered, stalePhysical>>
                        ELSE /\ browserRegistered' = TRUE
                             /\ stalePhysical' = stalePhysical \/ ~rendererLive
                   /\ UNCHANGED <<rendererLive, closePending, browserRevoked,
                                  effectAfterRevoke>>

\* Navigation, authoritative lifecycle invalidation or observed pipe closure.
BrowserRevoke == /\ ~browserRevoked
                 /\ browserRevoked' = TRUE /\ browserRegistered' = FALSE
                 /\ UNCHANGED <<rendererLive, registerPending, closePending,
                                stalePhysical, effectAfterRevoke>>

ReceiveClose == /\ closePending /\ ~registerPending
                /\ closePending' = FALSE
                /\ browserRevoked' = TRUE /\ browserRegistered' = FALSE
                /\ UNCHANGED <<rendererLive, registerPending, stalePhysical,
                               effectAfterRevoke>>

\* The browser's authority gate serializes effects against BrowserRevoke.
Effect == /\ browserRegistered
          /\ effectAfterRevoke' = effectAfterRevoke \/ browserRevoked
          /\ UNCHANGED <<rendererLive, registerPending, closePending,
                         browserRegistered, browserRevoked, stalePhysical>>

Next == SendRegister \/ ReleaseLocally \/ ReceiveRegister \/ BrowserRevoke \/
        ReceiveClose \/ Effect
Spec == Init /\ [][Next]_vars
NoReleasedRegistration == ~stalePhysical
NoEffectAfterBrowserRevoke == ~effectAfterRevoke
=============================================================================
