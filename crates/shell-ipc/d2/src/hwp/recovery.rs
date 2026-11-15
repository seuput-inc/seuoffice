//! The recovery channels of `apps/hwp/src/main/recovery.ts`
//! (`registerHwpRecoveryIpc`) over its port in `main-core`
//! (`main_core::hwp::recovery`), and the Electron side that port calls
//! ([`Electron`]).

use std::rc::Rc;
use std::time::Duration;

use main_core::hwp::ipc::HwpFormat;
use main_core::hwp::recovery::{
    HwpRecoveryElectron, MAX_HWP_SAVE_BYTES, RecoveryBeginResult, RecoveryTimer,
};
use main_core::hwp::save_fixes::apply_save_fixes;
use main_core::hwp::save_plan::bytes_match_format;
use main_core::hwp::tab_widths::new_tabs_of;
use shell_contract::IpcValue;
use shell_contract::runtime::PathName;

use super::editor::{Hwp, tab_spots, uint8, view_of, wc};
use super::{Boxes, Parent};
use crate::kit::{self, Entry, IntoIpc, as_num, as_str, obj, on_worker, to_json};

/// recovery.ts's Electron: the views, the app's paths, the dialog and the
/// timers, as the Hangul editor's state reaches them
pub(crate) struct Electron<'a>(pub &'a Rc<Hwp>);

impl HwpRecoveryElectron for Electron<'_> {
    type Window = Parent;
    type Dialog = Boxes;

    fn is_destroyed(&self, wc_id: i64) -> bool {
        self.0.is_destroyed(view_of(wc_id))
    }

    fn send_recovery_reset(&self, wc_id: i64) {
        self.0.emit(view_of(wc_id), "hwp:recovery-reset", vec![])
    }

    fn send_recovery_flush(&self, wc_id: i64, token: u64) -> Result<(), String> {
        let view = view_of(wc_id);
        let generation = match self.0.services.views.renderer_generation(view).filter(|generation| generation.is_active()) {
            Some(generation) => generation,
            None => {
                let error = "HWP recovery renderer generation unavailable".to_owned();
                *self.0.recovery_send_error.borrow_mut() = Some(error.clone());
                return Err(error);
            }
        };
        let owner = shell_contract::ipc::NativeRequestOwner {
            view,
            context: shell_contract::ipc::NativeContext::with_renderer((), generation.clone()),
        };
        let result = self.0.services.events.emit_to_owner_checked(
            &owner, "hwp:recovery-flush", vec![(token as f64).into_ipc()],
        );
        match result {
            Ok(()) if generation.is_active() => { self.0.recovery_targets.borrow_mut().insert(token, (view, generation)); Ok(()) }
            Ok(()) => {
                let error = "HWP recovery renderer was replaced".to_owned();
                *self.0.recovery_send_error.borrow_mut() = Some(error.clone());
                Err(error)
            }
            Err(error) => { *self.0.recovery_send_error.borrow_mut() = Some(error.clone()); Err(error) }
        }
    }

    fn reload(&self, wc_id: i64) {
        self.0.shell.reload(view_of(wc_id))
    }

    fn is_headless_mode(&self) -> bool {
        self.0.shell.is_headless_mode()
    }

    fn ui_lang(&self) -> String {
        self.0.shell.ui_lang()
    }

    fn user_data_dir(&self) -> String {
        self.0
            .services
            .paths
            .path(PathName::UserData)
            .to_string_lossy()
            .into_owned()
    }

    fn now_ms(&self) -> f64 {
        self.0.shell.now_ms()
    }

    fn random_uuid(&self) -> String {
        self.0.shell.random_uuid()
    }

    fn dialog(&self) -> &Boxes {
        &self.0.boxes
    }

    fn parent_of(&self, wc_id: i64) -> Option<Parent> {
        let contents = self
            .0
            .services
            .hosts
            .get::<shell_contract::ContentsHost>()
            .and_then(|c| c.window_of(view_of(wc_id)));
        contents
            .or_else(|| self.0.shell.focused_window())
            .map(Parent)
    }

    fn set_timeout(&self, timer: RecoveryTimer, ms: f64) {
        let weak = Rc::downgrade(self.0);
        let id = kit::after(
            &self.0.services,
            Duration::from_millis(ms.max(0.0) as u64),
            move || {
                let Some(hwp) = weak.upgrade() else { return };
                hwp.recovery_timers.borrow_mut().remove(&timer);
                match timer {
                    RecoveryTimer::DontSave(wc_id) => {
                        hwp.with_recovery(|r, e, _| r.dont_save_hold_ended(e, wc_id))
                    }
                    RecoveryTimer::Flush(token) => {
                        let timed_out = hwp.recovery.borrow_mut().flush_timed_out(token);
                        if timed_out {
                            hwp.finish_recovery_flush(token, Err("HWP recovery flush timed out".into()));
                        }
                    }
                }
            },
        );
        if let Some(old) = self.0.recovery_timers.borrow_mut().insert(timer, id) {
            kit::clear(old)
        }
    }

    fn clear_timeout(&self, timer: RecoveryTimer) {
        if let Some(id) = self.0.recovery_timers.borrow_mut().remove(&timer) {
            kit::clear(id)
        }
    }

    /// `console.warn`: the shell's log
    fn warn(&self, message: &str) {
        eprintln!("{message}")
    }
}

