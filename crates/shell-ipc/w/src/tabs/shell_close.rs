//! The unsaved-changes guard of the shell window (`installShellCloseGuard`):
//! closing the window, by its close box, a quit or the updater's restart, walks
//! every dirty tab of every editor family through the same Save / Don't Save /
//! Cancel prompt a tab close runs, and any Cancel keeps the window open.
//!
//! The handler is main-core's [`ShellCloseGuard`]; this carries its steps out
//! through the editors, as `drive_close` does for one tab. The prompts only ask:
//! a tab that agreed is not closed until the window is. A failure to ask is a
//! Cancel, so nothing unsaved is dropped for want of an answer.

use main_core::shell::tab_manager::{TabHost, TabKind, TabManager};
use main_core::shell::window_close_guard::{
    CloseGuardHost, CloseGuardTab, CloseStep, FAMILIES, Family, ShellCloseGuard,
};
use shell_contract::editors::{Then, editor_for};
use shell_contract::error::IpcError;
use shell_contract::ids::{ViewId, WindowId};

use super::bridge::Bridge;
use super::host::RetainedSpareBinding;
use super::state::{Inner, TabsState};

/// The guard of one shell window. It is per window, as `installShellCloseGuard`
/// is: a shell window made again (macOS) starts with a fresh one.
pub(super) struct ShellClose {
    window: WindowId,
    guard: ShellCloseGuard<ViewId>,
    /// the dirty PDF views of the walk in progress, for a veto
    dirty_pdf: Vec<ViewId>,
}

impl ShellClose {
    fn new(window: WindowId) -> Self {
        ShellClose {
            window,
            guard: ShellCloseGuard::new(),
            dirty_pdf: Vec::new(),
        }
    }
}

type Lists = Vec<(Family, Vec<CloseGuardTab<ViewId>>)>;

fn kind_of(family: Family) -> TabKind {
    match family {
        Family::Sheets => TabKind::Sheets,
        Family::Pdf => TabKind::Pdf,
        Family::Markdown => TabKind::Markdown,
        Family::Html => TabKind::Html,
        Family::Mindmap => TabKind::Mindmap,
        Family::Hwp => TabKind::Hwp,
        Family::Epub => TabKind::Epub,
        Family::Diagram => TabKind::Diagram,
        Family::Slides => TabKind::Slides,
        Family::Docs => TabKind::Docs,
    }
}

/// The lists the handler reads at the `close` event, in its order. A query that
/// fails fails the whole read.
fn dirty_lists(
    manager: &TabManager<ViewId, RetainedSpareBinding>,
    bridge: &mut Bridge,
) -> Result<Lists, String> {
    let host: &mut dyn TabHost<ViewId, RetainedSpareBinding> = bridge;
    let mut lists = Vec::with_capacity(FAMILIES.len());
    for family in FAMILIES {
        let tabs = match family {
            Family::Docs => manager.live_tabs(TabKind::Docs),
            Family::Hwp => manager.dirty_or_memo_hwp_tabs(host).map_err(|e| e.0)?,
            other => manager.dirty_tabs(host, kind_of(other)).map_err(|e| e.0)?,
        };
        let tabs = tabs
            .into_iter()
            .map(|(id, view)| CloseGuardTab {
                id,
                web_contents: view,
            })
            .collect();
        lists.push((family, tabs));
    }
    Ok(lists)
}

/// What the handler calls on the window and the manager
struct GuardHost<'a> {
    manager: &'a mut TabManager<ViewId, RetainedSpareBinding>,
    bridge: &'a mut Bridge,
    lists: Lists,
    dirty_pdf: &'a [ViewId],
}

impl CloseGuardHost for GuardHost<'_> {
    type Contents = ViewId;

    fn tabs(&mut self, family: Family) -> Vec<CloseGuardTab<ViewId>> {
        self.lists
            .iter()
            .find(|(listed, _)| *listed == family)
            .map(|(_, tabs)| tabs.clone())
            .unwrap_or_default()
    }

    fn activate_tab(&mut self, id: &str) -> Result<(), String> {
        self.manager.activate_tab(&mut *self.bridge, id);
        Ok(())
    }

    fn begin_exclusive_prompt(&mut self) {
        self.manager.begin_exclusive_prompt();
    }

    fn end_exclusive_prompt(&mut self) {
        self.manager.end_exclusive_prompt(&mut *self.bridge);
    }

    fn reset_sheets_shutting_down(&mut self) {
        self.bridge.hooks.reset_sheets_shutting_down();
    }

    /// `onVetoed`: the quit is off, so saves and clean closes may delete
    /// recovery copies again, a "Restart & Install" is off as well, and a tab
    /// that answered "Don't Save" before another cancelled keeps its copy
    fn on_vetoed(&mut self) -> Option<Result<(), String>> {
        self.bridge.hooks.cancel_update_restart();
        self.bridge.hooks.resume_discarded_copies();
        for view in self.dirty_pdf {
            self.bridge.hooks.pdf_close_cancelled(*view);
        }
        Some(Ok(()))
    }

    fn is_destroyed(&mut self) -> bool {
        match self.bridge.shell {
            Some(shell) => self.bridge.windows.window_state(shell.window).destroyed,
            None => true,
        }
    }

    fn console_error(&mut self, label: &str, err: &str) {
        self.bridge.hooks.warn(label, err);
    }
}

