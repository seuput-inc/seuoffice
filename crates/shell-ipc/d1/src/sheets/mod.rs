//! The `sheets` channels of batch D1: `registerSheetsIpc` of
//! `apps/sheets/src/main/sheets-main.ts` (the `workbook:*` and `sheets:*`
//! channels of this batch; printing and the AI channels belong to other
//! batches).

mod conv;
mod core;
mod dialogs;
mod editors;
pub mod engine;
#[cfg(any(test, feature = "probe"))]
mod fakes;
mod i18n;
mod misc;
mod open;
mod owned_close;
mod read;
mod recovery;
mod recovery_policy;
mod save;
mod schema;
mod session;
#[cfg(test)]
mod tests;

use std::rc::Rc;

use shell_contract::editors::DocumentEditors;
use shell_contract::ipc::RegisterError;
use shell_contract::provided::Provided;
use shell_contract::runtime::Services;
use shell_contract::{
    Batch, ClipboardHost, DialogsHost, IpcError, IpcValue, MainWindowHost, OsShellHost, ViewId,
};

pub use self::core::HeadlessReport;
use self::core::{Env, core_at, create_core};
pub use self::editors::{
    BlankSeedReceipt, PreparedBlankSeed, PreparedSavedWorkbookBarrier, PreparedTabClose,
    PreparedTabRegistration, PreparedTabTransfer, PreparedWorkbookQueue, RegisteredTab,
    SheetsViews, WorkbookQueueReceipt, views,
};
use self::editors::{SheetsEditors, install_views};
use self::engine::{ScreenCaptureHost, SheetsShellHost, WorkbookEngineHost};
use crate::host::{LocaleHost, RecoveryStateHost, Shared, on_view_gone};
use crate::route::{self, Route};
use crate::value::{Args, field, thrown};

type Reply = Result<IpcValue, IpcError>;

/// `ipcMain.handle` whose body awaits in TypeScript: it runs as straight-line
/// code on a worker and answers through the responder
fn worker(env: &Env, channel: &'static str, body: fn(&Env, ViewId, Args) -> Reply) -> Route {
    let env = env.clone();
    route::invoke_later(channel, move |cx, args, reply| {
        let guard = match core_at(env.slot).begin_headless_work(cx) {
            Ok(guard) => guard,
            Err(error) => return reply.reply(Err(error)),
        };
        let (view, env) = (cx.sender.view, env.clone());
        let work = env.clone();
        env.spawn(move || {
            let _guard = guard;
            reply.reply(body(&work, view, args));
        });
    })
}

fn open_worker(
    env: &Env,
    channel: &'static str,
    body: fn(&Env, ViewId, Args, &open::OpenAuthority) -> Reply,
) -> Route {
    let env = env.clone();
    route::invoke_later(channel, move |cx, args, reply| {
        let view = cx.sender.view;
        if let Err(error) = core_at(env.slot).validate_headless_sender(cx) {
            return reply.reply(Err(error));
        }
        let guard = match core_at(env.slot).begin_headless_work(cx) {
            Ok(guard) => guard,
            Err(error) => return reply.reply(Err(error)),
        };
        if matches!(channel, "workbook:select" | "workbook:reopen") {
            core_at(env.slot).bind_save_renderer(cx);
        }
        let reply = if channel == "workbook:select" {
            match core_at(env.slot).claim_saved_selection(cx, reply) {
                Some(reply) => reply,
                None => return,
            }
        } else {
            reply
        };
        let authority = open::OpenAuthority {
            owner: cx
                .native
                .clone()
                .map(|context| shell_contract::ipc::NativeRequestOwner { view, context }),
            headless: core_at(env.slot)
                .state
                .borrow()
                .tab(view)
                .and_then(|tab| tab.headless_target.clone()),
            cancel: cx.cancel.clone(),
            pending: shell_contract::runtime::CancelToken::default(),
        };
        let work = env.clone();
        env.spawn(move || {
            let _guard = guard;
            let result = body(&work, view, args, &authority);
            authority.pending.cancel();
            reply.reply(result);
        });
    })
}

