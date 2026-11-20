//! Native creator evidence, never a renderer payload or a numeric destroy event.
use crate::{ViewId, runtime::RendererGeneration};
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeClosureOutcome {
    Closed,
    NotCreated,
}

/// Trusted native creation assembly retains this one-shot issuer privately.
/// Mint a receipt only from the captured browser reservation's terminal callback,
/// never from a process loss, a revoked generation or a ViewId-only notification.
pub struct NativeClosureIssuer {
    reservation: NativeClosureReservation,
}

struct ClosureIdentity {
    view: ViewId,
    created: RendererGeneration,
    ownership: Mutex<BrowserOwnership>,
}

enum BrowserPhase {
    Creating(Arc<BrowserOwnerIdentity>),
    Owned(Arc<BrowserOwnerIdentity>),
    Moving {
        owner: Arc<BrowserOwnerIdentity>,
        identity: super::PlacementMoveIdentity,
    },
    Closing {
        owner: Arc<BrowserOwnerIdentity>,
        dispatching: bool,
    },
    Closed(Option<Arc<BrowserOwnerIdentity>>),
}

struct BrowserOwnership {
    phase: BrowserPhase,
    successors: Option<Box<BrowserSuccessor>>,
    placement: Option<super::PlacementMoveIssuer>,
    placement_successors: Vec<super::PlacementSuperseded>,
}

struct BrowserOwnerIdentity;

struct BrowserSuccessor {
    old: Arc<BrowserOwnerIdentity>,
    successor: Arc<BrowserOwnerIdentity>,
    previous: Option<Box<BrowserSuccessor>>,
}

pub struct NativeBrowserReplacement {
    original: NativeBrowserOwner,
    successor: NativeBrowserOwner,
    entry: Box<BrowserSuccessor>,
}

/// Identity of one committed owner of the original browser reservation.
#[derive(Clone)]
pub struct NativeBrowserOwner {
    reservation: NativeClosureReservation,
    identity: Arc<BrowserOwnerIdentity>,
}

/// Proof that the caller is executing inside this reservation's short transition.
pub struct NativeBrowserTransition<'a> {
    reservation: &'a NativeClosureReservation,
    owner: &'a Arc<BrowserOwnerIdentity>,
    successor: Option<&'a Arc<BrowserOwnerIdentity>>,
    _guard: std::marker::PhantomData<&'a mut ()>,
}

impl NativeBrowserTransition<'_> {
    pub fn matches(&self, reservation: &NativeClosureReservation) -> bool {
        self.reservation.same_reservation(reservation)
    }
    pub fn matches_owner(&self, owner: &NativeBrowserOwner) -> bool {
        self.reservation.same_reservation(&owner.reservation)
            && Arc::ptr_eq(self.owner, &owner.identity)
    }
    pub fn proposes_owner(&self, owner: &NativeBrowserOwner) -> bool {
        self.reservation.same_reservation(&owner.reservation)
            && self
                .successor
                .is_some_and(|successor| Arc::ptr_eq(successor, &owner.identity))
    }
}

/// Proof of the exact move's inverse settlement, not a fresh effect admission.
pub struct NativeBrowserCompensationTransition<'a>(NativeBrowserTransition<'a>);

impl NativeBrowserCompensationTransition<'_> {
    pub fn matches(&self, reservation: &NativeClosureReservation) -> bool {
        self.0.matches(reservation)
    }
    pub fn matches_owner(&self, owner: &NativeBrowserOwner) -> bool {
        self.0.matches_owner(owner)
    }
}

/// Evidence of an actual committed replacement, never a lost numeric view.
#[derive(Clone)]
pub struct NativeBrowserSuperseded {
    reservation: NativeClosureReservation,
    old: Arc<BrowserOwnerIdentity>,
    successor: Arc<BrowserOwnerIdentity>,
}

impl NativeBrowserSuperseded {
    pub fn matches(
        &self,
        reservation: &NativeClosureReservation,
        owner: &NativeBrowserOwner,
    ) -> bool {
        self.reservation.same_reservation(reservation)
            && owner.reservation.same_reservation(reservation)
            && Arc::ptr_eq(&self.old, &owner.identity)
    }
    pub fn successor_matches(&self, owner: &NativeBrowserOwner) -> bool {
        self.reservation.same_reservation(&owner.reservation)
            && Arc::ptr_eq(&self.successor, &owner.identity)
    }
}

