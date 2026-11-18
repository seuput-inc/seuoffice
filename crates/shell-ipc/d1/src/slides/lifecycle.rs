//! Document lifecycle channels of `slides-main.ts`: open, open-path,
//! consume-pending-open, new-blank, recent, is-dirty, close guard,
//! autosave pref, undo/redo and history batches.

use jscompat::JsonValue;
use jscompat::node_path::host::basename;
use main_core::packages::electron_utils::recovery_store::RecoveryDeleteReason;
use main_core::packages::electron_utils::save_conflict::record_disk_state;
use shell_contract::editors::Then;
use shell_contract::hosts::MessageBox;
use shell_contract::ipc::{CallContext, Responder};
use shell_contract::{IpcError, IpcValue, ViewId, WindowId};

use super::dialog::{filter, jstrs, options};
use super::engine::PreparedOpen;
use super::session::{null, number};
use super::state::{App, DeckHandle, SlideSession, StagedBinding};
use crate::host::path_string;
use crate::value::{Args, as_bool, as_num, field, num, obj, or_null, string, thrown};

type Reply = Result<IpcValue, IpcError>;

pub const AI_REVIEW_SAVE_REASON: &str = "ai-review";
/// how long `requestRendererSave` waits for the renderer's answer
pub(super) const CLOSE_SAVE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
const AI_RUN_OPEN: &str = "An AI request is editing this deck; saving waits for your review";

/// `'Error: ' + message`: `String(err)` of a thrown `Error`
fn error_text(message: &str) -> String {
    format!("Error: {message}")
}

/// `readFile(path)` as a rejection a handler lets through
pub fn read_file(path: &str) -> Result<Vec<u8>, IpcError> {
    // `readFile` opens, then reads: a folder opens and fails to read
    let mut file = std::fs::File::open(path)
        .map_err(|e| thrown(main_core::node_fs::node_error(&e, "open", path).message))?;
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut file, &mut bytes)
        .map_err(|e| thrown(main_core::node_fs::node_error_fd(&e, "read").message))?;
    Ok(bytes)
}