#[allow(clippy::type_complexity)]
fn copy_worker(
    env: &Env,
    channel: &'static str,
    kind: &'static str,
    body: fn(
        &Env,
        ViewId,
        IpcValue,
        Option<crate::owned_save::OwnedSave>,
        Option<core::GeneratedTransition>,
        Option<core::HeadlessPrint>,
    ) -> Reply,
) -> Route {
    let env = env.clone();
    route::invoke_later(channel, move |cx, mut args, reply| {
        let mut input = args.take(0);
        let token = match field(&input, "ownedSaveRequestId") {
            IpcValue::Undefined => None,
            IpcValue::String(token) => Some(token.clone()),
            _ => return reply.reply(Err(thrown("Invalid owned workbook copy request"))),
        };
        let target = core_at(env.slot)
            .state
            .borrow()
            .tab(cx.sender.view)
            .and_then(|tab| tab.headless_target.clone());
        if kind == "pdf" && env.shell.is_headless() && target.is_none() {
            return reply.reply(Err(thrown(
                "The headless PDF has no native invocation reservation",
            )));
        }
        let headless = if let Some(target) = target {
            if kind != "pdf" || token.is_some() {
                return reply.reply(Err(thrown(
                    "The headless invocation only exports its requested PDF",
                )));
            }
            if let Err(error) = core_at(env.slot).validate_headless_sender(cx) {
                return reply.reply(Err(error));
            }
            let registered = match target.registered_document() {
                Ok(document) => document,
                Err(error) => return reply.reply(Err(thrown(error))),
            };
            Some(core::HeadlessPrint {
                document: registered,
                cancel: cx.cancel.clone(),
                pending: shell_contract::runtime::CancelToken::default(),
            })
        } else {
            None
        };
        let (owned, transition) = if let Some(token) = &token {
            let path_key = if kind == "pdf" {
                "saveAsPath"
            } else {
                "targetPath"
            };
            let IpcValue::String(path) = field(&input, path_key) else {
                return reply.reply(Err(thrown("The owned copy has no selected target")));
            };
            match core_at(env.slot).owned_export_for(cx, token, kind, path) {
                Ok((scope, transition)) => (Some(scope), Some(transition)),
                Err(error) => return reply.reply(Err(error)),
            }
        } else {
            (None, None)
        };
        if let IpcValue::Object(fields) = &mut input {
            fields.retain(|(name, _)| name != "ownedSaveRequestId");
            if let Some((_, IpcValue::Object(workbook))) =
                fields.iter_mut().find(|(name, _)| name == "workbook")
            {
                workbook.retain(|(name, _)| name != "ownedSaveRequestId");
            }
        }
        let view = cx.sender.view;
        let guard = match core_at(env.slot).begin_headless_work(cx) {
            Ok(guard) => guard,
            Err(error) => return reply.reply(Err(error)),
        };
        let work = env.clone();
        env.spawn(move || {
            let _guard = guard;
            let mut result = body(
                &work,
                view,
                input,
                owned,
                transition.clone(),
                headless.clone(),
            );
            if let Some(token) = token {
                let settled = result.clone();
                if let Some(transition) = transition
                    .as_ref()
                    .filter(|transition| transition.has_receipt())
                    .cloned()
                {
                    let key = token.clone();
                    let admission = work.core(move |core| {
                        transition.terminal(&mut || {
                            if !core.state.borrow().owned_saves.contains_key(&key) {
                                return Err("The workbook save request is no longer pending".into());
                            }
                            core.owned_save_phase_finished(&key, &settled);
                            Ok(())
                        })
                    });
                    if let Err(error) = admission {
                        let failure = thrown(error);
                        let key = token.clone();
                        let settled = failure.clone();
                        work.post(move |core| core.finish_owned_save(&key, Err(settled)));
                        result = Err(failure);
                    }
                } else if transition
                    .as_ref()
                    .is_some_and(|transition| transition.retired_origin())
                {
                    let failure = result
                        .as_ref()
                        .err()
                        .cloned()
                        .unwrap_or_else(|| thrown("The generated document did not complete"));
                    let key = token.clone();
                    work.post(move |core| core.finish_owned_save(&key, Err(failure)));
                } else {
                    work.post(move |core| core.owned_save_phase_finished(&token, &settled));
                }
            }
            reply.reply(result);
            if let Some(headless) = headless {
                headless.pending.cancel();
            }
        });
    })
}

fn prompt_worker(
    env: &Env,
    channel: &'static str,
    token_index: usize,
    body: fn(&Env, ViewId, Args, Option<crate::owned_save::OwnedSave>) -> Reply,
) -> Route {
    let env = env.clone();
    route::invoke_later(channel, move |cx, args, reply| {
        let owned = match args.get(token_index) {
            IpcValue::Undefined => None,
            IpcValue::String(token) => {
                let core = core_at(env.slot);
                let scope = core
                    .state
                    .borrow()
                    .owned_saves
                    .get(token)
                    .map(|waiter| waiter.scope.clone());
                let Some(scope) = scope else {
                    return reply
                        .reply(Err(thrown("The workbook save prompt is no longer pending")));
                };
                if let Err(error) = scope.validate_sender(cx) {
                    return reply.reply(Err(error));
                }
                Some(scope)
            }
            _ => return reply.reply(Err(thrown("Invalid owned workbook prompt request"))),
        };
        let view = cx.sender.view;
        let work = env.clone();
        env.spawn(move || reply.reply(body(&work, view, args, owned)));
    })
}