/// `data, format` of a renderer's copy, as `validBytes` checks them
fn valid_bytes(data: &IpcValue, format: &IpcValue) -> Option<(Vec<u8>, HwpFormat)> {
    let data = uint8(data)?;
    let format = as_str(format).and_then(HwpFormat::parse)?;
    (!data.is_empty() && data.len() <= MAX_HWP_SAVE_BYTES && bytes_match_format(data, format))
        .then(|| (data.to_vec(), format))
}

pub(super) fn entries(hwp: &Rc<Hwp>) -> Vec<Entry> {
    let mut all = Vec::new();

    let h = Rc::clone(hwp);
    all.push(kit::invoke("hwp:recovery-begin", move |cx, _| {
        let view = cx.sender.view;
        Ok(
            match h.with_recovery(|r, e, s| r.recovery_begin(e, s, wc(view))) {
                RecoveryBeginResult::Refused => obj([("ok", false.into_ipc())]),
                RecoveryBeginResult::Began(format) => obj([
                    ("ok", true.into_ipc()),
                    ("format", format.as_str().into_ipc()),
                ]),
            },
        )
    }));

    // the tabs a copy's bytes leave without a width: as many as the file is long
    let h = Rc::clone(hwp);
    all.push(kit::invoke_later(
        "hwp:recovery-new-tabs",
        move |cx, args, reply| {
            let view = cx.sender.view;
            if !h.recovery.borrow().has_turn(wc(view)) {
                return reply.reply(Ok(IpcValue::Array(Vec::new())));
            }
            let Some((data, format)) = valid_bytes(args.get(0), args.get(1)) else {
                return reply.reply(Ok(IpcValue::Array(Vec::new())));
            };
            on_worker(
                &h.services,
                move || tab_spots(&new_tabs_of(&data, format)),
                move |spots| reply.reply(Ok(spots)),
            );
        },
    ));

    let h = Rc::clone(hwp);
    all.push(kit::invoke_later(
        "hwp:recovery-write",
        move |cx, args, reply| {
            let view = cx.sender.view;
            let data = uint8(args.get(0)).map(<[u8]>::to_vec);
            let format = as_str(args.get(1)).map(str::to_owned);
            let checked = h.with_recovery(|r, e, s| {
                r.recovery_write_check(e, s, wc(view), data.as_deref(), format.as_deref())
            });
            let job = match checked {
                Ok(job) => job,
                Err(outcome) => return reply.reply(Ok(outcome.as_str().into_ipc())),
            };
            let tabs: Option<Vec<serde_json::Value>> = match args.get(2) {
                IpcValue::Array(items) => Some(
                    items
                        .iter()
                        .map(|t| to_json(t).unwrap_or(serde_json::Value::Null))
                        .collect(),
                ),
                _ => None,
            };
            let data = data.unwrap_or_default();
            let (job_format, base) = (job.format, job.base.clone());
            let hwp = Rc::clone(&h);
            on_worker(
                &h.services,
                move || {
                    apply_save_fixes(&data, job_format, tabs.as_deref(), &base)
                        .bytes
                        .into_owned()
                },
                move |fixed| {
                    let outcome = hwp.with_recovery(|r, e, s| {
                        r.recovery_write_land(e, s, wc(view), job, &fixed)
                    });
                    reply.reply(Ok(outcome.as_str().into_ipc()))
                },
            );
        },
    ));

    let h = Rc::clone(hwp);
    all.push(kit::invoke("hwp:recovery-opened", move |cx, args| {
        let view = cx.sender.view;
        let protected = matches!(args.get(0), IpcValue::Bool(true));
        let opened = h.with_recovery(|r, e, _| r.recovery_opened(e, wc(view), protected));
        Ok(obj([
            ("restored", opened.restored.into_ipc()),
            ("notice", opened.notice.into_ipc()),
        ]))
    }));

    let h = Rc::clone(hwp);
    all.push(kit::send("hwp:recovery-flushed", move |cx, args| {
        if let Some(token) = as_num(args.get(0)).filter(|n| n.is_finite() && n.fract() == 0.0 && *n >= 0.0 && *n <= 9_007_199_254_740_991.0) {
            let id = token as u64;
            let owner = h.recovery_targets.borrow().get(&id).cloned();
            let generation = cx.native.as_ref().and_then(|native| native.renderer());
            let matches = owner.as_ref().zip(generation).is_some_and(|((view, expected), actual)| {
                *view == cx.sender.view && expected.is_active() && expected.same_generation(actual)
            });
            if matches && h.with_recovery(|r, e, _| r.recovery_flushed(e, token)) {
                let result = as_str(args.get(1)).map_or(Ok(()), |message| Err(message.to_owned()));
                h.finish_recovery_flush(id, result);
            }
        }
        Ok(())
    }));

    all
}