impl TabsState {
    /// The window's close request (its close box, `window.close()` by a quit):
    /// the host kept the window open, so it is closed here once every tab agreed
    pub(super) fn shell_close_requested(&self, window: WindowId) {
        match self.shell_guard(window, true, |guard, host| guard.on_close(host)) {
            Ok(Some(event)) if event.prevent_default => self.drive_shell_close(window, event.step),
            // nothing to protect, or a window with no tabs to ask
            Ok(_) => self.windows.window_destroy(window),
            Err(error) => {
                // the dirty tabs could not be read: the window stays, as for a Cancel
                self.hooks.warn("[shell] window close prompt failed:", &error);
                self.hooks.reset_sheets_shutting_down();
                self.hooks.cancel_update_restart();
            }
        }
    }

    fn drive_shell_close(&self, window: WindowId, step: CloseStep<ViewId>) {
        match step {
            CloseStep::Idle | CloseStep::Done => {}
            CloseStep::QueryDocsDirty(view) => {
                let this = self.rc();
                self.hooks.docs_query_dirty(
                    view,
                    Box::new(move |answer| this.resume_shell_close(window, answer)),
                );
            }
            CloseStep::RequestClose(_, view) => {
                let this = self.rc();
                let then: Then<bool> =
                    Box::new(move |answer| this.resume_shell_close(window, answer));
                match editor_for(&self.provided, view) {
                    Some(editor) => editor.request_close(view, Some(window), then),
                    None => then(Err(IpcError {
                        name: Some("Error".into()),
                        message: format!("no editor owns view {}", view.0),
                    })),
                }
            }
            CloseStep::Rejected(error) => self.settle_shell_close(window, Err(error)),
            CloseStep::CloseWindow => {
                if let Err(error) = self.shell_guard(window, false, |guard, host| {
                    guard.after_close(host)
                }) {
                    self.hooks.warn("[shell] window close failed:", &error);
                }
                self.windows.window_destroy(window);
                *self.shell_close.borrow_mut() = None;
            }
        }
    }

    fn resume_shell_close(&self, window: WindowId, answer: Result<bool, IpcError>) {
        self.settle_shell_close(window, answer.map_err(|error| error.message));
    }

    fn settle_shell_close(&self, window: WindowId, answer: Result<bool, String>) {
        match self.shell_guard(window, false, |guard, host| guard.resume(host, answer)) {
            Ok(Some(step)) => self.drive_shell_close(window, step),
            // the manager went away with the window: no prompt is waiting on it
            Ok(None) => {}
            Err(error) => self.hooks.warn("[shell] window close prompt failed:", &error),
        }
    }

    /// Runs `call` on the window's guard with the manager and the bridge as its
    /// host, then the effects the host calls queued. `collect` reads the dirty
    /// lists first (the `close` event); `Ok(None)` when there is no manager.
    fn shell_guard<R>(
        &self,
        window: WindowId,
        collect: bool,
        call: impl FnOnce(&mut ShellCloseGuard<ViewId>, &mut GuardHost<'_>) -> R,
    ) -> Result<Option<R>, String> {
        let mut slot = self.shell_close.borrow_mut();
        if slot.as_ref().is_none_or(|state| state.window != window) {
            *slot = Some(ShellClose::new(window));
        }
        let state = slot.as_mut().expect("installed above");
        let mut inner = self.inner.borrow_mut();
        let Inner {
            manager, bridge, ..
        } = &mut *inner;
        let Some(manager) = manager.as_mut() else {
            return Ok(None);
        };
        let ShellClose {
            guard, dirty_pdf, ..
        } = state;
        let lists = if collect {
            let lists = dirty_lists(manager, bridge)?;
            *dirty_pdf = lists
                .iter()
                .filter(|(family, _)| *family == Family::Pdf)
                .flat_map(|(_, tabs)| tabs.iter().map(|tab| tab.web_contents))
                .collect();
            lists
        } else {
            Vec::new()
        };
        let mut host = GuardHost {
            manager,
            bridge,
            lists,
            dirty_pdf,
        };
        let out = call(guard, &mut host);
        inner.drain();
        Ok(Some(out))
    }
}