fn screen_worker(
    env: &Env,
    channel: &'static str,
    body: fn(
        &Env,
        ViewId,
        Args,
        &shell_contract::ipc::NativeRequestOwner,
        &shell_contract::runtime::CancelToken,
    ) -> Reply,
) -> Route {
    let env = env.clone();
    route::invoke_later(channel, move |cx, args, reply| {
        let Some(context) = cx.native.clone() else {
            return reply.reply(Err(thrown(
                "Screen capture requires native request authority",
            )));
        };
        let owner = shell_contract::ipc::NativeRequestOwner {
            view: cx.sender.view,
            context,
        };
        let cancel = cx.cancel.clone();
        let view = cx.sender.view;
        let work = env.clone();
        env.spawn(move || reply.reply(body(&work, view, args, &owner, &cancel)));
    })
}

pub fn routes(services: &Services, provided: &Rc<Provided>) -> Result<Vec<Route>, RegisterError> {
    let hosts = &services.hosts;
    let locale: LocaleHost = hosts.need(Batch::D1)?;
    let core = create_core(Env {
        services: services.clone(),
        slot: 0,
        engine: hosts.need::<WorkbookEngineHost>(Batch::D1)?,
        shell: hosts.need::<SheetsShellHost>(Batch::D1)?,
        screen: hosts.need::<ScreenCaptureHost>(Batch::D1)?,
        dialogs: hosts.need::<DialogsHost>(Batch::D1)?,
        os_shell: hosts.need::<OsShellHost>(Batch::D1)?,
        clipboard: hosts.need::<ClipboardHost>(Batch::D1)?,
        main_window: hosts.need::<MainWindowHost>(Batch::D1)?,
        shared: Shared::need(hosts)?,
        recovery: hosts.need::<RecoveryStateHost>(Batch::D1)?,
        tm: i18n::Tm::new(locale),
    });
    let env = core.env.clone();

    // a tab's end, as `webContents.once('destroyed', …)`
    {
        let core = core.clone();
        on_view_gone(services, move |view| core.view_gone(view));
    }
    // the sidecar died: every tab using it is told
    {
        let env = env.clone();
        let listener_env = env.clone();
        env.engine.on_process_exit(Box::new(move || {
            listener_env.post(|core| {
                for view in core.tab_views() {
                    if core.env.alive(view) {
                        core.send(view, "workbook:sidecar-crashed", Vec::new());
                    }
                }
            })
        }));
    }
    install_views(&core);
    // the tabs and flags the shell made before the handlers run
    {
        let startup = env.shell.startup();
        for sender in services.views.all() {
            if startup.register_views
                && sender.surface == shell_contract::Surface::Sheets
                && !startup.unregistered.contains(&sender.view)
            {
                core.register_tab(sender.view);
            }
        }
        for (view, path) in &startup.queued {
            core.queue_workbook(*view, path);
        }
        if startup.new_blank {
            core.set_new_blank();
        }
        for path in &startup.untitled {
            core.mark_untitled_path(path);
        }
        for (view, path) in &startup.mcp_writable {
            core.authorize_mcp_write(*view, path);
        }
        for (view, input, out) in startup.headless {
            core.queue_workbook(view, &input);
            let mut state = core.state.borrow_mut();
            state.headless_targets.insert(view, out);
            state.headless_waiters.insert(view, Box::new(|_| {}));
        }
    }
    provided.add::<dyn DocumentEditors>(Rc::new(SheetsEditors {
        core: core.clone(),
        provided: Rc::downgrade(provided),
    }));
    provided
        .add::<dyn main_core::packages::electron_utils::recovery_contract::RecoveryEditorContract<
                ViewId,
                shell_contract::ipc::RecoveryOpenTarget,
            >>(Rc::new(SheetsViews(core.clone())));

    let mut routes = Vec::new();

    // ---- on the core thread ----
    let c = core.clone();
    routes.push(route::invoke(
        "sheets:consume-new-blank",
        move |cx, _args| {
            let mut state = c.state.borrow_mut();
            let matches =
                state
                    .pending_new_blank
                    .as_ref()
                    .is_some_and(|entry| match &entry.target {
                        None => true,
                        Some((view, created)) => {
                            *view == cx.sender.view
                                && created.is_active()
                                && cx
                                    .native
                                    .as_ref()
                                    .and_then(|native| native.renderer())
                                    .is_some_and(|actual| actual.same_generation(created))
                        }
                    });
            if !matches {
                return Ok(IpcValue::Bool(false));
            }
            if let Some(entry) = state.pending_new_blank.take() {
                entry.identity.set(self::core::QueueOutcome::Consumed);
                state.blank_revision = std::rc::Rc::new(());
                Ok(IpcValue::Bool(true))
            } else {
                Ok(IpcValue::Bool(false))
            }
        },
    ));
    let c = core.clone();
    routes.push(route::invoke(
        "sheets:has-queued-workbook",
        move |cx, _args| {
            c.headless_effect(cx, || {
                IpcValue::Bool(
                    c.state
                        .borrow()
                        .queued_workbooks
                        .contains_key(&cx.sender.view),
                )
            })
        },
    ));
    let c = core.clone();
    routes.push(route::invoke(
        "sheets:consume-headless-export",
        move |cx, _args| {
            c.headless_effect(cx, || {
                match c
                    .state
                    .borrow_mut()
                    .headless_targets
                    .remove(&cx.sender.view)
                {
                    Some(target) => IpcValue::String(target),
                    None => IpcValue::Null,
                }
            })
        },
    ));
    let c = core.clone();
    routes.push(route::send(
        "sheets:headless-export-done",
        move |cx, args| {
            let waiter = c.headless_effect(cx, || {
                c.state
                    .borrow_mut()
                    .headless_waiters
                    .remove(&cx.sender.view)
            })?;
            if let Some(settle) = waiter {
                let result = args.get(0);
                settle(core::HeadlessReport {
                    ok: matches!(field(result, "ok"), IpcValue::Bool(true)),
                    error: match field(result, "error") {
                        IpcValue::String(error) => Some(error.clone()),
                        _ => None,
                    },
                });
            }
            Ok(())
        },
    ));
    let c = core.clone();
    routes.push(route::invoke("workbook:cancel-import", move |cx, _args| {
        c.state
            .borrow_mut()
            .pending_imports
            .cancel(f64::from(cx.sender.view.0));
        Ok(IpcValue::Undefined)
    }));
    let c = core.clone();
    routes.push(route::invoke(
        "workbook:saved-open-result",
        move |cx, args| c.saved_open_result(cx, args.get(0)),
    ));
    let c = core.clone();
    routes.push(route::send("workbook:pending-edits", move |cx, args| {
        misc::pending_edits(&c, cx.sender.view, args.get(0));
        Ok(())
    }));
    let c = core.clone();
    routes.push(route::send(
        "workbook:close-save-result",
        move |cx, args| {
            if let IpcValue::String(token) = args.get(1) {
                let (scope, transition, in_flight, require_write, writes, failure) = {
                    let state = c.state.borrow();
                    state.owned_saves.get(token).map(|waiter| {
                        (
                            waiter.scope.clone(),
                            waiter.transition.clone(),
                            waiter.in_flight,
                            waiter.require_write,
                            waiter.writes,
                            waiter.failure.clone(),
                        )
                    })
                }
                .ok_or_else(|| thrown("The workbook save acknowledgement is no longer pending"))?;
                if transition.has_receipt() {
                    let actual = cx.native.as_ref().and_then(|native| native.renderer());
                    let expected = scope.destination.context.renderer();
                    if cx.sender.view != scope.destination.view
                        || !actual
                            .zip(expected)
                            .is_some_and(|(actual, expected)| actual.same_generation(expected))
                    {
                        return Err(thrown("The save response belongs to another renderer"));
                    }
                } else {
                    scope.validate_sender(cx)?;
                }
                let result = match (failure, args.get(2)) {
                    (Some(error), _) => Err(error),
                    (None, IpcValue::String(message)) => Err(thrown(message.clone())),
                    _ if in_flight => Err(thrown("The workbook save has not finished")),
                    _ if require_write
                        && writes == 0
                        && matches!(args.get(0), IpcValue::Bool(true)) =>
                    {
                        Err(thrown(
                            "The workbook save acknowledged success without a completed write",
                        ))
                    }
                    _ => Ok(matches!(args.get(0), IpcValue::Bool(true))),
                };
                if transition.has_receipt() {
                    let mut waiter = None;
                    transition
                        .terminal(&mut || {
                            waiter = c.state.borrow_mut().owned_saves.remove(token);
                            if waiter.is_some() {
                                Ok(())
                            } else {
                                Err("The workbook save request is no longer pending".into())
                            }
                        })
                        .map_err(thrown)?;
                    if let Some(waiter) = waiter {
                        c.complete_owned_save(waiter, result);
                    }
                } else {
                    c.finish_owned_save(token, result);
                }
            } else {
                misc::close_save_result(&c, cx.sender.view, args.get(0));
            }
            Ok(())
        },
    ));
    let c = core.clone();
    routes.push(route::send(
        "workbook:recovery-write-now-done",
        move |cx, args| {
            misc::recovery_write_now_done(&c, cx.sender.view, args.get(0), args.get(1));
            Ok(())
        },
    ));
    let c = core.clone();
    routes.push(route::invoke("workbook:list-tabs", move |cx, _args| {
        Ok(misc::list_tabs(&c, cx.sender.view))
    }));
    let c = core.clone();
    routes.push(route::invoke("workbook:pick-arm", move |cx, args| {
        misc::arm_pick(&c, cx.sender.view, args.get(0))
    }));
    let c = core.clone();
    routes.push(route::invoke("workbook:pick-select", move |cx, args| {
        misc::pick_select(&c, cx.sender.view, args.get(0))
    }));
    let c = core.clone();
    routes.push(route::invoke("workbook:pick-cancel", move |cx, args| {
        misc::pick_cancel(&c, cx.sender.view, args.get(0))
    }));
    let c = core.clone();
    routes.push(route::invoke(
        "workbook:save-edits-begin",
        move |cx, args| save::save_edits_begin(&c, cx.sender.view, args.get(0)),
    ));
    let c = core.clone();
    routes.push(route::invoke(
        "workbook:save-edits-abort",
        move |cx, args| save::save_edits_abort(&c, cx.sender.view, args.get(0)),
    ));

    // ---- on a worker ----
    routes.push(open_worker(
        &env,
        "workbook:select",
        |env, view, _args, authority| {
            open::open_selected_workbook(env, view, None, authority).inspect_err(|error| {
                // A headless export has no dialog to report a failed open through: settle it
                let message = format!("the input workbook did not open ({error})");
                let expected = authority
                    .owner
                    .as_ref()
                    .and_then(|owner| owner.context.renderer().cloned());
                env.core(move |core| core.fail_headless_export(view, expected.as_ref(), &message));
            })
        },
    ));
    routes.push(open_worker(
        &env,
        "workbook:reopen",
        |env, view, mut args, authority| reopen(env, view, args.take(0), authority),
    ));
    routes.push(open_worker(
        &env,
        "workbook:select-for-merge",
        |env, view, _args, authority| select_for_merge(env, view, authority),
    ));
    routes.push(open_worker(
        &env,
        "workbook:open-for-merge",
        |env, view, mut args, authority| open_for_merge(env, view, args.take(0), authority),
    ));
    routes.push(worker(
        &env,
        "workbook:update-password",
        |env, view, mut args| save::update_password(env, view, args.take(0)),
    ));
    routes.push({
        let env = env.clone();
        route::invoke_later("workbook:save", move |cx, mut args, reply| {
            let (view, env) = (cx.sender.view, env.clone());
            let mut input = args.take(0);
            let token = match field(&input, "ownedSaveRequestId") {
                IpcValue::Undefined => None,
                IpcValue::String(token) => Some(token.clone()),
                _ => return reply.reply(Err(thrown("Invalid owned workbook save request"))),
            };
            let owned = match token
                .as_deref()
                .map(|token| core_at(env.slot).owned_save_for(cx, token, &input))
                .transpose()
            {
                Ok(owned) => owned,
                Err(error) => return reply.reply(Err(error)),
            };
            if let IpcValue::Object(fields) = &mut input {
                fields.retain(|(name, _)| name != "ownedSaveRequestId");
            }
            // a Save As the shell asked for (`request_save`) ends with the
            // renderer's next `save-as` request; the request it waits for is
            // the one current when this save arrives, not when it ends
            let waiter =
                matches!(field(&input, "mode"), IpcValue::String(mode) if mode == "save-as")
                    .then(|| core_at(env.slot).save_as_token(view))
                    .flatten();
            let work = env.clone();
            env.spawn(move || {
                let answer = save::save_workbook_with_owner(&work, view, input, owned);
                if let Some(token) = token {
                    let result = answer.clone();
                    work.post(move |core| core.owned_save_phase_finished(&token, &result));
                }
                if let Some(token) = waiter {
                    // saved when the answer says it was not canceled
                    let saved = matches!(
                        &answer,
                        Ok(answer) if matches!(field(answer, "canceled"), IpcValue::Bool(false))
                    );
                    work.post(move |core| core.save_as_finished(view, token, saved));
                }
                reply.reply(answer)
            });
        })
    });
    routes.push(worker(
        &env,
        "workbook:save-edits-chunk",
        |env, view, mut args| save::save_edits_chunk(env, view, args.take(0)),
    ));
    routes.push(worker(
        &env,
        "workbook:write-recovery",
        |env, view, mut args| save::write_workbook_recovery(env, view, args.take(0)),
    ));
    routes.push(worker(
        &env,
        "workbook:template-bytes",
        |env, view, mut args| save::template_bytes(env, view, args.take(0)),
    ));
    routes.push(worker(&env, "workbook:close", |env, view, mut args| {
        save::close_workbook(env, view, args.take(0))
    }));
    routes.push(worker(
        &env,
        "workbook:auto-rename",
        |env, view, mut args| save::auto_rename(env, view, args.take(0), args.take(1)),
    ));
    routes.push(copy_worker(
        &env,
        "workbook:export-csv",
        "csv",
        |env, view, input, owned, _, _| save::export_csv(env, view, input, owned),
    ));
    routes.push(copy_worker(
        &env,
        "workbook:export-ods",
        "ods",
        |env, view, input, owned, _, _| save::export_ods(env, view, input, owned),
    ));
    routes.push(copy_worker(
        &env,
        "workbook:export-pdf",
        "pdf",
        save::export_pdf,
    ));
    routes.push({
        let env = env.clone();
        route::invoke_later("workbook:create-document", move |cx, mut args, reply| {
            // `sessionFor(event)` answers an unregistered sender before anything else
            if let Err(error) = core_at(env.slot).require_tab(cx.sender.view) {
                return reply.reply(Err(error));
            }
            let (scope, revision) = match core_at(env.slot).capture_owned_call(cx) {
                Ok(captured) => captured,
                Err(error) => return reply.reply(Err(error)),
            };
            let view = cx.sender.view;
            let input = args.take(0);
            let work = env.clone();
            env.spawn(move || {
                let result = save::create_document(&work, view, input, scope.clone(), revision);
                scope.pending.cancel();
                reply.reply(result);
            });
        })
    });
    routes.push(prompt_worker(
        &env,
        "workbook:csv-save-confirm",
        0,
        |env, view, _args, scope| save::csv_save_confirm(env, view, scope),
    ));
    routes.push(prompt_worker(
        &env,
        "workbook:withheld-export-choice",
        1,
        |env, view, args, scope| save::withheld_export_choice(env, view, args.get(0), scope),
    ));
    routes.push(worker(
        &env,
        "workbook:read-range",
        |env, view, mut args| read::read_range(env, view, args.take(0)),
    ));
    routes.push(worker(
        &env,
        "workbook:read-formulas",
        |env, view, mut args| read::read_formulas(env, view, args.take(0)),
    ));
    routes.push(worker(
        &env,
        "workbook:find-cells",
        |env, view, mut args| read::find_cells(env, view, args.take(0)),
    ));
    routes.push(worker(
        &env,
        "workbook:read-row-outline",
        |env, view, mut args| read::read_row_outline(env, view, args.take(0)),
    ));
    routes.push(worker(&env, "workbook:recalc", |env, view, mut args| {
        read::recalc(env, view, args.take(0))
    }));
    routes.push(worker(
        &env,
        "workbook:read-media",
        |env, view, mut args| read::read_media(env, view, args.take(0)),
    ));
    routes.push(worker(
        &env,
        "workbook:list-external-links",
        |env, view, mut args| read::list_external_links(env, view, args.take(0)),
    ));
    routes.push(worker(
        &env,
        "workbook:update-external-links",
        |env, view, mut args| read::update_external_links(env, view, args.take(0)),
    ));
    routes.push(worker(
        &env,
        "workbook:read-external-link-usage",
        |env, view, mut args| read::read_external_link_usage(env, view, args.take(0)),
    ));
    routes.push(worker(
        &env,
        "workbook:select-external-link-source",
        |env, view, _args| read::select_external_link_source(env, view),
    ));
    routes.push(worker(
        &env,
        "workbook:read-pivot-definition",
        |env, view, mut args| read::read_pivot_definition(env, view, args.take(0)),
    ));
    routes.push(worker(
        &env,
        "workbook:read-slicer-state",
        |env, view, mut args| read::read_slicer_state(env, view, args.take(0)),
    ));
    routes.push(worker(
        &env,
        "workbook:list-connections",
        |env, view, mut args| read::list_connections(env, view, args.take(0)),
    ));
    routes.push(worker(
        &env,
        "workbook:refresh-connection",
        |env, view, mut args| read::refresh_connection(env, view, args.take(0)),
    ));
    routes.push(worker(
        &env,
        "workbook:read-queries",
        |env, view, mut args| read::read_queries(env, view, args.take(0)),
    ));
    routes.push(worker(
        &env,
        "workbook:read-redactions",
        |env, view, mut args| read::read_redactions(env, view, args.take(0)),
    ));
    routes.push(worker(&env, "sheets:files-pick", |env, view, _args| {
        misc::files_pick(env, view)
    }));
    routes.push(worker(&env, "sheets:files-add", |env, view, args| {
        misc::files_add(env, view, args.get(0))
    }));
    routes.push(worker(
        &env,
        "sheets:files-add-pasted-image",
        |env, view, args| misc::files_add_pasted_image(env, view, args.get(0), args.get(1)),
    ));
    routes.push(worker(&env, "sheets:files-read", |env, view, args| {
        misc::files_read(env, view, args.get(0), args.get(1), args.get(2))
    }));
    routes.push(worker(
        &env,
        "sheets:files-read-image",
        |env, view, args| misc::files_read_image(env, view, args.get(0)),
    ));
    routes.push(screen_worker(
        &env,
        "sheets:capture-screen-sources",
        |env, view, _args, owner, cancel| misc::capture_screen_sources(env, view, owner, cancel),
    ));
    routes.push(screen_worker(
        &env,
        "sheets:capture-screen-source",
        |env, view, args, owner, cancel| {
            misc::capture_screen_source(env, view, args.get(0), owner, cancel)
        },
    ));
    Ok(routes)
}

