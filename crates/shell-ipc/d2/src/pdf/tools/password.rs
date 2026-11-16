//! `pdf-password-dialog.ts`: the password prompt for encrypted PDFs, a
//! modal card over the shell window. One [`Password::prompt`] gets one
//! attempt (the password, or `None` when the user cancels or closes the
//! window); the window stays across attempts and the caller closes it with
//! [`Password::close`] when its retry loop ends.
//! Owned attempts use the same registry and generation protocol, retaining the
//! original request and exact native controls across asynchronous creation.

mod owned;

use std::cell::RefCell;
use std::rc::Rc;

use shell_contract::ipc::Target;
use shell_contract::{IpcValue, ViewId, WindowId};

use super::{Ctx, with};
use crate::kit::{self, Entry, as_str, field};

type Settle = Box<dyn FnOnce(Result<Option<String>, String>)>;

#[derive(Default)]
struct State {
    /// `pwWin`'s view
    view: Option<ViewId>,
    /// `currentState`: a `PdfPasswordUiState`
    current: Option<IpcValue>,
    /// `pendingResolve`
    pending: Option<Settle>,
    generation: u64,
    owned: Option<owned::Owned>,
}

#[derive(Clone)]
pub struct Password {
    ctx: Rc<Ctx>,
    state: Rc<RefCell<State>>,
    _closed: Rc<dyn Fn(ViewId)>,
}

impl Password {
    pub fn new(ctx: &Rc<Ctx>) -> Self {
        let state: Rc<RefCell<State>> = Rc::default();
        let weak = Rc::downgrade(&state);
        // closing the window any other way (Alt+F4) counts as cancel
        let closed = kit::on_view_destroyed(&ctx.services, move |view| {
            let Some(state) = weak.upgrade() else { return };
            let ours = {
                let mut s = state.borrow_mut();
                if s.owned.is_none() && s.view == Some(view) {
                    s.view = None;
                    true
                } else {
                    false
                }
            };
            if ours {
                settle(&state, Ok(None));
            }
        });
        Password {
            ctx: ctx.clone(),
            state,
            _closed: closed,
        }
    }

    /// `pushState(patch)`
    fn push_busy(&self) {
        let (view, current) = {
            let mut s = self.state.borrow_mut();
            let Some(current) = s.current.take() else {
                return;
            };
            let next = with(current, "busy", IpcValue::Bool(true));
            s.current = Some(next.clone());
            (s.view, next)
        };
        if let Some(view) = view.filter(|v| self.ctx.alive(*v)) {
            self.ctx.services.events.emit(
                Target::View(view),
                "pdf-password:changed",
                vec![current],
            );
        }
    }

    /// `promptPdfPassword(parent, state)`
    pub fn prompt(&self, parent: Option<WindowId>, state: IpcValue, then: Settle) {
        self.replace_owned();
        // a second prompt while one is pending answers the first as cancelled
        settle(&self.state, Ok(None));
        let title = as_str(field(field(&state, "strings"), "title"))
            .unwrap_or("")
            .to_owned();
        let generation = {
            let mut state = self.state.borrow_mut();
            state.generation = state
                .generation
                .checked_add(1)
                .expect("password generation exhausted");
            state.generation.to_string()
        };
        let current = with(
            with(state, "busy", IpcValue::Bool(false)),
            "generation",
            IpcValue::String(generation),
        );
        let open = {
            let mut s = self.state.borrow_mut();
            s.current = Some(current.clone());
            s.pending = Some(then);
            s.view.filter(|v| self.ctx.alive(*v))
        };
        if let Some(view) = open {
            self.ctx.services.events.emit(
                Target::View(view),
                "pdf-password:changed",
                vec![current],
            );
            self.ctx.contents.focus(view);
            return;
        }
        let shell = self.ctx.shell.clone();
        let this = self.clone();
        let generation = self.state.borrow().generation;
        kit::on_worker(
            &self.ctx.services,
            move || shell.open_password_window(parent, &title),
            move |view| {
                let current = {
                    let mut state = this.state.borrow_mut();
                    if state.generation != generation || state.pending.is_none() {
                        None
                    } else if let Some(view) = view {
                        if !this.ctx.alive(view) {
                            state.current = None;
                            drop(state);
                            settle(&this.state, Ok(None));
                            return;
                        }
                        state.view = Some(view);
                        state.current.clone()
                    } else {
                        state.current = None;
                        drop(state);
                        settle(
                            &this.state,
                            Err("Opening the password window failed".into()),
                        );
                        return;
                    }
                };
                if let (Some(view), Some(current)) = (view, current) {
                    this.ctx.services.events.emit(
                        Target::View(view),
                        "pdf-password:changed",
                        vec![current],
                    );
                } else if let Some(view) = view {
                    let belongs_to_owned = {
                        let state = this.state.borrow();
                        state.owned.is_some() && state.view == Some(view)
                    };
                    if !belongs_to_owned && let Some(window) = this.ctx.window_of(view) {
                        this.ctx.contents.close_window(window);
                    }
                }
            },
        );
    }