/// `sanitizeDraftBaseName(raw)`
pub fn sanitize_draft_base_name(raw: Option<&str>) -> Option<String> {
    let raw = raw.filter(|r| !r.is_empty())?;
    let replaced: String = raw
        .chars()
        .map(|c| {
            if matches!(c, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
                || (c as u32) < 0x20
            {
                ' '
            } else {
                c
            }
        })
        .collect();
    // `\s+` -> ' '
    let mut collapsed = String::new();
    let mut in_space = false;
    for c in replaced.chars() {
        if jscompat::is_js_space(c) {
            if !in_space {
                collapsed.push(' ');
            }
            in_space = true;
        } else {
            collapsed.push(c);
            in_space = false;
        }
    }
    let trimmed = jscompat::trim(&collapsed).to_owned();
    let stripped = trimmed.trim_matches('.');
    let cleaned = jscompat::trim(stripped).to_owned();
    if cleaned.is_empty() {
        return None;
    }
    let units: Vec<u16> = cleaned.encode_utf16().collect();
    if units.len() > 40 {
        let cut = String::from_utf16_lossy(&units[..40]);
        return Some(jscompat::trim(&cut).to_owned());
    }
    Some(cleaned)
}

impl App {
    /// `projectDeckFileName(firstSaveName)`
    pub fn project_deck_file_name(&self, first_save_name: Option<&str>) -> String {
        let named = first_save_name
            .filter(|n| !n.is_empty())
            .map(|n| {
                let base = basename(n, None);
                match base.to_lowercase().strip_suffix(".pptx") {
                    Some(stem) => base[..stem.len()].to_owned(),
                    None => base,
                }
            })
            .unwrap_or_default();
        let base = sanitize_draft_base_name(Some(&named))
            .or_else(|| sanitize_draft_base_name(Some(&self.tm("untitledDeck", &[]))));
        format!("{}.pptx", base.unwrap_or_else(|| "presentation".to_owned()))
    }

    /// The Electron `Buffer` a save writes: re-encrypted for a password deck
    fn deck_file_bytes(&self, s: &SlideSession, bytes: Vec<u8>) -> Result<Vec<u8>, String> {
        match &s.password {
            None => Ok(bytes),
            Some(password) => self
                .engine
                .encrypt_ooxml(&bytes, password)
                .map_err(|e| error_text(&e.0)),
        }
    }

    /// `writeDeckFile(session, filePath)`: every save funnels through here
    pub fn write_deck_file(&self, s: &SlideSession, path: &str) -> Result<(), String> {
        if s.core.ai_run {
            return Err(error_text(AI_RUN_OPEN));
        }
        let deck = s.deck();
        let engine = self.engine.clone();
        match &s.password {
            None => main_core::packages::electron_utils::atomic_write::atomic_write_via(
                path,
                &mut |tmp| {
                    engine
                        .save_to_file(deck, tmp)
                        .map_err(|e| main_core::node_fs::NodeError {
                            code: None,
                            message: e.0,
                        })
                },
            )
            .map_err(|e| error_text(&e.message)),
            Some(_) => {
                let bytes = self.engine.save_bytes(deck).map_err(|e| error_text(&e.0))?;
                let bytes = self.deck_file_bytes(s, bytes)?;
                main_core::packages::electron_utils::atomic_write::atomic_write_file(path, &bytes)
                    .map_err(|e| error_text(&e.message))
            }
        }
    }

    /// `syncAttachedPaths(session, path)`
    pub fn sync_attached_paths(&self, slot: usize, path: &str, ids: &[ViewId]) {
        let _ = slot;
        for view in ids {
            self.shell.opened(*view, path);
            self.emit(*view, "slides:renamed", vec![string(path)]);
            self.shell.set_window_title(*view, &basename(path, None));
        }
    }

    // ---- open ----

    /// `openAndBuild(wc, path, fitWidthPx, unlocked)`
    pub fn open_and_build(
        &self,
        owner: &super::engine::RenderOwner,
        path: &str,
        fit_width_px: f64,
        unlocked: Option<(Vec<u8>, String)>,
    ) -> Reply {
        let cancel = owner
            .loss_token()
            .ok_or_else(|| thrown("The native presentation owner lifetime is unavailable"))?;
        let authority = super::open_authority::OpenAuthority::capture(self, owner, cancel)?;
        self.open_and_build_owned(owner, path, fit_width_px, unlocked, &authority)
    }

    fn open_and_build_owned(
        &self,
        owner: &super::engine::RenderOwner,
        path: &str,
        fit_width_px: f64,
        unlocked: Option<(Vec<u8>, String)>,
        authority: &super::open_authority::OpenAuthority,
    ) -> Reply {
        let view = owner.view;
        // Same file already open in another window: attach to that session
        let wanted = std::path::absolute(path).unwrap_or_else(|_| path.into());
        {
            let state = self.state.borrow_mut();
            let mut found = None;
            // A preselected recovery decision must open its own bytes.
            for slot in state
                .live_slots()
                .into_iter()
                .filter(|_| !state.recovery_open_decisions.contains_key(&view))
            {
                let Some(existing) = state.slot(slot) else {
                    continue;
                };
                if existing.path().is_empty() {
                    continue;
                }
                if state.slot_of(view) == Some(slot) {
                    continue;
                }
                let theirs =
                    std::path::absolute(existing.path()).unwrap_or_else(|_| existing.path().into());
                if theirs == wanted {
                    found = Some(slot);
                    break;
                }
            }
            if let Some(slot) = found {
                let existing = state.slot(slot).expect("existing slot");
                let staged = StagedBinding::new(self.engine.clone(), existing.deck(), owner)
                    .map_err(|error| thrown(error.0))?;
                let result = self.open_result(existing, self.render_all(existing, owner)?)?;
                let existing_path = existing.path().to_owned();
                drop(state);
                authority.commit(self, owner, Some(slot), None, staged)?;
                if let Some(session) = self.state.borrow_mut().slot_mut(slot) {
                    session.core.schedule_history_notify();
                }
                self.push_recent(path);
                self.shell.opened(view, &existing_path);
                return Ok(result);
            }
        }
        let (raw, password) = match unlocked {
            Some((bytes, password)) => (bytes, Some(password)),
            None => (read_file(path)?, None),
        };
        // the split-view save guard's baseline
        let disk_state = record_disk_state(path);
        // a 0-byte .pptx is an empty deck, not a corrupt one
        let original = if raw.is_empty() {
            self.engine.create_blank_pptx().map_err(|e| thrown(e.0))?
        } else {
            raw
        };
        let recovered = self.recovered_open_bytes(view, path, original, password.as_deref())?;
        let deck = self.engine.open(&recovered.bytes).map_err(|e| {
            if recovered.recovered {
                self.set_copy_later(path);
            }
            thrown(e.0)
        })?;
        let handle = DeckHandle::new(deck, self.engine.clone());
        let mut session = SlideSession::new(path, handle, fit_width_px);
        let staged = StagedBinding::new(self.engine.clone(), deck, owner)
            .map_err(|error| thrown(error.0))?;
        self.engine
            .adopt_embedded_fonts(deck, owner)
            .map_err(|error| thrown(error.0))?;
        if recovered.recovered {
            session.core.meta_dirty = true;
        }
        session.disk_state = disk_state;
        session.password = password;
        session.core.schedule_history_notify();
        let slides = self.render_all(&session, owner)?;
        let payload = if recovered.recovered {
            Some(obj([
                ("slides", slides.clone()),
                (
                    "size",
                    self.size_value(self.engine.size(deck).map_err(|error| thrown(error.0))?),
                ),
            ]))
        } else {
            None
        };
        let result = self.open_result(&session, slides)?;
        let owner = authority.commit(self, owner, None, Some(session), staged)?;
        self.push_recent(path);
        self.shell.opened(view, path);
        if let Some(payload) = payload {
            // An open shows the deck as clean; the deck-changed message right
            // after it makes the renderer ask the session, which answers "unsaved"
            let owner = owner.clone();
            self.post(move |app| {
                if let Err(error) = app.services.events.emit_to_owner_checked(
                    &owner,
                    "slides:deck-changed",
                    vec![payload],
                ) {
                    eprintln!("[slides] scoped open notification failed: {error}");
                }
            });
        }
        Ok(result)
    }

    fn set_copy_later(&self, path: &str) {
        use main_core::packages::electron_utils::recovery_store::RecoveryCopyState;
        let identity = super::recovery::file_recovery_id(path);
        self.with_recovery_store(|store| {
            let _ = store.set_copy_state(&identity, RecoveryCopyState::Later);
        });
    }

    /// Prepares `path` for open (conversions, password), then runs `then` on
    /// the core thread
    fn prepare_then(
        &self,
        authority: super::open_authority::OpenAuthority,
        path: String,
        then: impl FnOnce(&App, Result<(PreparedOpen, super::open_authority::OpenAuthority), IpcError>)
        + Send
        + 'static,
    ) {
        let parent = self.shell.dialog_parent();
        let index = self.registry_index;
        let services = self.services.clone();
        self.shell.prepare_for_open_owned(
            parent,
            &path,
            authority.source.clone(),
            authority.source.clone(),
            authority.cancel.clone(),
            authority.cancel.clone(),
            Box::new(move |prepared| {
                super::post_to(&services, index, move |app| {
                    then(
                        app,
                        prepared
                            .map(|prepared| (prepared, authority))
                            .map_err(thrown),
                    )
                });
            }),
        );
    }

    fn open_prepared(
        &self,
        owner: &super::engine::RenderOwner,
        fit: f64,
        prepared: PreparedOpen,
        authority: &super::open_authority::OpenAuthority,
    ) -> Reply {
        match prepared {
            PreparedOpen::Rejected => null(),
            PreparedOpen::Plain(path) => {
                self.open_and_build_owned(owner, &path, fit, None, authority)
            }
            PreparedOpen::Unlocked {
                path,
                bytes,
                password,
            } => self.open_and_build_owned(owner, &path, fit, Some((bytes, password)), authority),
        }
    }
}

fn answer(responder: Responder, reply: Reply, app: &App) {
    responder.reply(reply);
    app.pump();
}

pub fn open(app: &App, cx: &CallContext, args: Args, responder: Responder) {
    let owner = match super::session::render_owner(cx) {
        Ok(owner) => owner,
        Err(error) => return responder.reply(Err(error)),
    };
    let authority =
        match super::open_authority::OpenAuthority::capture(app, &owner, cx.cancel.clone()) {
            Ok(authority) => authority,
            Err(error) => return responder.reply(Err(error)),
        };
    let fit = number(args.get(0));
    let options = options([
        ("properties", jstrs(&["openFile"])),
        (
            "filters",
            JsonValue::Array(vec![filter("PowerPoint", &["pptx", "ppt", "odp", "otp"])]),
        ),
    ]);
    let options = match app.shared.dialog_memory.lock() {
        Ok(memory) => crate::host::file_dialog(
            &main_core::packages::electron_utils::dialog_memory::prepare_dialog_options(
                &memory, &options, None,
            ),
        ),
        Err(_) => return responder.reply(Err(thrown("The dialog memory is unavailable"))),
    };
    let effects = match app
        .services
        .hosts
        .get::<shell_contract::hosts::OwnedEffectsHost>()
        .cloned()
    {
        Some(effects) => effects,
        None => {
            return responder.reply(Err(thrown("The native dialog effect host is unavailable")));
        }
    };
    let services = app.services.clone();
    let index = app.registry_index;
    app.dialogs.open_owned(
        app.shell.dialog_parent(),
        options,
        authority.source.clone(),
        authority.source.clone(),
        authority.cancel.clone(),
        authority.cancel.clone(),
        Box::new(move |picked| {
            super::post_to(&services, index, move |app| {
                let paths = match picked {
                    Ok(paths) => paths,
                    Err(error) => return responder.reply(Err(thrown(error))),
                };
                let Some(path) = paths
                    .and_then(|paths| paths.into_iter().next())
                    .map(path_string)
                    .filter(|path| !path.is_empty())
                else {
                    return responder.reply(null());
                };
                let remembered = effects.with_owned_effect(
                    &authority.source,
                    &authority.source,
                    &authority.cancel,
                    &authority.cancel,
                    &mut || {
                        let mut memory = app
                            .shared
                            .dialog_memory
                            .lock()
                            .map_err(|_| "The dialog memory is unavailable".to_owned())?;
                        main_core::packages::electron_utils::dialog_memory::remember_open_path(
                            &mut memory,
                            &path,
                            false,
                        );
                        Ok(())
                    },
                );
                if let Err(error) = remembered {
                    return responder.reply(Err(thrown(error)));
                }
                app.prepare_then(authority, path, move |app, prepared| {
                    let reply = prepared.and_then(|(prepared, authority)| {
                        app.open_prepared(&owner, fit, prepared, &authority)
                    });
                    answer(responder, reply, app);
                });
            });
        }),
    );
}

pub fn open_path(app: &App, cx: &CallContext, args: Args, responder: Responder) {
    let owner = match super::session::render_owner(cx) {
        Ok(owner) => owner,
        Err(error) => return responder.reply(Err(error)),
    };
    let authority =
        match super::open_authority::OpenAuthority::capture(app, &owner, cx.cancel.clone()) {
            Ok(authority) => authority,
            Err(error) => return responder.reply(Err(error)),
        };
    let fit = number(args.get(1));
    let Some(path) = args
        .str(0)
        .filter(|p| !p.is_empty() && std::path::Path::new(p).exists())
    else {
        return responder.reply(null());
    };
    app.prepare_then(authority, path.to_owned(), move |app, prepared| {
        let reply = prepared
            .and_then(|(prepared, authority)| app.open_prepared(&owner, fit, prepared, &authority));
        answer(responder, reply, app);
    });
}

pub fn consume_pending_open(app: &App, cx: &CallContext, args: Args, responder: Responder) {
    let view = cx.sender.view;
    let owner = match super::session::render_owner(cx) {
        Ok(owner) => owner,
        Err(error) => return responder.reply(Err(error)),
    };
    let fit = number(args.get(0));
    if app.state.borrow().saved_loads.contains_key(&view) {
        return super::saved_load::consume(app, cx, fit, responder);
    }
    let mut response = Some(responder);
    let recovered = super::recovery::consume_prepared(app, cx, fit, &mut response);
    let Some(responder) = response else {
        return;
    };
    match recovered {
        Ok(Some(value)) => return answer(responder, Ok(value), app),
        Err(error) => return answer(responder, Err(error), app),
        Ok(None) => {}
    }
    if app.state.borrow().owned_seeds.contains_key(&view) {
        let result = app
            .open_owned_seed(cx, fit)
            .and_then(|value| value.ok_or_else(|| thrown("The presentation seed was released")));
        return answer(responder, result, app);
    }
    let authority =
        match super::open_authority::OpenAuthority::capture(app, &owner, cx.cancel.clone()) {
            Ok(authority) => authority,
            Err(error) => {
                eprintln!(
                    "[slides] view {}: consume-pending-open refused: {}",
                    view.0, error.message
                );
                return responder.reply(Err(error));
            }
        };
    consume_pending_ready(app, owner, authority, fit, responder);
}

pub(super) fn consume_pending_ready(
    app: &App,
    owner: super::engine::RenderOwner,
    authority: super::open_authority::OpenAuthority,
    fit: f64,
    responder: Responder,
) {
    let view = owner.view;
    let queued = {
        let state = app.state.borrow();
        state
            .pending_by_view
            .get(&view)
            .cloned()
            .or_else(|| state.pending_open_path.clone())
    };
    eprintln!(
        "[slides] view {}: consume-pending-open finds queued path {queued:?} (exists: {})",
        view.0,
        queued
            .as_deref()
            .is_some_and(|q| std::path::Path::new(q).exists())
    );
    if let Some(queued) = queued.filter(|q| std::path::Path::new(q).exists()) {
        fn drop_queued(app: &App, view: ViewId, queued: &str) {
            let mut state = app.state.borrow_mut();
            if state.pending_by_view.get(&view).map(String::as_str) == Some(queued) {
                state.pending_by_view.remove(&view);
            }
            if state.pending_open_path.as_deref() == Some(queued) {
                state.pending_open_path = None;
            }
        }
        let queued_for_then = queued.clone();
        app.prepare_then(authority, queued, move |app, prepared| {
            // clear the queue only after a successful open: keep it on a
            // parse failure, so a remount can retry
            let reply = prepared.and_then(|(prepared, authority)| match prepared {
                PreparedOpen::Rejected => {
                    drop_queued(app, view, &queued_for_then);
                    null()
                }
                other => {
                    let r = app.open_prepared(&owner, fit, other, &authority);
                    if r.is_ok() {
                        drop_queued(app, view, &queued_for_then);
                    }
                    r
                }
            });
            answer(responder, reply, app);
        });
        return;
    }
    // A new tab that starts with a given deck: taken once, unsaved
    let reply = app
        .open_seed(&owner, fit, &authority)
        .and_then(|seeded| match seeded {
            Some(result) => Ok(result),
            None => {
                // No queued path but the main process already has a session
                // (remount after a reload): restore from the session
                let mut state = app.state.borrow_mut();
                match state.session_mut(view) {
                    Some(session) => {
                        session.fit_width_px = fit;
                        let slides = app.render_all(session, &owner)?;
                        app.open_result(session, slides)
                    }
                    None => null(),
                }
            }
        });
    answer(responder, reply, app);
}

impl App {
    /// `openSlidesSeed(wc, fitWidthPx, defaultFontOf)`
    fn open_seed(
        &self,
        owner: &super::engine::RenderOwner,
        fit: f64,
        authority: &super::open_authority::OpenAuthority,
    ) -> Result<Option<IpcValue>, IpcError> {
        let view = owner.view;
        let entry = self
            .state
            .borrow_mut()
            .seeds
            .take_slides_seed_entry(f64::from(view.0));
        let Some(entry) = entry else {
            return Ok(None);
        };
        let deck = self.engine.open(&entry.deck).map_err(|e| thrown(e.0))?;
        let mut session = SlideSession::new("", DeckHandle::new(deck, self.engine.clone()), fit);
        let staged = StagedBinding::new(self.engine.clone(), deck, owner)
            .map_err(|error| thrown(error.0))?;
        if entry.options.ask_path_on_first_save == Some(true) {
            session.ask_path_on_first_save = true;
            session.first_save_name = entry.options.suggested_name.clone();
        }
        session.recovery_id = entry.options.recovery_id.clone();
        session.password = entry.options.password.clone();
        // never saved: the deck holds what the disk does not
        session.core.mark_meta_dirty();
        session.core.schedule_history_notify();
        let slides = self.render_all(&session, owner)?;
        let payload = obj([
            ("slides", slides.clone()),
            (
                "size",
                self.size_value(self.engine.size(deck).map_err(|error| thrown(error.0))?),
            ),
        ]);
        let result = self.open_result(&session, slides)?;
        let owner = authority.commit(self, owner, None, Some(session), staged)?;
        // An open shows the deck as clean; the deck-changed message right
        // after it makes the renderer ask the session, which answers "unsaved"
        self.post(move |app| {
            if let Err(error) = app.services.events.emit_to_owner_checked(
                &owner,
                "slides:deck-changed",
                vec![payload],
            ) {
                eprintln!("[slides] scoped seed notification failed: {error}");
            }
        });
        Ok(Some(result))
    }
}

pub fn new_blank(app: &App, cx: &CallContext, args: Args) -> Reply {
    let owner = super::session::render_owner(cx)?;
    let authority = super::open_authority::OpenAuthority::capture(app, &owner, cx.cancel.clone())?;
    let fit = number(args.get(0));
    let bytes = app.engine.create_blank_pptx().map_err(|e| thrown(e.0))?;
    let deck = app.engine.open(&bytes).map_err(|e| thrown(e.0))?;
    let mut session = SlideSession::new("", DeckHandle::new(deck, app.engine.clone()), fit);
    let staged =
        StagedBinding::new(app.engine.clone(), deck, &owner).map_err(|error| thrown(error.0))?;
    let side = |v: &IpcValue| -> Option<f64> {
        as_num(v).filter(|x| x.fract() == 0.0 && (914_400.0..=51_206_400.0).contains(x))
    };
    let size = args.get(1);
    if let (Some(cx_), Some(cy)) = (side(field(size, "cx")), side(field(size, "cy"))) {
        app.engine
            .set_slide_size(deck, cx_, cy)
            .map_err(|error| thrown(error.0))?;
    }
    session.core.schedule_history_notify();
    let slides = app.render_all(&session, &owner)?;
    let result = app.open_result(&session, slides)?;
    authority.commit(app, &owner, None, Some(session), staged)?;
    Ok(result)
}

pub fn recent(app: &App, _cx: &CallContext, _args: Args, responder: Responder) {
    app.read_recent(move |paths| {
        responder.reply(Ok(IpcValue::Array(paths.into_iter().map(string).collect())))
    });
}

// ---- close guard ----

pub fn autosave_pref(app: &App, cx: &CallContext, args: Args) -> Result<(), IpcError> {
    app.state
        .borrow_mut()
        .auto_save_pref
        .insert(cx.sender.view, as_bool(args.get(0)) == Some(true));
    Ok(())
}

pub fn close_save_result(app: &App, cx: &CallContext, args: Args) -> Result<(), IpcError> {
    if let Some(id) = args.str(1) {
        return app.owned_save_answered(cx, id, as_bool(args.get(0)) == Some(true));
    }
    let waiter = {
        let mut state = app.state.borrow_mut();
        let view = cx.sender.view;
        let at = state.close_waiters.iter().rposition(|w| w.0 == view);
        at.map(|at| state.close_waiters.remove(at).2)
    };
    if let Some(waiter) = waiter {
        waiter(as_bool(args.get(0)) == Some(true));
    }
    Ok(())
}

pub fn is_dirty(app: &App, cx: &CallContext, _args: Args) -> Reply {
    Ok(IpcValue::Bool(app.is_view_dirty(cx.sender.view)?))
}

impl App {
    /// `slidesIsDirty(webContentsId)`
    pub fn is_view_dirty(&self, view: ViewId) -> Result<bool, IpcError> {
        match self.state.borrow().session(view) {
            Some(session) => self.session_dirty(session).map_err(|error| thrown(error.0)),
            None => Ok(false),
        }
    }

    /// `requestRendererSave(contents)`: asks the renderer to run the full
    /// save flow; `done` runs when it answers (or the view is gone: false)
    pub fn request_renderer_save(&self, view: ViewId, done: Box<dyn FnOnce(bool)>) {
        let ticket = {
            let mut state = self.state.borrow_mut();
            state.next_ticket += 1;
            let ticket = state.next_ticket;
            state.close_waiters.push((view, ticket, done));
            ticket
        };
        self.emit(view, "slides:close-save-request", Vec::new());
        // failure/timeout = false: the renderer gets two minutes
        self.after(CLOSE_SAVE_TIMEOUT, move |app| {
            app.close_save_expired(view, ticket)
        });
    }

    /// The two minutes of save `ticket` are over. A save already answered
    /// (or ended with its view) has no timer in the TypeScript, which clears
    /// it on the answer: nothing happens then.
    pub(crate) fn close_save_expired(&self, view: ViewId, ticket: u64) {
        let (own, current) = {
            let mut state = self.state.borrow_mut();
            let Some(at) = state.close_waiters.iter().position(|w| w.1 == ticket) else {
                return;
            };
            let own = state.close_waiters.remove(at).2;
            // `closeSaveWaiters.delete(contents.id)`: the view's current waiter goes too
            let current = state
                .close_waiters
                .iter()
                .rposition(|w| w.0 == view)
                .map(|at| state.close_waiters.remove(at));
            (own, current)
        };
        drop(current);
        own(false);
    }

    /// The File menu's Save As: the renderer is told with `slides:menu` and runs
    /// its Save As, which calls `slides:save-as`; `done` gets what that came to
    /// (`true` saved, `false` cancelled or failed), or `false` when the view goes
    /// or the renderer does not call within two minutes. A newer request of the
    /// same view replaces the older, which resumes `false`.
    pub fn request_save_as(&self, view: ViewId, done: Box<dyn FnOnce(bool)>) {
        let (ticket, older) = {
            let mut state = self.state.borrow_mut();
            state.next_ticket += 1;
            let ticket = state.next_ticket;
            let older = state.save_as_waiters.insert(view, (ticket, done));
            (ticket, older)
        };
        if let Some((_, older)) = older {
            older(false);
        }
        self.emit(view, "slides:menu", vec![string("save-as")]);
        self.after(CLOSE_SAVE_TIMEOUT, move |app| {
            app.save_as_expired(view, ticket)
        });
    }

    /// The two minutes of Save As `ticket` are over. One answered, replaced or
    /// ended with its view has nothing waiting for this timer.
    pub(crate) fn save_as_expired(&self, view: ViewId, ticket: u64) {
        let waiter = {
            let mut state = self.state.borrow_mut();
            match state.save_as_waiters.get(&view) {
                Some((current, _)) if *current == ticket => state.save_as_waiters.remove(&view),
                _ => None,
            }
        };
        if let Some((_, waiter)) = waiter {
            waiter(false);
        }
    }

    /// The renderer's `slides:save-as` answered: resumes the Save As the menu asked for
    pub(crate) fn save_as_answered(&self, view: ViewId, reply: &Reply) {
        let waiter = self.state.borrow_mut().save_as_waiters.remove(&view);
        if let Some((_, waiter)) = waiter {
            let saved =
                matches!(reply, Ok(value) if matches!(field(value, "ok"), IpcValue::Bool(true)));
            waiter(saved);
        }
    }

    /// `requestSlidesClose(contents, parent)`
    pub fn request_close(&self, view: ViewId, parent: Option<WindowId>, then: Then<bool>) {
        match self.is_view_dirty(view) {
            Err(error) => return then(Err(error)),
            Ok(false) => return then(Ok(true)),
            Ok(true) => {}
        }
        if self.services.views.sender_of(view).is_none() {
            return then(Ok(true));
        }
        // A shared session stays alive in another EDITOR window
        let (shared, autosave_with_path) = {
            let state = self.state.borrow();
            let slot = state.slot_of(view);
            let shared = slot.is_some_and(|slot| state.editor_attached_ids(slot).len() > 1);
            let autosave = state.auto_save_pref.get(&view).copied().unwrap_or(false)
                && state.session(view).is_some_and(|s| !s.path().is_empty());
            (shared, autosave)
        };
        if shared {
            return then(Ok(true));
        }
        // Autosave on and a path exists: save silently and proceed; only a
        // failed save falls through to the dialog
        if autosave_with_path {
            let app = self.clone();
            self.request_renderer_save(
                view,
                Box::new(move |ok| {
                    if ok {
                        then(Ok(true))
                    } else {
                        app.close_dialog(view, parent, then)
                    }
                }),
            );
            return;
        }
        self.close_dialog(view, parent, then)
    }

    fn close_dialog(&self, view: ViewId, parent: Option<WindowId>, then: Then<bool>) {
        let message = MessageBox {
            kind: Some("warning".into()),
            title: None,
            message: self.tm("closeUnsavedMsg", &[]),
            detail: Some(self.tm("closeUnsavedDetail", &[])),
            buttons: vec![
                self.tm("menuSave", &[]),
                self.tm("btnDontSave", &[]),
                self.tm("btnCancel", &[]),
            ],
            default_id: Some(0),
            cancel_id: Some(2),
            checkbox_label: None,
        };
        let response = self.message_box(parent, &message);
        match response {
            2 => then(Ok(false)),
            1 => {
                // The user discarded the changes: the copy goes too
                if let Some(s) = self.state.borrow().session(view) {
                    self.delete_session_copies(s, RecoveryDeleteReason::DontSave);
                }
                then(Ok(true))
            }
            _ => self.request_renderer_save(view, Box::new(move |ok| then(Ok(ok)))),
        }
    }

    /// `discardSlidesRecovery(contents)`
    pub fn discard_recovery(&self, view: ViewId) {
        if let Some(s) = self.state.borrow().session(view) {
            self.delete_session_copies(s, RecoveryDeleteReason::DontSave);
        }
    }

    /// `saveSessionDeckTo(session, filePath)`: the save-as pipeline minus
    /// the dialog
    pub fn save_session_deck_to(&self, view: ViewId, file_path: &str) -> Result<(), IpcError> {
        let mut state = self.state.borrow_mut();
        let Some(slot) = state.slot_of(view) else {
            return Err(thrown("no file open"));
        };
        let ids = state.attached_ids(slot);
        let s = state.slot_mut(slot).ok_or_else(|| thrown("no file open"))?;
        // the caller supplies an arbitrary absolute path: its parent may not exist yet
        if let Some(parent) = std::path::Path::new(file_path).parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                thrown(
                    main_core::node_fs::node_error(&e, "mkdir", &path_string(parent.into()))
                        .message,
                )
            })?;
        }
        let meta_rev_at_save = s.core.meta_rev.unwrap_or(0);
        let previous_path = s.path().to_owned();
        self.write_deck_file(s, file_path).map_err(thrown_text)?;
        self.follow_template(s, s.path(), file_path);
        s.core.path = file_path.to_owned();
        s.disk_state = record_disk_state(file_path);
        self.push_recent(file_path);
        self.sync_attached_paths(slot, file_path, &ids);
        self.engine
            .commit_saved(s.deck())
            .map_err(|error| thrown(error.0))?;
        if s.core.meta_rev.unwrap_or(0) == meta_rev_at_save {
            s.core.meta_dirty = false;
        }
        self.recovery_copies_saved(s, &previous_path);
        Ok(())
    }

    /// `slidesFileRenamed(wc, oldPath, newPath)`
    pub fn file_renamed(&self, view: ViewId, old_path: &str, new_path: &str) {
        if let Some(s) = self.state.borrow_mut().session_mut(view)
            && s.path() == old_path
        {
            s.core.path = new_path.to_owned();
        }
        self.emit(view, "slides:renamed", vec![string(new_path)]);
    }
}