/// `reopenWorkbook`
fn reopen(env: &Env, view: ViewId, path_arg: IpcValue, authority: &open::OpenAuthority) -> Reply {
    use main_core::sheets::session_reopen::{continued_open_options, session_to_continue};
    use std::path::Path;

    schema::parse(&schema::string().min(1), &path_arg)?;
    let IpcValue::String(path) = path_arg else {
        return Ok(IpcValue::Undefined);
    };
    // A converted, restored, new or decrypted workbook is more than its path:
    // re-open the twin of the session being continued
    let sessions = env.core(move |core| {
        core.require_tab(view)?;
        Ok(core
            .state
            .borrow()
            .tab(view)
            .map(|tab| tab.sessions.clone())
            .unwrap_or_default())
    })?;
    let maps: Vec<_> = sessions.iter().map(|(_, s)| s.to_map()).collect();
    let previous = session_to_continue(&maps, &path)
        .and_then(|found| maps.iter().position(|m| std::ptr::eq(m, found)));
    if let Some(at) = previous {
        let previous = &sessions[at].1;
        if !Path::new(&previous.path).exists() {
            return Err(thrown("Workbook file not found."));
        }
        let options = session::OpenOptions::from_carried(&continued_open_options(&maps[at]));
        let result = open::open_workbook_session(env, view, &previous.path, options)?;
        if !env.alive(view) {
            // The tab closed while the twin was opening; this session has to go the same way
            let id = result.session_id.clone();
            env.core(move |core| {
                if let Some(tab) = core.state.borrow_mut().tab_mut(view) {
                    tab.remove_session(&id);
                }
            });
            open::cleanup_session(
                env,
                &result.session_id,
                &result.session,
                previous.import_temp_dir.as_deref(),
            );
            return Ok(IpcValue::Null);
        }
        if let Some(dir) = previous.import_temp_dir.clone() {
            let (id, mut twin) = (result.session_id.clone(), result.session.clone());
            twin.import_temp_dir = Some(dir);
            env.core(move |core| {
                if let Some(tab) = core.state.borrow_mut().tab_mut(view) {
                    tab.set_session(&id, twin);
                }
            });
        }
        return Ok(result.value);
    }
    // A crash re-open targets the file the user already has open
    if !Path::new(&path).exists() {
        return Err(thrown("Workbook file not found."));
    }
    let ext = open::extension_of(&path);
    if !open::WORKBOOK_EXTS.contains(&ext.as_str()) {
        return Err(thrown(format!("Unsupported workbook: {ext}")));
    }
    open::open_selected_workbook(env, view, Some(path), authority)
}