    /// `closePdfPasswordDialog()`
    pub fn close(&self) {
        if self.state.borrow().owned.is_some() {
            return;
        }
        let view = {
            let mut s = self.state.borrow_mut();
            s.current = None;
            s.view.take()
        };
        settle(&self.state, Ok(None));
        if let Some(view) = view.filter(|v| self.ctx.alive(*v))
            && let Some(window) = self.ctx.window_of(view)
        {
            self.ctx.contents.close_window(window);
        }
    }

    pub fn entries(&self) -> Vec<Entry> {
        let (a, b, c) = (self.clone(), self.clone(), self.clone());
        vec![
            kit::invoke("pdf-password:get-state", move |cx, _| {
                if a.state.borrow().owned.is_some() {
                    return Ok(a.owned_state(cx));
                }
                let state = a.state.borrow();
                Ok(if state.view == Some(cx.sender.view) {
                    state.current.clone().unwrap_or(IpcValue::Null)
                } else {
                    IpcValue::Null
                })
            }),
            kit::invoke("pdf-password:submit", move |cx, args| {
                let Some(password) = args.str(0) else {
                    return Ok(IpcValue::Undefined);
                };
                if b.state.borrow().owned.is_some() {
                    b.submit_owned(cx, password, args.str(1));
                    return Ok(IpcValue::Undefined);
                }
                let authorized = {
                    let state = b.state.borrow();
                    state.view == Some(cx.sender.view)
                        && args.str(1) == Some(state.generation.to_string().as_str())
                        && state.pending.is_some()
                };
                if authorized {
                    b.push_busy();
                    settle(&b.state, Ok(Some(password.to_owned())));
                }
                Ok(IpcValue::Undefined)
            }),
            kit::invoke("pdf-password:cancel", move |cx, args| {
                if c.state.borrow().owned.is_some() {
                    c.cancel_owned(cx, args.str(0));
                    return Ok(IpcValue::Undefined);
                }
                let authorized = {
                    let state = c.state.borrow();
                    state.view == Some(cx.sender.view)
                        && args.str(0) == Some(state.generation.to_string().as_str())
                        && state.pending.is_some()
                };
                if authorized {
                    settle(&c.state, Ok(None));
                    c.close();
                }
                Ok(IpcValue::Undefined)
            }),
        ]
    }
}