/// A rejection whose message is already `String(err)`
fn thrown_text(text: String) -> IpcError {
    let message = text.strip_prefix("Error: ").unwrap_or(&text).to_owned();
    thrown(message)
}

// ---- undo / redo / history batches ----

pub fn get_render_slides(app: &App, cx: &CallContext, _args: Args) -> Reply {
    app.with_session(cx.sender.view, |app, s| {
        app.rebuild_every_slide(s, &super::session::render_owner(cx)?)
    })
}

pub fn undo(app: &App, cx: &CallContext, _args: Args) -> Reply {
    app.step_history(&super::session::render_owner(cx)?, true)
}

pub fn redo(app: &App, cx: &CallContext, _args: Args) -> Reply {
    app.step_history(&super::session::render_owner(cx)?, false)
}

impl App {
    /// Runs `f` with the sender's session and the state's id counter
    pub fn with_session_ids<R>(
        &self,
        view: ViewId,
        f: impl FnOnce(&mut SlideSession, &mut main_core::slides::session_state::SessionIds) -> R,
    ) -> Option<R> {
        let mut state = self.state.borrow_mut();
        let mut ids = std::mem::take(&mut state.ids);
        let result = state.session_mut(view).map(|s| f(s, &mut ids));
        state.ids = ids;
        result
    }