/// `selectWorkbooksForMerge`
fn select_for_merge(env: &Env, view: ViewId, authority: &open::OpenAuthority) -> Reply {
    use jscompat::JsonValue;
    let picked = dialogs::open_dialog(
        env,
        dialogs::dialog_parent(env, view),
        &dialogs::dialog_options(&[
            (
                "properties",
                dialogs::json_strings(&["openFile", "multiSelections"]),
            ),
            (
                "filters",
                JsonValue::Array(vec![dialogs::json_filter(
                    &env.tm.t("filterSpreadsheets"),
                    &open::WORKBOOK_EXTS,
                )]),
            ),
        ]),
    );
    if picked.canceled || picked.paths.is_empty() {
        return Ok(IpcValue::Null);
    }
    open::open_merge_sources(env, view, &picked.paths, authority)
}

/// `openWorkbooksForMerge`: chat attachments as merge-source sessions, no dialog
fn open_for_merge(
    env: &Env,
    view: ViewId,
    input: IpcValue,
    authority: &open::OpenAuthority,
) -> Reply {
    use std::path::Path;
    schema::parse(
        &schema::array(schema::string().min(1)).min(1).max(20),
        &input,
    )?;
    let paths: Vec<String> = match &input {
        IpcValue::Array(items) => items
            .iter()
            .filter_map(|item| match item {
                IpcValue::String(text) => Some(text.clone()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    for path in &paths {
        let ext = open::extension_of(path);
        if !open::WORKBOOK_EXTS.contains(&ext.as_str()) {
            return Err(thrown(format!("Unsupported merge source: {ext}")));
        }
        if !Path::new(path).exists() {
            return Err(thrown("Merge source not found."));
        }
        // an attached workbook open with unsaved changes is not read, like any attachment
        env.assert_file_saved(path).map_err(thrown)?;
    }
    open::open_merge_sources(env, view, &paths, authority)
}

/// This app's fake hosts for [`crate::testkit::Kit`]
#[cfg(any(test, feature = "probe"))]
pub fn insert_fakes(hosts: &mut shell_contract::Hosts, case: &serde_json::Value) {
    fakes::insert_fakes(hosts, case);
}

/// This app's triggers for [`crate::probe_trigger`]: `name` without its
/// `d1/` prefix; false when it is not one of them
#[cfg(feature = "probe")]
pub fn probe_trigger(
    name: &str,
    view: shell_contract::ViewId,
    args: &serde_json::Value,
    services: &Services,
    provided: &std::rc::Rc<shell_contract::provided::Provided>,
    then: shell_contract::editors::Then<shell_contract::IpcValue>,
) -> bool {
    use shell_contract::editors::editor_for;
    use shell_contract::{IpcValue, WindowId};

    // each arm runs what the TypeScript trigger of the same name runs
    // (`sheetsTriggers` of tools/ipc-diff/d1/sheets-host.ts)
    let parent = args
        .get(0)
        .and_then(serde_json::Value::as_u64)
        .map(|id| WindowId(id as u32));
    match name {
        // `requestSheetsClose(contents, parent)`
        "workbook:request-sheets-close" => match editor_for(provided, view) {
            Some(editor) => {
                editor.request_close(view, parent, Box::new(move |r| then(r.map(IpcValue::Bool))));
            }
            None => then(Err(crate::value::thrown("no sheets editor for the view"))),
        },
        // `request_save(view, SaveAs)`: see `request-save-as` of sheets-host.ts
        "workbook:request-save-as" => match editor_for(provided, view) {
            Some(editor) => editor.request_save(
                view,
                shell_contract::editors::SaveMode::SaveAs,
                Box::new(move |r| then(r.map(IpcValue::Bool))),
            ),
            None => then(Err(crate::value::thrown("no sheets editor for the view"))),
        },
        // `writeSheetsCopiesNow()`
        "workbook:write-sheets-copies-now" => match views() {
            Some(tabs) => tabs.write_copies_now(Box::new(move |result| {
                then(
                    result
                        .map(|_| IpcValue::Undefined)
                        .map_err(|error| crate::value::thrown(error.0)),
                )
            })),
            None => then(Err(crate::value::thrown(
                "the sheets module is not registered",
            ))),
        },
        // the sidecar process exits
        "workbook:sidecar-exit" => {
            if let Some(engine) = services.hosts.get::<std::sync::Arc<fakes::FakeEngine>>() {
                engine.exit();
            }
            then(Ok(IpcValue::Undefined));
        }
        // `webContents.on('render-process-gone')`
        "workbook:render-process-gone" => {
            if let Some(tabs) = views() {
                tabs.render_process_gone(view);
            }
            then(Ok(IpcValue::Undefined));
        }
        "workbook:mark-sheets-shutting-down" => {
            if let Some(tabs) = views() {
                tabs.mark_shutting_down();
            }
            then(Ok(IpcValue::Undefined));
        }
        "workbook:reset-sheets-shutting-down" => {
            if let Some(tabs) = views() {
                tabs.reset_shutting_down();
            }
            then(Ok(IpcValue::Undefined));
        }
        "workbook:discard-sheets-recovery" => {
            if let Some(tabs) = views() {
                tabs.discard_recovery(view);
            }
            then(Ok(IpcValue::Undefined));
        }
        // a held timer of this length (the close wait, the shutdown flush) ran
        "workbook:timeout-elapsed" => {
            if let Some(shell) = services.hosts.get::<std::sync::Arc<fakes::FakeShell>>() {
                shell.fire(args.get(0).and_then(serde_json::Value::as_u64).unwrap_or(0));
            }
            then(Ok(IpcValue::Undefined));
        }
        _ => return false,
    }
    true
}
