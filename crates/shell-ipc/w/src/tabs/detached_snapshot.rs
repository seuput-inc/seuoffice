use super::state::TabsState;
use super::window_snapshot::{WindowObservation, WindowReservation, with_window_observations};
use main_core::shell::detached_windows::{
    DetachedSessionSnapshot, DetachedSnapshotPlan, DetachedWindowObservation,
};
use shell_contract::ViewId;
use std::rc::Rc;

pub(super) struct CapturedDetachedSnapshot {
    plan: DetachedSnapshotPlan<ViewId, WindowReservation>,
    observations: Vec<WindowObservation>,
}

impl CapturedDetachedSnapshot {
    /// The caller must compose this with its captured native operation admission.
    /// This closure performs no native I/O or asynchronous work.
    pub(super) fn commit(&self, state: &TabsState) -> Result<DetachedSessionSnapshot, String> {
        let observations = self
            .plan
            .entries()
            .iter()
            .zip(&self.observations)
            .map(|(entry, observation)| DetachedWindowObservation {
                rec: entry.rec,
                destroyed: observation.destroyed(),
                title: observation.title().to_owned(),
            })
            .collect::<Vec<_>>();
        with_window_observations(&self.observations, || {
            state
                .inner
                .borrow()
                .detached
                .commit_session_snapshot(&self.plan, &observations)
        })?
    }
}

/// Capture the exact creation-bound handles before any native window observation.
pub(super) fn capture(
    state: Rc<TabsState>,
    done: Box<dyn FnOnce(Result<CapturedDetachedSnapshot, String>)>,
) {
    let plan = state.inner.borrow().detached.prepare_session_snapshot();
    collect(state, plan, Vec::new(), done);
}

fn collect(
    state: Rc<TabsState>,
    plan: DetachedSnapshotPlan<ViewId, WindowReservation>,
    observations: Vec<WindowObservation>,
    done: Box<dyn FnOnce(Result<CapturedDetachedSnapshot, String>)>,
) {
    let Some(entry) = plan.entries().get(observations.len()) else {
        return done(Ok(CapturedDetachedSnapshot { plan, observations }));
    };
    let window = entry.window.clone();
    let expected = window.clone();
    let callback = crate::home_files::back_to_core(
        state.services.core.clone(),
        move |result: Result<WindowObservation, String>| match result {
            Ok(observation) if expected.same_reservation(observation.reservation()) => {
                let mut observations = observations;
                observations.push(observation);
                collect(state, plan, observations, done);
            }
            Ok(_) => done(Err(
                "The native window observation belongs to another reservation".into(),
            )),
            Err(error) => done(Err(error)),
        },
    );
    window.observe(callback);
}