    fn step_history(&self, owner: &super::engine::RenderOwner, undo: bool) -> Reply {
        let answer = self.with_session_ids(
            owner.view,
            |s, ids| -> Result<Option<IpcValue>, super::engine::EngineError> {
                if s.master_edit.is_some() || s.core.ai_run {
                    return Ok(None);
                }
                let target = if undo {
                    match s.core.history_batch.as_ref() {
                        Some(batch) if s.core.undo_stack.len() > batch.undo_start => {
                            Some(&batch.before)
                        }
                        _ => s.core.undo_stack.last(),
                    }
                } else {
                    s.core.redo_stack.last()
                }
                .cloned();
                let Some(target) = target else {
                    return Ok(None);
                };
                let current = s.core.take_snapshot()?;
                s.core.restore_snapshot(&target)?;
                let rendered = match self.render_all(s, owner) {
                    Ok(rendered) => rendered,
                    Err(error) => {
                        s.core.restore_snapshot(&current).map_err(|rollback| {
                            super::engine::EngineError(format!(
                                "{}; history rollback failed: {}",
                                error.message, rollback.0
                            ))
                        })?;
                        return Err(super::engine::EngineError(error.message));
                    }
                };
                s.core.settle_stale_history_batch(ids);
                if undo {
                    s.core.undo_stack.pop();
                    s.core.redo_stack.push(current);
                } else {
                    s.core.redo_stack.pop();
                    s.core.undo_stack.push(current);
                }
                s.core.schedule_history_notify();
                s.core.schedule_deck_broadcast();
                Ok(Some(rendered))
            },
        );
        Ok(or_null(
            answer
                .transpose()
                .map_err(|error| thrown(error.0))?
                .flatten(),
        ))
    }
}

pub fn history_batch_begin(app: &App, cx: &CallContext, _args: Args) -> Reply {
    let mut state = app.state.borrow_mut();
    Ok(IpcValue::Bool(match state.session_mut(cx.sender.view) {
        Some(s) => {
            s.core
                .begin_history_batch()
                .map_err(|error| thrown(error.0))?;
            true
        }
        None => false,
    }))
}

pub fn history_batch_end(app: &App, cx: &CallContext, _args: Args) -> Reply {
    let id = app.with_session_ids(cx.sender.view, |s, ids| {
        s.core
            .end_history_batch()
            .map(|before| s.core.register_ai_snapshot(ids, &before))
    });
    Ok(match id.flatten() {
        Some(id) => num(id as f64),
        None => IpcValue::Null,
    })
}