/// Non-cloneable right to request the original browser's close, retained on failure.
pub struct NativeBrowserClosePermit {
    reservation: NativeClosureReservation,
    owner: Arc<BrowserOwnerIdentity>,
}

/// Exclusive accepted placement operation. Dropping it never claims compensation.
pub struct NativeBrowserMovePermit {
    reservation: NativeClosureReservation,
    owner: Arc<BrowserOwnerIdentity>,
    issuer: super::PlacementMoveIssuer,
    identity: super::PlacementMoveIdentity,
}

pub enum NativeBrowserCloseClaim<T> {
    Won {
        permit: NativeBrowserClosePermit,
        value: T,
    },
    Superseded(NativeBrowserSuperseded),
}

impl NativeBrowserOwner {
    pub fn same_owner(&self, other: &Self) -> bool {
        self.reservation.same_reservation(&other.reservation)
            && Arc::ptr_eq(&self.identity, &other.identity)
    }
    pub fn reservation(&self) -> &NativeClosureReservation {
        &self.reservation
    }
}

/// The actor retains the exact predispatch reservation. Creating a second
/// issuer for the same numeric view and generation cannot satisfy this guard.
#[derive(Clone)]
pub struct NativeClosureReservation(Arc<ClosureIdentity>);

impl NativeClosureReservation {
    pub fn same_reservation(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
    pub fn view(&self) -> ViewId {
        self.0.view
    }
    pub fn created(&self) -> &RendererGeneration {
        &self.0.created
    }
    pub fn matches(&self, view: ViewId, created: &RendererGeneration) -> bool {
        self.0.view == view && self.0.created.same_generation(created)
    }

    /// Run the first actual actor/manager insertion in the same transition.
    pub fn with_initial_owner<T>(
        &self,
        owner: &NativeBrowserOwner,
        actor: impl FnOnce(&NativeBrowserTransition<'_>) -> Result<T, String>,
    ) -> Result<T, String> {
        if !owner.reservation.same_reservation(self) {
            return Err("The native browser owner does not match".into());
        }
        let mut state = self
            .0
            .ownership
            .lock()
            .map_err(|_| "The native browser arbiter is unavailable")?;
        if !matches!(&state.phase, BrowserPhase::Creating(current) if Arc::ptr_eq(current, &owner.identity))
        {
            return Err("The native browser is not available for initial adoption".into());
        }
        let value = actor(&NativeBrowserTransition {
            reservation: self,
            owner: &owner.identity,
            successor: None,
            _guard: std::marker::PhantomData,
        })?;
        state.phase = BrowserPhase::Owned(owner.identity.clone());
        Ok(value)
    }

    /// Run one short mutation of the currently committed actor/manager owner.
    pub fn with_owner<T>(
        &self,
        owner: &NativeBrowserOwner,
        actor: impl FnOnce(&NativeBrowserTransition<'_>) -> Result<T, String>,
    ) -> Result<T, String> {
        if !owner.reservation.same_reservation(self) {
            return Err("The native browser owner does not match".into());
        }
        let state = self
            .0
            .ownership
            .lock()
            .map_err(|_| "The native browser arbiter is unavailable")?;
        if !matches!(&state.phase, BrowserPhase::Owned(current) if Arc::ptr_eq(current, &owner.identity))
        {
            return Err("The native browser owner is no longer current".into());
        }
        actor(&NativeBrowserTransition {
            reservation: self,
            owner: &owner.identity,
            successor: None,
            _guard: std::marker::PhantomData,
        })
    }

    /// Admit one native placement operation under the original browser owner.
    pub fn begin_move<T>(
        &self,
        owner: &NativeBrowserOwner,
        prepare: impl FnOnce(&NativeBrowserTransition<'_>) -> Result<T, String>,
    ) -> Result<(NativeBrowserMovePermit, T), String> {
        if !owner.reservation.same_reservation(self) {
            return Err("The native browser owner does not match".into());
        }
        let mut state = self
            .0
            .ownership
            .lock()
            .map_err(|_| "The native browser arbiter is unavailable")?;
        if !matches!(&state.phase, BrowserPhase::Owned(current) if Arc::ptr_eq(current, &owner.identity))
        {
            return Err("The native browser cannot begin a placement move".into());
        }
        state
            .placement_successors
            .try_reserve(1)
            .map_err(|_| "The native placement transition cannot be recorded")?;
        let (issuer, identity) = super::PlacementMoveIssuer::new();
        let value = prepare(&NativeBrowserTransition {
            reservation: self,
            owner: &owner.identity,
            successor: None,
            _guard: std::marker::PhantomData,
        })?;
        state.phase = BrowserPhase::Moving {
            owner: owner.identity.clone(),
            identity: identity.clone(),
        };
        Ok((
            NativeBrowserMovePermit {
                reservation: self.clone(),
                owner: owner.identity.clone(),
                issuer,
                identity,
            },
            value,
        ))
    }

    pub fn placement_superseded(
        &self,
        identity: &super::PlacementMoveIdentity,
    ) -> Result<Option<super::PlacementSuperseded>, String> {
        let state = self
            .0
            .ownership
            .lock()
            .map_err(|_| "The native browser arbiter is unavailable")?;
        Ok(state
            .placement_successors
            .iter()
            .find(|proof| proof.matches(identity))
            .cloned())
    }

    /// Prepare allocations before entering any native authority gate.
    pub fn prepare_replacement(
        &self,
        owner: &NativeBrowserOwner,
    ) -> Result<NativeBrowserReplacement, String> {
        if !owner.reservation.same_reservation(self) {
            return Err("The native browser owner does not match".into());
        }
        let successor = NativeBrowserOwner {
            reservation: self.clone(),
            identity: Arc::new(BrowserOwnerIdentity),
        };
        let entry = Box::new(BrowserSuccessor {
            old: owner.identity.clone(),
            successor: successor.identity.clone(),
            previous: None,
        });
        Ok(NativeBrowserReplacement {
            original: owner.clone(),
            successor,
            entry,
        })
    }

    /// Fence replacement before scheduling any native close. Actor mutation is short and synchronous.
    pub fn claim_close<T>(
        &self,
        owner: &NativeBrowserOwner,
        actor: impl FnOnce(&NativeBrowserTransition<'_>) -> Result<T, String>,
    ) -> Result<NativeBrowserCloseClaim<T>, String> {
        if !owner.reservation.same_reservation(self) {
            return Err("The native browser owner does not match".into());
        }
        let mut state = self
            .0
            .ownership
            .lock()
            .map_err(|_| "The native browser arbiter is unavailable")?;
        if matches!(&state.phase, BrowserPhase::Creating(current) | BrowserPhase::Owned(current) if Arc::ptr_eq(current, &owner.identity))
        {
            let value = actor(&NativeBrowserTransition {
                reservation: self,
                owner: &owner.identity,
                successor: None,
                _guard: std::marker::PhantomData,
            })?;
            state.phase = BrowserPhase::Closing {
                owner: owner.identity.clone(),
                dispatching: false,
            };
            return Ok(NativeBrowserCloseClaim::Won {
                permit: NativeBrowserClosePermit {
                    reservation: self.clone(),
                    owner: owner.identity.clone(),
                },
                value,
            });
        }
        let mut previous = state.successors.as_deref();
        while let Some(entry) = previous {
            if Arc::ptr_eq(&entry.old, &owner.identity) {
                return Ok(NativeBrowserCloseClaim::Superseded(
                    NativeBrowserSuperseded {
                        reservation: self.clone(),
                        old: owner.identity.clone(),
                        successor: entry.successor.clone(),
                    },
                ));
            }
            previous = entry.previous.as_deref();
        }
        Err("The native browser close claim is no longer available".into())
    }
}

impl NativeBrowserReplacement {
    pub fn original(&self) -> &NativeBrowserOwner {
        &self.original
    }
    pub fn successor(&self) -> &NativeBrowserOwner {
        &self.successor
    }

    pub fn commit<T>(
        self,
        actor: impl FnOnce(&NativeBrowserTransition<'_>, &NativeBrowserOwner) -> Result<T, String>,
    ) -> Result<(NativeBrowserOwner, NativeBrowserSuperseded, T), String> {
        let Self {
            original,
            successor,
            mut entry,
        } = self;
        let reservation = &original.reservation;
        let mut state = reservation
            .0
            .ownership
            .lock()
            .map_err(|_| "The native browser arbiter is unavailable")?;
        if !matches!(&state.phase, BrowserPhase::Owned(current) if Arc::ptr_eq(current, &original.identity))
        {
            return Err("The native browser owner cannot be replaced".into());
        }
        let value = actor(
            &NativeBrowserTransition {
                reservation,
                owner: &original.identity,
                successor: Some(&successor.identity),
                _guard: std::marker::PhantomData,
            },
            &successor,
        )?;
        entry.previous = state.successors.take();
        state.successors = Some(entry);
        state.phase = BrowserPhase::Owned(successor.identity.clone());
        let evidence = NativeBrowserSuperseded {
            reservation: reservation.clone(),
            old: original.identity.clone(),
            successor: successor.identity.clone(),
        };
        Ok((successor, evidence, value))
    }
}

impl NativeBrowserMovePermit {
    pub fn identity(&self) -> &super::PlacementMoveIdentity {
        &self.identity
    }

    /// Commit the actual native placement and its metadata in one short transition.
    /// An error leaves the permit active for retry or a real inverse operation.
    pub fn commit<T>(
        &mut self,
        apply: impl FnOnce(&NativeBrowserTransition<'_>) -> Result<T, String>,
    ) -> Result<T, String> {
        let mut state = self
            .reservation
            .0
            .ownership
            .lock()
            .map_err(|_| "The native browser arbiter is unavailable")?;
        if !matches!(&state.phase, BrowserPhase::Moving { owner, identity } if Arc::ptr_eq(owner, &self.owner) && identity.same_move(&self.identity))
        {
            return Err("The native placement move is no longer active".into());
        }
        let superseded = state
            .placement
            .as_ref()
            .map(|issuer| issuer.clone().superseded(self.identity.clone()))
            .transpose()?;
        let value = apply(&NativeBrowserTransition {
            reservation: &self.reservation,
            owner: &self.owner,
            successor: None,
            _guard: std::marker::PhantomData,
        })?;
        if let Some(proof) = superseded {
            state.placement_successors.push(proof);
        }
        state.placement = Some(self.issuer.clone());
        state.phase = BrowserPhase::Owned(self.owner.clone());
        Ok(value)
    }

    /// Release only after the caller has observed the actual inverse native operation.
    pub fn compensated<T>(
        &mut self,
        apply: impl FnOnce(&NativeBrowserCompensationTransition<'_>) -> Result<T, String>,
    ) -> Result<T, String> {
        let mut state = self
            .reservation
            .0
            .ownership
            .lock()
            .map_err(|_| "The native browser arbiter is unavailable")?;
        if !matches!(&state.phase, BrowserPhase::Moving { owner, identity } if Arc::ptr_eq(owner, &self.owner) && identity.same_move(&self.identity))
        {
            return Err("The native placement move is no longer active".into());
        }
        let value = apply(&NativeBrowserCompensationTransition(
            NativeBrowserTransition {
                reservation: &self.reservation,
                owner: &self.owner,
                successor: None,
                _guard: std::marker::PhantomData,
            },
        ))?;
        state.phase = BrowserPhase::Owned(self.owner.clone());
        Ok(value)
    }
}

impl NativeBrowserClosePermit {
    pub fn reservation(&self) -> &NativeClosureReservation {
        &self.reservation
    }

    /// Admit a bounded mutation for the same closing owner before native close dispatch.
    /// Retain this non-cloneable permit across any separately queued native work.
    pub fn with_closing<T>(
        &mut self,
        effect: impl FnOnce(&NativeBrowserTransition<'_>) -> Result<T, String>,
    ) -> Result<T, String> {
        let state = self
            .reservation
            .0
            .ownership
            .lock()
            .map_err(|_| "The native browser arbiter is unavailable")?;
        if !matches!(&state.phase, BrowserPhase::Closing { owner, dispatching: false } if Arc::ptr_eq(owner, &self.owner))
        {
            return Err("The original browser is not available for closing placement".into());
        }
        effect(&NativeBrowserTransition {
            reservation: &self.reservation,
            owner: &self.owner,
            successor: None,
            _guard: std::marker::PhantomData,
        })
    }

    /// Settle metadata only after the same closing operation's native inverse completed.
    pub fn with_closing_compensation<T>(
        &mut self,
        effect: impl FnOnce(&NativeBrowserCompensationTransition<'_>) -> Result<T, String>,
    ) -> Result<T, String> {
        let state = self
            .reservation
            .0
            .ownership
            .lock()
            .map_err(|_| "The native browser arbiter is unavailable")?;
        if !matches!(&state.phase, BrowserPhase::Closing { owner, dispatching: false } if Arc::ptr_eq(owner, &self.owner))
        {
            return Err("The original browser is not available for closing compensation".into());
        }
        effect(&NativeBrowserCompensationTransition(
            NativeBrowserTransition {
                reservation: &self.reservation,
                owner: &self.owner,
                successor: None,
                _guard: std::marker::PhantomData,
            },
        ))
    }

    /// Claim one native dispatch. Call `dispatch_failed` only for its real failure.
    pub fn begin_dispatch(&mut self) -> Result<(), String> {
        let mut state = self
            .reservation
            .0
            .ownership
            .lock()
            .map_err(|_| "The native browser arbiter is unavailable")?;
        match &mut state.phase {
            BrowserPhase::Closing { owner, dispatching }
                if Arc::ptr_eq(owner, &self.owner) && !*dispatching =>
            {
                *dispatching = true;
                Ok(())
            }
            _ => Err("The native browser close permit cannot be dispatched".into()),
        }
    }

    pub fn dispatch_failed(&mut self) -> Result<(), String> {
        let mut state = self
            .reservation
            .0
            .ownership
            .lock()
            .map_err(|_| "The native browser arbiter is unavailable")?;
        match &mut state.phase {
            BrowserPhase::Closing { owner, dispatching }
                if Arc::ptr_eq(owner, &self.owner) && *dispatching =>
            {
                *dispatching = false;
                Ok(())
            }
            _ => Err("The native browser close dispatch is no longer retryable".into()),
        }
    }
}

impl NativeClosureIssuer {
    pub fn new(view: ViewId, created: RendererGeneration) -> (Self, NativeClosureReservation) {
        let (issuer, reservation, _) = Self::new_with_owner(view, created);
        (issuer, reservation)
    }

    pub fn new_with_owner(
        view: ViewId,
        created: RendererGeneration,
    ) -> (Self, NativeClosureReservation, NativeBrowserOwner) {
        let identity = Arc::new(BrowserOwnerIdentity);
        let reservation = NativeClosureReservation(Arc::new(ClosureIdentity {
            view,
            created,
            ownership: Mutex::new(BrowserOwnership {
                phase: BrowserPhase::Creating(identity.clone()),
                successors: None,
                placement: None,
                placement_successors: Vec::new(),
            }),
        }));
        let owner = NativeBrowserOwner {
            reservation: reservation.clone(),
            identity,
        };
        (
            Self {
                reservation: reservation.clone(),
            },
            reservation,
            owner,
        )
    }
    pub fn closed(self) -> NativeClosureReceipt {
        self.finish(NativeClosureOutcome::Closed)
    }
    pub fn not_created(self) -> NativeClosureReceipt {
        self.finish(NativeClosureOutcome::NotCreated)
    }
    fn finish(self, outcome: NativeClosureOutcome) -> NativeClosureReceipt {
        {
            let mut state = self
                .reservation
                .0
                .ownership
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let owner = match &state.phase {
                BrowserPhase::Creating(owner)
                | BrowserPhase::Owned(owner)
                | BrowserPhase::Moving { owner, .. }
                | BrowserPhase::Closing { owner, .. } => Some(owner.clone()),
                BrowserPhase::Closed(owner) => owner.clone(),
            };
            state.phase = BrowserPhase::Closed(owner);
        }
        NativeClosureReceipt {
            reservation: self.reservation,
            outcome,
        }
    }
}
/// Closed-phase actor retirement evidence. This is not live effect authority.
pub struct NativeClosedBrowserTransition<'a> {
    reservation: &'a NativeClosureReservation,
    owner: &'a Arc<BrowserOwnerIdentity>,
}
impl NativeClosedBrowserTransition<'_> {
    pub fn matches(
        &self,
        reservation: &NativeClosureReservation,
        owner: &NativeBrowserOwner,
    ) -> bool {
        self.reservation.same_reservation(reservation)
            && owner.reservation.same_reservation(reservation)
            && Arc::ptr_eq(self.owner, &owner.identity)
    }
}

/// Terminal actor-retirement evidence for either actual closure or a proven
/// uncreated browser. It does not admit native or live-owner effects.
pub struct NativeTerminalBrowserTransition<'a> {
    reservation: &'a NativeClosureReservation,
    owner: &'a Arc<BrowserOwnerIdentity>,
}
impl NativeTerminalBrowserTransition<'_> {
    pub fn matches(
        &self,
        reservation: &NativeClosureReservation,
        owner: &NativeBrowserOwner,
    ) -> bool {
        self.reservation.same_reservation(reservation)
            && owner.reservation.same_reservation(reservation)
            && Arc::ptr_eq(self.owner, &owner.identity)
    }
}

/// Observation of this exact native reservation's terminal state. This grants
/// no effect authority; actor receipts still verify their insertion identity.
#[derive(Clone)]
pub struct NativeClosureReceipt {
    reservation: NativeClosureReservation,
    outcome: NativeClosureOutcome,
}

/// The original operation either observed its browser's actual terminal state,
/// or was superseded by an exact committed browser owner.
#[derive(Clone)]
pub enum NativeBrowserRetirement {
    Closed(NativeClosureReceipt),
    Superseded(NativeBrowserSuperseded),
}

impl NativeClosureReceipt {
    pub fn matches(
        &self,
        reservation: &NativeClosureReservation,
        view: ViewId,
        created: &RendererGeneration,
    ) -> bool {
        Arc::ptr_eq(&self.reservation.0, &reservation.0) && reservation.matches(view, created)
    }
    pub fn outcome(&self) -> NativeClosureOutcome {
        self.outcome
    }

    /// Identity retained by this terminal observation, not a live close grant.
    pub fn terminal_owner(&self) -> Option<NativeBrowserOwner> {
        let state = self.reservation.0.ownership.lock().ok()?;
        let BrowserPhase::Closed(Some(owner)) = &state.phase else {
            return None;
        };
        Some(NativeBrowserOwner {
            reservation: self.reservation.clone(),
            identity: owner.clone(),
        })
    }

    /// Retire this exact owner's actor after a terminal native observation.
    /// The actor must claim its own insertion once. The callback is a short
    /// state transition and must not perform IO, native calls, waits or callbacks.
    pub fn with_terminal_owner<T>(
        &self,
        owner: &NativeBrowserOwner,
        actor: impl FnOnce(&NativeTerminalBrowserTransition<'_>) -> Result<T, String>,
    ) -> Result<T, String> {
        if !owner.reservation.same_reservation(&self.reservation) {
            return Err("The native browser terminal receipt does not match its owner".into());
        }
        let state = self
            .reservation
            .0
            .ownership
            .lock()
            .map_err(|_| "The native browser arbiter is unavailable")?;
        if !matches!(&state.phase, BrowserPhase::Closed(Some(current)) if Arc::ptr_eq(current, &owner.identity))
        {
            return Err("The terminal native browser belongs to another owner".into());
        }
        actor(&NativeTerminalBrowserTransition {
            reservation: &self.reservation,
            owner: &owner.identity,
        })
    }

    /// Admit a short actor-only retirement after the original browser closed.
    /// The actor must separately claim its own original insertion exactly once.
    /// No native calls, waits, IO or external callbacks may run in this transition.
    pub fn with_closed_owner<T>(
        &self,
        owner: &NativeBrowserOwner,
        actor: impl FnOnce(&NativeClosedBrowserTransition<'_>) -> Result<T, String>,
    ) -> Result<T, String> {
        if self.outcome != NativeClosureOutcome::Closed
            || !owner.reservation.same_reservation(&self.reservation)
        {
            return Err("The native browser closure does not match its owner".into());
        }
        let state = self
            .reservation
            .0
            .ownership
            .lock()
            .map_err(|_| "The native browser arbiter is unavailable")?;
        if !matches!(&state.phase, BrowserPhase::Closed(Some(current)) if Arc::ptr_eq(current, &owner.identity))
        {
            return Err("The closed native browser belongs to another owner".into());
        }
        actor(&NativeClosedBrowserTransition {
            reservation: &self.reservation,
            owner: &owner.identity,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closed_transition_only_accepts_final_owner_and_not_not_created() {
        let (issuer, reservation, first) =
            NativeClosureIssuer::new_with_owner(ViewId(3), RendererGeneration::new());
        reservation.with_initial_owner(&first, |_| Ok(())).unwrap();
        let (last, _, ()) = reservation
            .prepare_replacement(&first)
            .unwrap()
            .commit(|_, _| Ok(()))
            .unwrap();
        let receipt = issuer.closed();
        assert!(receipt.with_closed_owner(&first, |_| Ok(())).is_err());
        assert!(
            receipt
                .with_closed_owner(&last, |transition| {
                    assert!(transition.matches(&reservation, &last));
                    assert!(!transition.matches(&reservation, &first));
                    Ok(())
                })
                .is_ok()
        );
        assert!(receipt.with_terminal_owner(&first, |_| Ok(())).is_err());
        assert!(
            receipt
                .with_terminal_owner(&last, |transition| {
                    assert!(transition.matches(&reservation, &last));
                    Ok(())
                })
                .is_ok()
        );
        let (other, other_reservation, owner) =
            NativeClosureIssuer::new_with_owner(ViewId(3), RendererGeneration::new());
        let absent = other.not_created();
        assert!(absent.with_closed_owner(&owner, |_| Ok(())).is_err());
        assert!(
            absent
                .with_terminal_owner(&owner, |transition| {
                    assert!(transition.matches(&other_reservation, &owner));
                    Ok(())
                })
                .is_ok()
        );
        assert!(absent.with_terminal_owner(&last, |_| Ok(())).is_err());
    }

    #[test]
    fn exact_closed_generation_survives_revocation_but_not_replacement() {
        let created = RendererGeneration::new();
        let (issuer, reservation) = NativeClosureIssuer::new(ViewId(4), created.clone());
        created.revoke();
        let receipt = issuer.closed();
        assert!(receipt.matches(&reservation, ViewId(4), &created));
        assert!(!receipt.matches(&reservation, ViewId(5), &created));
        assert!(!receipt.matches(&reservation, ViewId(4), &RendererGeneration::new()));
        let (forged, _) = NativeClosureIssuer::new(ViewId(4), created.clone());
        assert!(!forged.closed().matches(&reservation, ViewId(4), &created));
        assert_eq!(receipt.outcome(), NativeClosureOutcome::Closed);
    }

    #[test]
    fn unsubmitted_reservation_has_a_distinct_terminal_outcome() {
        let created = RendererGeneration::new();
        let (issuer, reservation) = NativeClosureIssuer::new(ViewId(6), created.clone());
        let receipt = issuer.not_created();
        assert!(receipt.matches(&reservation, ViewId(6), &created));
        assert_eq!(receipt.outcome(), NativeClosureOutcome::NotCreated);
    }

    #[test]
    fn committed_successor_fences_old_close_and_failed_replacement_does_not() {
        let (_, reservation, original) =
            NativeClosureIssuer::new_with_owner(ViewId(8), RendererGeneration::new());
        reservation
            .with_initial_owner(&original, |transition| {
                assert!(transition.matches(&reservation));
                Ok(())
            })
            .unwrap();
        assert!(
            reservation
                .prepare_replacement(&original)
                .unwrap()
                .commit(|_, _| Err::<(), _>("actor rejected".into()))
                .is_err()
        );
        assert!(reservation.with_owner(&original, |_| Ok(())).is_ok());
        let (successor, evidence, ()) = reservation
            .prepare_replacement(&original)
            .unwrap()
            .commit(|_, _| Ok(()))
            .unwrap();
        assert!(evidence.matches(&reservation, &original));
        assert!(evidence.successor_matches(&successor));
        let claim = reservation
            .claim_close(&original, |_| Err::<(), _>("must not run".into()))
            .unwrap();
        assert!(
            matches!(claim, NativeBrowserCloseClaim::Superseded(e) if e.matches(&reservation, &original))
        );
        assert!(reservation.with_owner(&successor, |_| Ok(())).is_ok());
    }

    #[test]
    fn placement_fences_close_until_real_settlement_and_proves_only_committed_successor() {
        let (_, reservation, owner) =
            NativeClosureIssuer::new_with_owner(ViewId(10), RendererGeneration::new());
        reservation.with_initial_owner(&owner, |_| Ok(())).unwrap();
        let (mut first, ()) = reservation.begin_move(&owner, |_| Ok(())).unwrap();
        let first_id = first.identity().clone();
        assert!(reservation.claim_close(&owner, |_| Ok(())).is_err());
        assert!(
            reservation
                .prepare_replacement(&owner)
                .unwrap()
                .commit(|_, _| Ok(()))
                .is_err()
        );
        assert!(
            first
                .commit(|_| Err::<(), _>("metadata failed".into()))
                .is_err()
        );
        assert!(reservation.claim_close(&owner, |_| Ok(())).is_err());
        first.commit(|_| Ok(())).unwrap();
        let (mut inverse, ()) = reservation.begin_move(&owner, |_| Ok(())).unwrap();
        inverse.compensated(|_| Ok(())).unwrap();
        assert!(
            reservation
                .placement_superseded(&first_id)
                .unwrap()
                .is_none()
        );
        let (mut second, ()) = reservation.begin_move(&owner, |_| Ok(())).unwrap();
        let second_id = second.identity().clone();
        assert!(
            reservation
                .placement_superseded(&first_id)
                .unwrap()
                .is_none()
        );
        second.commit(|_| Ok(())).unwrap();
        let proof = reservation
            .placement_superseded(&first_id)
            .unwrap()
            .unwrap();
        assert!(proof.matches(&first_id));
        assert!(proof.successor_matches(&second_id));
    }

    #[test]
    fn dropped_unsettled_placement_does_not_authorize_close() {
        let (_, reservation, owner) =
            NativeClosureIssuer::new_with_owner(ViewId(11), RendererGeneration::new());
        reservation.with_initial_owner(&owner, |_| Ok(())).unwrap();
        let (permit, ()) = reservation.begin_move(&owner, |_| Ok(())).unwrap();
        drop(permit);
        assert!(reservation.claim_close(&owner, |_| Ok(())).is_err());
        assert!(reservation.begin_move(&owner, |_| Ok(())).is_err());
    }

    #[test]
    fn close_winner_fences_replacement_and_retries_only_real_dispatch_failure() {
        let (issuer, reservation, owner) =
            NativeClosureIssuer::new_with_owner(ViewId(9), RendererGeneration::new());
        reservation.with_initial_owner(&owner, |_| Ok(())).unwrap();
        let NativeBrowserCloseClaim::Won { mut permit, .. } =
            reservation.claim_close(&owner, |_| Ok(())).unwrap()
        else {
            panic!("current owner must win the close");
        };
        assert!(
            reservation
                .prepare_replacement(&owner)
                .unwrap()
                .commit(|_, _| Ok(()))
                .is_err()
        );
        permit
            .with_closing(|transition| {
                assert!(transition.matches_owner(&owner));
                Ok(())
            })
            .unwrap();
        permit
            .with_closing_compensation(|transition| {
                assert!(transition.matches_owner(&owner));
                Ok(())
            })
            .unwrap();
        permit.begin_dispatch().unwrap();
        assert!(permit.with_closing(|_| Ok(())).is_err());
        assert!(permit.begin_dispatch().is_err());
        permit.dispatch_failed().unwrap();
        permit.begin_dispatch().unwrap();
        let receipt = issuer.closed();
        assert!(receipt.matches(&reservation, ViewId(9), &reservation.0.created));
        assert!(permit.dispatch_failed().is_err());
    }
}
