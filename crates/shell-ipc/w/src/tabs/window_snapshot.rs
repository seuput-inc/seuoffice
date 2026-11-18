//! Creation-bound native window observations for short core-only snapshot commits.
use std::sync::Arc;
use shell_contract::WindowId;

/// Implemented only by the native window owner (or its explicit synthetic fixture).
pub trait NativeWindowProof: Send + Sync {
    fn lock(&self) -> Box<dyn NativeWindowGuard + '_>;
    /// Schedule the actual native-window read without looking up a numeric ID.
    fn observe(self: Arc<Self>, done: Box<dyn FnOnce(Result<WindowObservation, String>) + Send>);
    /// Native UI action only; callers must separately admit the captured origin.
    fn set_title(&self, title: &str) -> Result<(), String>;
}

/// A held native window revision gate. No UI or worker wait is allowed while held.
pub trait NativeWindowGuard {
    fn revision(&self) -> u64;
    fn stable(&self) -> bool;
    fn destroyed(&self) -> bool;
}

#[derive(Clone)]
pub struct WindowReservation {
    window: WindowId,
    proof: Arc<dyn NativeWindowProof>,
}

impl WindowReservation {
    /// Called by the native constructor with the proof for this exact window.
    pub fn new(window: WindowId, proof: Arc<dyn NativeWindowProof>) -> Self { Self { window, proof } }
    pub fn window(&self) -> WindowId { self.window }
    pub fn observe(&self, done: Box<dyn FnOnce(Result<WindowObservation, String>) + Send>) {
        self.proof.clone().observe(done);
    }
    /// Must be called only by an already-authorized native UI action.
    pub fn set_title(&self, title: &str) -> Result<(), String> { self.proof.set_title(title) }
    pub fn same_reservation(&self, other: &Self) -> bool {
        self.window == other.window && Arc::ptr_eq(&self.proof, &other.proof)
    }
    fn key(&self) -> usize { Arc::as_ptr(&self.proof) as *const () as usize }
}

impl PartialEq for WindowReservation {
    fn eq(&self, other: &Self) -> bool { self.same_reservation(other) }
}
impl Eq for WindowReservation {}

#[derive(Clone)]
pub struct WindowObservation {
    reservation: WindowReservation,
    revision: u64,
    destroyed: bool,
    title: String,
}

impl WindowObservation {
    /// The native producer samples the actual UI title outside the gate and binds
    /// it to the revision it observed for the same creation-bound reservation.
    pub fn new(reservation: WindowReservation, revision: u64, destroyed: bool, title: String) -> Self {
        Self { reservation, revision, destroyed, title }
    }
    pub fn reservation(&self) -> &WindowReservation { &self.reservation }
    pub fn destroyed(&self) -> bool { self.destroyed }
    pub fn title(&self) -> &str { &self.title }
}

/// All locks are ordered and deduplicated before validating every observation.
/// The effect must be a short, pure core-state operation: no UI, disk or waits.
pub fn with_window_observations<T>(observations: &[WindowObservation], effect: impl FnOnce() -> T) -> Result<T, String> {
    let mut reservations: Vec<&WindowReservation> = observations.iter().map(|observation| &observation.reservation).collect();
    reservations.sort_unstable_by_key(|reservation| reservation.key());
    reservations.dedup_by(|left, right| Arc::ptr_eq(&left.proof, &right.proof));
    let guards: Vec<_> = reservations.iter().map(|reservation| reservation.proof.lock()).collect();
    for observation in observations {
        let index = reservations.binary_search_by_key(&observation.reservation.key(), |reservation| reservation.key())
            .map_err(|_| "A native window observation was not captured".to_owned())?;
        let guard = &guards[index];
        if !guard.stable() || guard.revision() != observation.revision || guard.destroyed() != observation.destroyed {
            return Err("A captured native window changed before the snapshot commit".into());
        }
    }
    Ok(effect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    struct Proof(Mutex<(u64, bool, bool)>);
    struct Guard<'a>(MutexGuard<'a, (u64, bool, bool)>);
    impl NativeWindowGuard for Guard<'_> {
        fn revision(&self) -> u64 { self.0.0 }
        fn destroyed(&self) -> bool { self.0.1 }
        fn stable(&self) -> bool { self.0.2 }
    }
    impl NativeWindowProof for Proof {
        fn lock(&self) -> Box<dyn NativeWindowGuard + '_> { Box::new(Guard(self.0.lock().unwrap())) }
        fn observe(self: Arc<Self>, done: Box<dyn FnOnce(Result<WindowObservation, String>) + Send>) {
            let (revision, destroyed, stable) = *self.0.lock().unwrap();
            if !stable { return done(Err("window is changing".into())); }
            done(Ok(WindowObservation::new(WindowReservation::new(WindowId(1), self), revision, destroyed, "title".into())));
        }
        fn set_title(&self, _title: &str) -> Result<(), String> {
            let mut state = self.0.lock().unwrap();
            if state.1 || !state.2 { return Err("window is unavailable".into()); }
            state.0 = state.0.checked_add(1).ok_or("window revision exhausted")?;
            Ok(())
        }
    }

    #[test]
    fn observation_rejects_mutation_and_revision_aba() {
        let proof = Arc::new(Proof(Mutex::new((1, false, true))));
        let reservation = WindowReservation::new(WindowId(1), proof.clone());
        let observation = WindowObservation::new(reservation, 1, false, "title".into());
        assert!(with_window_observations(&[observation.clone()], || ()).is_ok());
        *proof.0.lock().unwrap() = (1, false, false);
        assert!(with_window_observations(&[observation.clone()], || ()).is_err());
        *proof.0.lock().unwrap() = (2, false, true);
        assert!(with_window_observations(&[observation], || ()).is_err());
    }

    #[test]
    fn same_window_id_does_not_rebind_a_replacement_reservation() {
        let original = WindowReservation::new(WindowId(1), Arc::new(Proof(Mutex::new((1, false, true)))));
        let replacement = WindowReservation::new(WindowId(1), Arc::new(Proof(Mutex::new((1, false, true)))));
        assert!(!original.same_reservation(&replacement));
        assert!(original.same_reservation(&original.clone()));
    }
}