/// `settle(password)`: answers the pending prompt, once
fn settle(state: &RefCell<State>, password: Result<Option<String>, String>) {
    let pending = state.borrow_mut().pending.take();
    if let Some(then) = pending {
        then(password)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use serde_json::json;
    use shell_contract::{Surface, ViewId, WindowId};

    use super::super::test_support::*;
    use crate::kit::IntoIpc;
    use crate::testing::HostCall as Call;

    fn state() -> shell_contract::IpcValue {
        v(
            json!({ "fileName": "a.pdf", "retry": false, "busy": true, "lang": "ko",
                  "strings": { "title": "Password" } }),
        )
    }

    #[test]
    fn one_prompt_is_one_attempt_and_the_window_survives_retries() {
        let mut t = tools(json!({}));
        let tools = t.tools();
        let got: Rc<RefCell<Vec<Option<String>>>> = Rc::default();
        let g = got.clone();
        tools.prompt_password(
            Some(WindowId(1)),
            state(),
            Box::new(move |p| g.borrow_mut().push(p.unwrap())),
        );
        assert_eq!(
            t.fake.called("open_password_window"),
            vec![json!([1, "Password"])]
        );
        let win = t.rt.view(901, Surface::ShellPdfPassword);
        t.rt.drain();
        t.rt.events();
        assert_eq!(
            j(&t.call("pdf-password:get-state", win, vec![])),
            json!({ "fileName": "a.pdf", "retry": false, "busy": false, "lang": "ko",
                    "generation": "1", "strings": { "title": "Password" } })
        );
        let stranger = t.rt.view(902, Surface::ShellPdfPassword);
        assert_eq!(
            j(&t.call("pdf-password:get-state", stranger, vec![])),
            json!(null)
        );
        t.call(
            "pdf-password:submit",
            stranger,
            vec!["pw".into_ipc(), "1".into_ipc()],
        );
        assert!(got.borrow().is_empty());
        // not a string: nothing happens
        t.call("pdf-password:submit", win, vec![3.0.into_ipc()]);
        assert!(got.borrow().is_empty());
        t.call(
            "pdf-password:submit",
            win,
            vec!["pw".into_ipc(), "0".into_ipc()],
        );
        assert!(got.borrow().is_empty());
        t.call(
            "pdf-password:submit",
            win,
            vec!["pw".into_ipc(), "1".into_ipc()],
        );
        assert_eq!(*got.borrow(), vec![Some("pw".to_owned())]);
        let events = t.rt.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].channel, "pdf-password:changed");
        assert_eq!(j(&events[0].args[0])["busy"], json!(true));

        // the retry re-prompts in the open window
        let g = got.clone();
        tools.prompt_password(
            None,
            state(),
            Box::new(move |p| g.borrow_mut().push(p.unwrap())),
        );
        assert_eq!(t.fake.called("open_password_window").len(), 1);
        assert_eq!(t.rt.events()[0].channel, "pdf-password:changed");
        assert!(t.rt.script().calls.contains(&Call::Focus(ViewId(901))));

        t.call("pdf-password:cancel", win, vec!["2".into_ipc()]);
        assert_eq!(got.borrow()[1], None);
        assert!(
            t.rt.script()
                .calls
                .contains(&Call::CloseWindow(WindowId(901)))
        );
        assert_eq!(
            j(&t.call("pdf-password:get-state", win, vec![])),
            json!(null)
        );
    }

    #[test]
    fn closing_the_window_cancels_and_a_second_prompt_cancels_the_first() {
        let mut t = tools(json!({}));
        let tools = t.tools();
        let got: Rc<RefCell<Vec<Option<String>>>> = Rc::default();
        let (g1, g2) = (got.clone(), got.clone());
        tools.prompt_password(
            None,
            state(),
            Box::new(move |p| g1.borrow_mut().push(p.unwrap().map(|_| "1".into()))),
        );
        t.rt.view(901, Surface::ShellPdfPassword);
        t.rt.drain();
        tools.prompt_password(
            None,
            state(),
            Box::new(move |p| {
                g2.borrow_mut()
                    .push(p.unwrap().or(Some("2 cancelled".into())))
            }),
        );
        assert_eq!(*got.borrow(), vec![None]);
        t.rt.destroy(ViewId(901));
        assert_eq!(got.borrow()[1].as_deref(), Some("2 cancelled"));
    }

    #[test]
    fn closing_the_dialog_with_a_prompt_pending_cancels_it_once() {
        let mut t = tools(json!({}));
        let tools = t.tools();
        let got: Rc<RefCell<Vec<Option<String>>>> = Rc::default();
        let g = got.clone();
        tools.prompt_password(
            None,
            state(),
            Box::new(move |p| g.borrow_mut().push(p.unwrap())),
        );
        t.rt.view(901, Surface::ShellPdfPassword);
        t.rt.drain();
        tools.close_password_dialog();
        assert_eq!(*got.borrow(), vec![None]);
        t.rt.destroy(ViewId(901));
        assert_eq!(*got.borrow(), vec![None]);
        // a later window of the same view number settles nothing more
        t.rt.view(901, Surface::ShellPdfPassword);
        t.rt.destroy(ViewId(901));
        assert_eq!(*got.borrow(), vec![None]);
    }
}
