//! Owned EPUB saves correlate renderer flushes and retain the captured document
//! through worker serialization, atomic writes, result migration and publication.

use super::*;
use shell_contract::editors::{SaveMode, Then};
use shell_contract::hosts::OwnedEffectsHost;
use shell_contract::ipc::NativeRequestOwner;
use shell_contract::runtime::{CancelSubscription, CancelToken, RendererGeneration};

pub(super) mod close;

#[derive(Clone)]
enum SaveAuthority {
    Renderer(NativeRequestOwner),
    Native(shell_contract::ipc::NativeExistingDocumentBinding),
}

enum BookDestination {
    Renderer(PathBuf),
    Native(shell_contract::ipc::NativeSaveDestination),
}

#[derive(Clone)]
pub(super) struct Scope {
    authority: SaveAuthority,
    destination: NativeRequestOwner,
    cancel: CancelToken,
    pending: CancelToken,
    document: RendererGeneration,
    effects: OwnedEffectsHost,
    target: String,
    native_destination: Option<shell_contract::ipc::NativeSaveDestination>,
    additional: Vec<CancelToken>,
    callback_owner: Option<NativeRequestOwner>,
}

impl Scope {
    pub(super) fn check(&self) -> Result<(), String> {
        if self.cancel.is_cancelled()
            || self.pending.is_cancelled()
            || self.additional.iter().any(CancelToken::is_cancelled)
            || !self.document.is_active()
        {
            return Err("The book save was cancelled".into());
        }
        if let SaveAuthority::Native(binding) = &self.authority {
            let operation = binding.operation();
            if operation.loss_token().is_cancelled()
                || operation.cancel().is_cancelled()
                || operation.shutdown().is_cancelled()
            {
                return Err("The native save operation was released".into());
            }
        }
        let origin = match &self.authority {
            SaveAuthority::Renderer(owner) => Some(owner),
            SaveAuthority::Native(_) => None,
        };
        for owner in origin.into_iter().chain(std::iter::once(&self.destination)) {
            if !owner
                .context
                .renderer()
                .is_some_and(RendererGeneration::is_active)
                || !owner.loss_token().is_some_and(|loss| !loss.is_cancelled())
            {
                return Err("The save owner was released".into());
            }
        }
        Ok(())
    }

    fn participants(&self) -> Vec<NativeRequestOwner> {
        let mut participants = vec![self.destination.clone()];
        if let Some(owner) = &self.callback_owner {
            participants.push(owner.clone());
        }
        participants
    }

    fn capture_callback(&self, cx: &CallContext) -> Result<Self, String> {
        self.check()?;
        let actual = NativeRequestOwner {
            view: cx.sender.view,
            context: cx
                .native
                .clone()
                .ok_or("The book callback has no native authority")?,
        };
        let mut additional = self.additional.clone();
        additional.push(cx.cancel.clone());
        let refs = additional.iter().collect::<Vec<_>>();
        let mut admit = || Ok(());
        match &self.authority {
            SaveAuthority::Renderer(_) => self.effects.with_same_native_origin_effect(
                &self.destination,
                &actual,
                &[],
                &self.cancel,
                &self.pending,
                &refs,
                &mut admit,
            ),
            SaveAuthority::Native(binding) => {
                self.effects.with_existing_document_same_origin_effect(
                    binding,
                    &actual,
                    &[],
                    &self.cancel,
                    &self.pending,
                    &refs,
                    &mut admit,
                )
            }
        }?;
        let mut captured = self.clone();
        captured.callback_owner = Some(actual);
        captured.additional = additional;
        Ok(captured)
    }

    pub(super) fn effect<T>(
        &self,
        action: impl FnOnce() -> Result<T, String>,
    ) -> Result<T, String> {
        let mut action = Some(action);
        let mut output = None;
        let mut effect = || {
            self.check()?;
            let run = action
                .take()
                .ok_or("The save effect was already consumed")?;
            output = Some(run()?);
            Ok(())
        };
        let participants = self.participants();
        let additional = self.additional.iter().collect::<Vec<_>>();
        match &self.authority {
            SaveAuthority::Renderer(origin) => self.effects.with_owned_effect_many_extra(
                origin,
                &participants,
                &self.cancel,
                &self.pending,
                &additional,
                &mut effect,
            ),
            SaveAuthority::Native(binding) => {
                self.effects.with_existing_document_save_effect_extra(
                    binding,
                    &participants,
                    &self.cancel,
                    &self.pending,
                    &additional,
                    &mut effect,
                )
            }
        }?;
        output.ok_or_else(|| "The native save effect did not execute".into())
    }

    pub(super) fn write(&self, path: &str, bytes: &[u8]) -> Result<(), String> {
        main_core::packages::electron_utils::atomic_write::atomic_write_file_authorized(path, bytes, &mut |action| {
            match &self.authority {
                SaveAuthority::Renderer(_) => self.effect(|| { action(); Ok(()) }),
                SaveAuthority::Native(binding) => {
                    match self.native_destination.as_ref() {
                        Some(destination) if destination.matches_binding(binding) => self.effects.with_existing_document_destination_effect(destination, Path::new(path), &self.participants(), &self.cancel, &self.pending, &self.additional.iter().collect::<Vec<_>>(), &mut || { action(); Ok(()) }),
                        _ => Err("The book save destination was released or belongs to another operation".into()),
                    }
                }
            }.map_err(|message| NodeError { code: None, message })
        }).map_err(|error| error.message)
    }
}

#[derive(Clone)]
enum RenameAuthority {
    Owned(NativeRequestOwner),
    Native(shell_contract::ipc::NativeSaveRevisionSnapshot, PathBuf),
    Recovery {
        receipt: shell_contract::ipc::RecoverySeedReceipt,
        receiver: NativeRequestOwner,
    },
}

#[derive(Clone)]
struct RenameScope {
    authority: RenameAuthority,
    destinations: Vec<NativeRequestOwner>,
    cancel: CancelToken,
    pending: CancelToken,
    additional: Vec<CancelToken>,
    effects: OwnedEffectsHost,
}

impl RenameScope {
    fn effect<T>(&self, action: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
        let mut action = Some(action);
        let mut output = None;
        let mut effect = || {
            if let RenameAuthority::Native(snapshot, expected) = &self.authority {
                if snapshot.selection().path() != expected {
                    return Err("The native save destination changed".into());
                }
            }
            let run = action
                .take()
                .ok_or("The rename effect was already consumed")?;
            output = Some(run()?);
            Ok(())
        };
        match &self.authority {
            RenameAuthority::Owned(origin) => self.effects.with_owned_effect_many_extra(
                origin,
                &self.destinations,
                &self.cancel,
                &self.pending,
                &self.additional.iter().collect::<Vec<_>>(),
                &mut effect,
            ),
            RenameAuthority::Native(snapshot, _) => {
                self.effects.with_native_save_revision_snapshot_effect(
                    snapshot,
                    &self.destinations,
                    &self.cancel,
                    &self.pending,
                    &self.additional.iter().collect::<Vec<_>>(),
                    &mut effect,
                )
            }
            RenameAuthority::Recovery { receipt, receiver } => {
                self.effects.with_recovery_open_effect_many(
                    receipt,
                    receiver,
                    &self.destinations,
                    &self.cancel,
                    &self.pending,
                    &mut effect,
                )
            }
        }?;
        output.ok_or_else(|| "The native rename effect did not run".into())
    }

    fn check(&self) -> Result<(), String> {
        self.effect(|| Ok(()))
    }

    fn emit(&self, channel: &str, args: Vec<IpcValue>) -> Result<(), String> {
        match &self.authority {
            RenameAuthority::Owned(origin) => self.effects.emit_owned_many_extra(
                origin,
                &self.destinations,
                0,
                &self.cancel,
                &self.pending,
                &self.additional.iter().collect::<Vec<_>>(),
                channel,
                args,
            ),
            RenameAuthority::Native(snapshot, expected)
                if snapshot.selection().path() == expected =>
            {
                self.effects.emit_native_save_revision_snapshot(
                    snapshot,
                    &self.destinations,
                    0,
                    &self.cancel,
                    &self.pending,
                    &self.additional.iter().collect::<Vec<_>>(),
                    channel,
                    args,
                )
            }
            RenameAuthority::Native(_, _) => Err("The native save destination changed".into()),
            RenameAuthority::Recovery { receipt, receiver } => {
                self.effects.emit_recovery_open_many(
                    receipt,
                    receiver,
                    &self.destinations,
                    0,
                    &self.cancel,
                    &self.pending,
                    channel,
                    args,
                )
            }
        }
    }
}

pub(super) struct Pending {
    scope: Scope,
    then: Then<bool>,
    _subscriptions: Vec<CancelSubscription>,
    saving: bool,
    owns_pending: bool,
}

impl Pending {
    pub(super) fn cancel_and_finish(self) {
        if self.owns_pending {
            self.scope.pending.cancel();
        }
        (self.then)(Err(kit::error("The book view was closed")));
    }
}

impl Epub {
    pub(super) fn apply_owned_edits(
        self: &Rc<Self>,
        cx: &CallContext,
        args: Args,
        reply: Responder,
    ) {
        let request = field(args.get(0), "requestId");
        if request == &IpcValue::Undefined {
            return self.apply_edits(cx.sender.view, args, reply, None);
        }
        let IpcValue::String(id) = request else {
            return reply.reply(Err(kit::error("Invalid book save request identity")));
        };
        let scope = self
            .view(cx.sender.view, |entry| {
                entry
                    .owned_saves
                    .get(id)
                    .filter(|pending| !pending.saving)
                    .map(|pending| pending.scope.clone())
            })
            .flatten();
        let Some(scope) = scope else {
            return reply.reply(Err(kit::error("Unknown book save request")));
        };
        let scope = match scope.capture_callback(cx) {
            Ok(scope) => scope,
            Err(error) => return reply.reply(Err(kit::error(error))),
        };
        self.apply_edits(cx.sender.view, args, reply, Some(scope));
    }

    pub(super) fn replace_save_document(&self, view: ViewId) {
        let previous = self
            .with_view(view, |entry| {
                entry.document.replace(RendererGeneration::new())
            })
            .flatten();
        if let Some(previous) = previous {
            previous.revoke();
        }
    }

    fn settle_owned(&self, view: ViewId, id: &str, result: Result<bool, IpcError>) {
        let pending = self
            .with_view(view, |entry| entry.owned_saves.remove(id))
            .flatten();
        if let Some(pending) = pending {
            if pending.owns_pending {
                pending.scope.pending.cancel();
            }
            (pending.then)(result);
        }
    }

    pub(super) fn begin_owned(
        self: &Rc<Self>,
        view: ViewId,
        mode: SaveMode,
        path: Option<PathBuf>,
        origin: NativeRequestOwner,
        cancel: CancelToken,
        then: Then<bool>,
    ) {
        self.begin_owned_in(view, mode, path, Some(origin), cancel, None, then);
    }

    pub(super) fn begin_native(
        self: &Rc<Self>,
        view: ViewId,
        mode: SaveMode,
        path: Option<PathBuf>,
        binding: shell_contract::ipc::NativeExistingDocumentBinding,
        cancel: CancelToken,
        pending: CancelToken,
        then: Then<bool>,
    ) {
        use shell_contract::ipc::{
            NativeEditorCommand, NativeInvocationSelection, NativeSaveSelection,
        };
        let valid = match (binding.operation().selection(), &path) {
            (
                NativeInvocationSelection::NativeCommand(NativeEditorCommand::SaveDocument {
                    surface: Surface::Epub,
                    view: selected,
                    selection: NativeSaveSelection::Mode(selected_mode),
                }),
                None,
            ) => *selected == view && *selected_mode == mode,
            (
                NativeInvocationSelection::NativeCommand(NativeEditorCommand::SaveDocument {
                    surface: Surface::Epub,
                    view: selected,
                    selection: NativeSaveSelection::ExactPath(selected_path),
                }),
                Some(path),
            ) => *selected == view && selected_path == path,
            _ => false,
        };
        if !valid || binding.owner().view != view {
            return then(Err(kit::error(
                "The native save selection does not match the book",
            )));
        }
        let document = match self.view(view, |entry| entry.document.clone()).flatten() {
            Some(value) => value,
            None => return then(Err(kit::error("The book document is unavailable"))),
        };
        if !binding
            .owner()
            .context
            .document_lease()
            .is_some_and(|lease| lease.same_generation(&document))
        {
            return then(Err(kit::error("The book document was replaced")));
        }
        let effects = match self.services.hosts.need::<OwnedEffectsHost>(Batch::D2) {
            Ok(value) => value,
            Err(_) => return then(Err(kit::error("Native save effects are unavailable"))),
        };
        let scope = Scope {
            authority: SaveAuthority::Native(binding.clone()),
            destination: binding.owner().clone(),
            cancel: cancel.clone(),
            pending,
            document,
            effects,
            target: String::new(),
            native_destination: None,
            additional: Vec::new(),
            callback_owner: None,
        };
        self.begin_owned_in(view, mode, path, None, cancel, Some(scope), then);
    }

    fn begin_owned_in(
        self: &Rc<Self>,
        view: ViewId,
        mode: SaveMode,
        path: Option<PathBuf>,
        origin: Option<NativeRequestOwner>,
        cancel: CancelToken,
        captured: Option<Scope>,
        then: Then<bool>,
    ) {
        let owns_pending = captured.is_none();
        let result = (|| -> Result<_, IpcError> {
            if !self.alive(view) || !self.may_write(view) {
                return Err(kit::error(
                    "The file did not open, so there is no book to save",
                ));
            }
            let current = self.view(view, |entry| entry.save_path.clone()).flatten();
            let document = self
                .view(view, |entry| entry.document.clone())
                .flatten()
                .ok_or_else(|| kit::error("The book document is unavailable"))?;
            let explicit = match path {
                Some(path) if path.is_absolute() => Some(
                    path.to_str()
                        .ok_or_else(|| kit::error("The save path is not valid Unicode"))?
                        .to_owned(),
                ),
                Some(_) => return Err(kit::error("The save path must be absolute")),
                None => None,
            };
            let target =
                explicit.or_else(|| (mode == SaveMode::Save).then(|| current.clone()).flatten());
            let mut scope = match captured {
                Some(scope) => {
                    if !scope.document.same_generation(&document) {
                        return Err(kit::error("The book document was replaced"));
                    }
                    scope
                }
                None => {
                    let effects = self
                        .services
                        .hosts
                        .need::<OwnedEffectsHost>(Batch::D2)
                        .map_err(|_| kit::error("Native save effects are unavailable"))?;
                    let generation = self
                        .services
                        .views
                        .renderer_generation(view)
                        .ok_or_else(|| kit::error("The book renderer is unavailable"))?;
                    let mut destination = effects
                        .capture_owner(view, &generation)
                        .map_err(kit::error)?;
                    if destination
                        .context
                        .document_lease()
                        .is_some_and(|expected| !expected.same_generation(&document))
                    {
                        return Err(kit::error("The book document was replaced"));
                    }
                    destination.context = destination.context.with_document_lease(document.clone());
                    Scope {
                        authority: SaveAuthority::Renderer(
                            origin.ok_or_else(|| {
                                kit::error("The renderer save owner is unavailable")
                            })?,
                        ),
                        destination,
                        cancel,
                        pending: CancelToken::default(),
                        document,
                        effects,
                        target: String::new(),
                        native_destination: None,
                        additional: Vec::new(),
                        callback_owner: None,
                    }
                }
            };
            scope.target = target.clone().unwrap_or_default();
            if let (SaveAuthority::Native(binding), Some(path)) =
                (&scope.authority, target.as_ref())
            {
                scope.native_destination = Some(
                    scope
                        .effect(|| {
                            use shell_contract::ipc::{
                                NativeEditorCommand, NativeInvocationSelection,
                                NativeSaveDestination, NativeSaveSelection,
                            };
                            match binding.operation().selection() {
                                NativeInvocationSelection::NativeCommand(
                                    NativeEditorCommand::SaveDocument {
                                        selection: NativeSaveSelection::ExactPath(_),
                                        ..
                                    },
                                ) => NativeSaveDestination::from_exact_selection(binding.clone()),
                                NativeInvocationSelection::NativeCommand(
                                    NativeEditorCommand::SaveDocument {
                                        selection: NativeSaveSelection::Mode(SaveMode::Save),
                                        ..
                                    },
                                ) => {
                                    if !self
                                        .view(view, |entry| {
                                            entry.save_path.as_deref() == Some(path.as_str())
                                        })
                                        .unwrap_or(false)
                                    {
                                        return Err("The book save path was replaced".into());
                                    }
                                    NativeSaveDestination::from_current_document(
                                        binding.clone(),
                                        PathBuf::from(path),
                                    )
                                }
                                _ => Err("The native book save has no selected destination".into()),
                            }
                        })
                        .map_err(kit::error)?,
                );
            }
            scope.check().map_err(kit::error)?;
            Ok((scope, target, current))
        })();
        let (scope, target, current) = match result {
            Ok(value) => value,
            Err(error) => return then(Err(error)),
        };
        if mode == SaveMode::Save
            && !self.is_dirty(view)
            && current.as_ref() == target.as_ref()
            && target.is_some()
        {
            return then(scope.effect(|| Ok(true)).map_err(kit::error));
        }
        let id = self.shell.random_token();
        if self
            .view(view, |entry| entry.owned_saves.contains_key(&id))
            .unwrap_or(false)
        {
            return then(Err(kit::error("Duplicate book save request")));
        }
        let mut subscriptions = Vec::new();
        let mut losses = vec![scope.cancel.clone(), scope.document.loss_token()];
        if let SaveAuthority::Renderer(origin) = &scope.authority {
            if let Some(loss) = origin.loss_token() {
                losses.push(loss);
            }
        }
        if let SaveAuthority::Native(binding) = &scope.authority {
            losses.push(binding.operation().loss_token());
        }
        if let Some(loss) = scope.destination.loss_token() {
            losses.push(loss);
        }
        for loss in losses {
            let pending = scope.pending.clone();
            let weak = Rc::downgrade(self);
            let request = id.clone();
            let notify = kit::back_on_core(&self.services, move |()| {
                if let Some(epub) = weak.upgrade() {
                    epub.settle_owned(
                        view,
                        &request,
                        Err(kit::error("The book save owner was released")),
                    );
                }
            });
            subscriptions.push(loss.on_cancel(move || {
                if owns_pending {
                    pending.cancel();
                }
                notify(());
            }));
        }
        self.with_view(view, |entry| {
            entry.owned_saves.insert(
                id.clone(),
                Pending {
                    scope: scope.clone(),
                    then,
                    _subscriptions: subscriptions,
                    saving: false,
                    owns_pending,
                },
            );
        });
        let weak = Rc::downgrade(self);
        let request = id.clone();
        kit::after(&self.services, SAVE_WAIT, move || {
            if let Some(epub) = weak.upgrade() {
                epub.settle_owned(view, &request, Err(kit::error("Timed out saving the book")));
            }
        });
        if target.is_some() {
            self.emit_owned(view, &id, mode);
            return;
        }
        let default_path = current.unwrap_or_else(|| {
            let documents = documents_dir_or_fallback(&AppPathProvider(&*self.services.paths))
                .unwrap_or_default();
            join(&[&documents, &format!("book{BOOK_EXTENSION}")])
        });
        let options = js_object(vec![
            (
                "title",
                JsonValue::String(tm(self.lang(), "dlgSaveTitle").into()),
            ),
            ("defaultPath", JsonValue::String(default_path)),
            ("filters", self.epub_filters(self.lang())),
        ]);
        let options = main_core::packages::electron_utils::dialog_memory::prepare_dialog_options(
            &self
                .dialog_memory
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
            &options,
            None,
        );
        let options = file_dialog_of(&options);
        let weak = Rc::downgrade(self);
        let selected_scope = scope.clone();
        let deliver = kit::back_on_core(
            &self.services,
            move |result: Result<Option<BookDestination>, String>| {
                let Some(epub) = weak.upgrade() else {
                    return;
                };
                let selected = match result {
                    Ok(Some(selected)) => selected,
                    Ok(None) => return epub.settle_owned(view, &id, Ok(false)),
                    Err(error) => return epub.settle_owned(view, &id, Err(kit::error(error))),
                };
                let raw = match &selected {
                    BookDestination::Renderer(path) => path.as_path(),
                    BookDestination::Native(destination) => destination.path(),
                };
                let path = match raw.to_str() {
                    Some(path) => path.to_owned(),
                    None => {
                        return epub.settle_owned(view, &id, Err(kit::error("Invalid save path")));
                    }
                };
                let target = with_book_extension(&path);
                let destination = match selected {
                    BookDestination::Renderer(_) => None,
                    BookDestination::Native(destination) => match destination
                        .normalize_dialog_format(shell_contract::ipc::NativeSaveFormat::Epub)
                    {
                        Ok(destination) if destination.path() == Path::new(&target) => {
                            Some(destination)
                        }
                        Ok(_) => {
                            return epub.settle_owned(
                                view,
                                &id,
                                Err(kit::error("The selected book path changed")),
                            );
                        }
                        Err(error) => return epub.settle_owned(view, &id, Err(kit::error(error))),
                    },
                };
                let result = selected_scope.effect(|| {
                    main_core::packages::electron_utils::dialog_memory::remember_save_path(
                        &mut epub
                            .dialog_memory
                            .lock()
                            .unwrap_or_else(|error| error.into_inner()),
                        &path,
                    );
                    epub.with_view(view, |entry| {
                        if let Some(pending) = entry.owned_saves.get_mut(&id) {
                            pending.scope.target = target;
                            pending.scope.native_destination = destination;
                        }
                    });
                    Ok(())
                });
                if let Err(error) = result {
                    return epub.settle_owned(view, &id, Err(kit::error(error)));
                }
                epub.emit_owned(view, &id, mode);
            },
        );
        match &scope.authority {
            SaveAuthority::Renderer(origin) => self.dialogs.save_owned_many_extra(
                self.parent(view),
                options,
                origin.clone(),
                scope.participants(),
                scope.cancel.clone(),
                scope.pending.clone(),
                scope.additional.clone(),
                Box::new(move |result| {
                    deliver(result.map(|value| value.map(BookDestination::Renderer)))
                }),
            ),
            SaveAuthority::Native(binding) => self.dialogs.save_existing_document(
                self.parent(view),
                options,
                binding.clone(),
                scope.participants(),
                scope.cancel.clone(),
                scope.pending.clone(),
                scope.additional.clone(),
                Box::new(move |result| {
                    deliver(result.map(|value| value.map(BookDestination::Native)))
                }),
            ),
        }
    }

    fn emit_owned(&self, view: ViewId, id: &str, mode: SaveMode) {
        let scope = self
            .view(view, |entry| {
                entry
                    .owned_saves
                    .get(id)
                    .map(|pending| pending.scope.clone())
            })
            .flatten();
        let Some(scope) = scope else {
            return;
        };
        let mode = if mode == SaveMode::SaveAs {
            "saveAs"
        } else {
            "save"
        };
        let participants = scope.participants();
        let additional = scope.additional.iter().collect::<Vec<_>>();
        let args = vec![mode.into_ipc(), id.into_ipc()];
        let result = scope.check().and_then(|_| match &scope.authority {
            SaveAuthority::Renderer(origin) => scope.effects.emit_owned_many_extra(
                origin,
                &participants,
                0,
                &scope.cancel,
                &scope.pending,
                &additional,
                "epub:save-request",
                args,
            ),
            SaveAuthority::Native(binding) => scope.effects.emit_existing_document_save_many_extra(
                binding,
                &participants,
                0,
                &scope.cancel,
                &scope.pending,
                &additional,
                "epub:save-request",
                args,
            ),
        });
        if let Err(error) = result {
            self.settle_owned(view, id, Err(kit::error(error)));
        }
    }
    pub(super) fn owned_ack(&self, cx: &CallContext, id: &str, ok: bool) -> Result<(), IpcError> {
        let scope = self
            .view(cx.sender.view, |entry| {
                entry
                    .owned_saves
                    .get(id)
                    .map(|pending| pending.scope.clone())
            })
            .flatten()
            .ok_or_else(|| kit::error("Unknown book save request"))?;
        let scope = scope.capture_callback(cx).map_err(kit::error)?;
        let pending = scope
            .effect(|| {
                if ok {
                    return Ok(None);
                }
                Ok(self
                    .with_view(cx.sender.view, |entry| entry.owned_saves.remove(id))
                    .flatten())
            })
            .map_err(kit::error)?;
        if let Some(pending) = pending {
            if pending.owns_pending {
                pending.scope.pending.cancel();
            }
            (pending.then)(Ok(false));
        }
        Ok(())
    }

    pub(super) fn take_owned(&self, cx: &CallContext, id: &str) -> Result<Scope, IpcError> {
        let scope = self
            .view(cx.sender.view, |entry| {
                entry
                    .owned_saves
                    .get(id)
                    .map(|pending| pending.scope.clone())
            })
            .flatten()
            .ok_or_else(|| kit::error("Unknown book save request"))?;
        let scope = scope.capture_callback(cx).map_err(kit::error)?;
        scope
            .effect(|| {
                self.with_view(cx.sender.view, |entry| {
                    let pending = entry
                        .owned_saves
                        .get_mut(id)
                        .ok_or("Unknown book save request")?;
                    if pending.saving {
                        return Err("The book save was already consumed".into());
                    }
                    pending.saving = true;
                    pending.scope = scope.clone();
                    Ok(scope.clone())
                })
                .unwrap_or_else(|| Err("The book is no longer open".into()))
            })
            .map_err(kit::error)
    }

    pub(super) fn save_owned_dispatch(
        self: &Rc<Self>,
        cx: &CallContext,
        args: Args,
        reply: Responder,
    ) {
        let IpcValue::String(id) = field(args.get(0), "requestId") else {
            if field(args.get(0), "requestId") != &IpcValue::Undefined {
                return reply.reply(Err(kit::error("Invalid book save request identity")));
            }
            return self.save(cx.sender.view, args, reply);
        };
        let id = id.clone();
        let view = cx.sender.view;
        let scope = match self.take_owned(cx, &id) {
            Ok(scope) => scope,
            Err(error) => return reply.reply(Err(error)),
        };
        let request = args.get(0).clone();
        let epub = self.clone();
        let done = self.clone();
        let job: Box<dyn FnOnce()> = Box::new(move || {
            epub.save_owned_view(
                view,
                &request,
                scope,
                Box::new(move |result| {
                    let ok = field(&result, "ok") == &IpcValue::Bool(true)
                        && field(&result, "canceled") == &IpcValue::Undefined;
                    reply.reply(Ok(result));
                    done.settle_owned(view, &id, Ok(ok));
                    done.save_finished(view);
                }),
            );
        });
        let run = self
            .with_view(view, |entry| {
                if entry.saving {
                    entry.queued_saves.push_back(job);
                    None
                } else {
                    entry.saving = true;
                    Some(job)
                }
            })
            .flatten();
        if let Some(run) = run {
            run();
        }
    }

    fn save_owned_view(
        self: &Rc<Self>,
        view: ViewId,
        request: &IpcValue,
        scope: Scope,
        done: Box<dyn FnOnce(IpcValue)>,
    ) {
        if let Err(error) = scope.check() {
            return done(save_failure(error));
        }
        let Some(session) = self.session(view).filter(|_| self.may_write(view)) else {
            return done(save_failure(
                "epub: the file did not open, nothing is written",
            ));
        };
        let current = self.view(view, |entry| entry.save_path.clone()).flatten();
        let target = scope.target.clone();
        let same_file = current
            .as_deref()
            .is_some_and(|current| same_path(current, &target, None));
        let auto = field(request, "auto") == &IpcValue::Bool(true);
        let elsewhere = same_file
            && field(request, "mode") != &IpcValue::String("saveAs".into())
            && current
                .as_deref()
                .is_some_and(|path| self.shell.open_elsewhere(path));
        let recorded = current.as_ref().and_then(|path| {
            self.view(view, |entry| entry.disk_states.get(path).cloned())
                .flatten()
        });
        let (edits, snapshot, bytes, old_hash) = {
            let session = session.borrow();
            (
                session.edits_for_save(),
                session.snapshot(),
                session.bytes.clone(),
                session.doc_hash.clone(),
            )
        };
        let labels =
            main_core::packages::electron_utils::save_conflict_labels::save_conflict_labels(
                self.lang().as_str(),
            );
        let options = MessageBox {
            kind: Some("warning".into()),
            message: labels.message.into(),
            detail: Some(labels.detail.into()),
            buttons: vec![labels.overwrite.into(), labels.cancel.into()],
            default_id: Some(0),
            cancel_id: Some(1),
            ..Default::default()
        };
        let parent = self.parent(view);
        let dialogs = self.dialogs.clone();
        let engine = self.engine.clone();
        let shell = self.shell.clone();
        let dir = self.user_data_dir();
        let write_to = target.clone();
        let work_scope = scope.clone();
        let check_path = current.clone();
        let epub = self.clone();
        kit::on_worker(
            &self.services,
            move || {
                if let Err(error) = work_scope.check() {
                    return SaveRun::Failed(error);
                }
                let conflict = elsewhere
                    && check_path.as_ref().is_some_and(|path| {
                        main_core::packages::electron_utils::save_conflict::is_modified_since(
                            recorded.as_ref(),
                            path,
                        )
                    });
                if conflict {
                    if auto {
                        return SaveRun::Failed("external-modified".into());
                    }
                    let (tx, rx) = std::sync::mpsc::sync_channel(1);
                    let done = Box::new(move |answer| {
                        let _ = tx.send(answer);
                    });
                    match &work_scope.authority {
                        SaveAuthority::Renderer(origin) => dialogs.message_owned_many_extra(
                            parent,
                            options,
                            origin.clone(),
                            work_scope.participants(),
                            work_scope.cancel.clone(),
                            work_scope.pending.clone(),
                            work_scope.additional.clone(),
                            done,
                        ),
                        SaveAuthority::Native(binding) => dialogs.message_existing_document(
                            parent,
                            options,
                            binding.clone(),
                            work_scope.participants(),
                            work_scope.cancel.clone(),
                            work_scope.pending.clone(),
                            work_scope.additional.clone(),
                            done,
                        ),
                    }
                    match rx.recv() {
                        Ok(Ok(answer)) if answer.response == 0 => {}
                        Ok(Ok(_)) => return SaveRun::Failed("external-modified".into()),
                        Ok(Err(error)) => return SaveRun::Failed(error),
                        Err(_) => {
                            return SaveRun::Failed("The save dialog did not complete".into());
                        }
                    }
                }
                save_book_admitted(
                    &*engine,
                    &*shell,
                    &dir,
                    bytes,
                    &edits,
                    same_file,
                    &write_to,
                    &old_hash,
                    Some(&work_scope),
                )
            },
            move |run| {
                let result = scope.effect(|| {
                    let disk = match run {
                        SaveRun::Failed(error) => return Err(error),
                        SaveRun::NotWritten => None,
                        SaveRun::Copied(disk) => Some(disk),
                        SaveRun::Rebased {
                            bytes,
                            opened,
                            disk,
                        } => {
                            session.borrow_mut().rebase(bytes, opened, &snapshot);
                            Some(disk)
                        }
                    };
                    let doc_hash = session.borrow().doc_hash.clone();
                    epub.know(view, &doc_hash);
                    epub.with_view(view, |entry| {
                        entry.renderer_dirty = false;
                        if let Some(disk) = disk.as_ref() {
                            entry.save_path = Some(target.clone());
                            entry.open_path = Some(target.clone());
                            entry.allowed.insert(target.clone());
                            if let Some(disk) = disk {
                                entry.disk_states.insert(target.clone(), disk.clone());
                            }
                            if let Some(current) = current.as_ref().filter(|_| !same_file) {
                                entry.disk_states.remove(current);
                            }
                        }
                    });
                    Ok((disk.is_some(), doc_hash))
                });
                let (written, doc_hash) = match result {
                    Ok(value) => value,
                    Err(error) => return done(save_failure(error)),
                };
                if written && !same_file {
                    let completion_scope = scope.clone();
                    let completion_epub = epub.clone();
                    let published_target = target.clone();
                    let finish = Box::new(
                        move |result: Result<shell_contract::hosts::SavedFile<String>, String>| {
                            let result = result.and_then(|saved| {
                                let path = saved.path;
                                let answer = completion_scope.effect(|| {
                                    completion_epub.with_view(view, |entry| {
                                        entry.save_path = Some(path.clone());
                                        entry.open_path = Some(path.clone());
                                        entry.allowed.insert(path.clone());
                                        if path != published_target {
                                            if let Some(disk) =
                                                entry.disk_states.remove(&published_target)
                                            {
                                                entry.disk_states.insert(path.clone(), disk);
                                            }
                                        }
                                    });
                                    Ok(object(vec![
                                        ("ok", true.into_ipc()),
                                        ("path", path.into_ipc()),
                                        ("written", written.into_ipc()),
                                        ("docHash", doc_hash.into_ipc()),
                                    ]))
                                })?;
                                Ok((answer, saved.session))
                            });
                            match result {
                                Err(error) => done(save_failure(error)),
                                Ok((answer, session)) => {
                                    session.finalize(Box::new(move |result| {
                                        let result = result
                                            .and_then(|()| completion_scope.effect(|| Ok(answer)));
                                        done(match result {
                                            Ok(answer) => answer,
                                            Err(error) => save_failure(error),
                                        });
                                    }))
                                }
                            }
                        },
                    );
                    match (&scope.authority, scope.native_destination.clone()) {
                        (SaveAuthority::Renderer(origin), None) => epub.shell.file_saved_owned(
                            origin,
                            &scope.destination,
                            &scope.participants(),
                            &scope.cancel,
                            &scope.pending,
                            &scope.additional,
                            &scope.effects,
                            &target,
                            finish,
                        ),
                        (SaveAuthority::Native(_), Some(destination)) => {
                            epub.shell.file_saved_existing_document(
                                destination,
                                scope.participants(),
                                scope.cancel.clone(),
                                scope.pending.clone(),
                                scope.additional.clone(),
                                scope.effects.clone(),
                                finish,
                            )
                        }
                        _ => finish(Err(
                            "The book save destination does not match its authority".into(),
                        )),
                    }
                } else {
                    let answer = object(vec![
                        ("ok", true.into_ipc()),
                        ("path", target.into_ipc()),
                        ("written", written.into_ipc()),
                        ("docHash", doc_hash.into_ipc()),
                    ]);
                    done(match scope.check() {
                        Ok(()) => answer,
                        Err(error) => save_failure(error),
                    });
                }
            },
        );
    }

    pub(super) fn rename_owned_book(
        self: &Rc<Self>,
        view: ViewId,
        old_path: &Path,
        new_path: &Path,
        origin: &NativeRequestOwner,
        destination: &NativeRequestOwner,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        additional: &[CancelToken],
        then: Then<()>,
    ) {
        self.rename_book(
            view,
            old_path,
            new_path,
            RenameAuthority::Owned(origin.clone()),
            destination,
            participants,
            cancel,
            pending,
            additional,
            then,
        );
    }

    pub(super) fn rename_native_book(
        self: &Rc<Self>,
        view: ViewId,
        old_path: &Path,
        new_path: &Path,
        snapshot: &shell_contract::ipc::NativeSaveRevisionSnapshot,
        expected_path: &Path,
        destination: &NativeRequestOwner,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        additional: &[CancelToken],
        then: Then<()>,
    ) {
        self.rename_book(
            view,
            old_path,
            new_path,
            RenameAuthority::Native(snapshot.clone(), expected_path.to_path_buf()),
            destination,
            participants,
            cancel,
            pending,
            additional,
            then,
        );
    }

    pub(super) fn rename_recovery_book(
        self: &Rc<Self>,
        view: ViewId,
        old_path: &Path,
        new_path: &Path,
        receipt: &shell_contract::ipc::RecoverySeedReceipt,
        receiver: &NativeRequestOwner,
        destination: &NativeRequestOwner,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        then: Then<()>,
    ) {
        self.rename_book(
            view,
            old_path,
            new_path,
            RenameAuthority::Recovery {
                receipt: receipt.clone(),
                receiver: receiver.clone(),
            },
            destination,
            participants,
            cancel,
            pending,
            &[],
            then,
        );
    }

    fn rename_book(
        self: &Rc<Self>,
        view: ViewId,
        old_path: &Path,
        new_path: &Path,
        authority: RenameAuthority,
        destination: &NativeRequestOwner,
        participants: &[NativeRequestOwner],
        cancel: &CancelToken,
        pending: &CancelToken,
        additional: &[CancelToken],
        then: Then<()>,
    ) {
        let prepared = (|| -> Result<_, String> {
            let old = old_path
                .to_str()
                .ok_or("The old path is not valid Unicode")?;
            let new = new_path
                .to_str()
                .ok_or("The new path is not valid Unicode")?;
            if destination.view != view {
                return Err("The rename belongs to another view".into());
            }
            let document = self
                .view(view, |entry| entry.document.clone())
                .flatten()
                .ok_or("The book document is unavailable")?;
            if !destination
                .context
                .document_lease()
                .is_some_and(|expected| expected.same_generation(&document))
            {
                return Err("The book document was replaced".into());
            }
            let effects = self
                .services
                .hosts
                .need::<OwnedEffectsHost>(Batch::D2)
                .map_err(|_| "Native save effects are unavailable")?;
            let mut destinations = Vec::with_capacity(participants.len() + 1);
            destinations.push(destination.clone());
            destinations.extend_from_slice(participants);
            let scope = RenameScope {
                authority,
                destinations,
                cancel: cancel.clone(),
                pending: pending.clone(),
                additional: additional.to_vec(),
                effects,
            };
            scope.effect(|| {
                self.with_view(view, |entry| {
                    if entry.save_path.as_deref() == Some(old) {
                        entry.save_path = Some(new.to_owned());
                    }
                    if entry.open_path.as_deref() == Some(old) {
                        entry.open_path = Some(new.to_owned());
                    }
                    if entry.allowed.contains(old) {
                        entry.allowed.insert(new.to_owned());
                    }
                    if let Some(disk) = entry.disk_states.remove(old) {
                        entry.disk_states.insert(new.to_owned(), disk);
                    }
                });
                Ok(())
            })?;
            let reading_revision = scope.effect(|| Ok(self.begin_reading_change()))?;
            let reading = self.reading(|reading| {
                reading.prepare_move_position_guarded(old, new, &mut |effect| {
                    scope
                        .effect(|| {
                            effect();
                            Ok(())
                        })
                        .map_err(|message| NodeError {
                            code: None,
                            message,
                        })
                })
            });
            scope.check()?;
            Ok((scope, reading.ok(), reading_revision, new.to_owned()))
        })();
        let (scope, reading, reading_revision, new) = match prepared {
            Ok(value) => value,
            Err(error) => return then(Err(kit::error(error))),
        };
        let work_scope = scope.clone();
        kit::on_worker(
            &self.services,
            move || {
                if let Some((path, text)) = reading {
                    let _ = main_core::packages::electron_utils::atomic_write::atomic_write_file_authorized(&path, text.as_bytes(), &mut |effect| {
                    reading_revision.with_active(|| work_scope.effect(|| { effect(); Ok(()) }))
                        .ok_or_else(|| NodeError { code: None, message: "The reading state changed".into() })?
                        .map_err(|message| NodeError { code: None, message })
                });
                }
                work_scope.check()
            },
            move |result| {
                let result =
                    result.and_then(|()| scope.emit("epub:file-renamed", vec![new.into_ipc()]));
                then(result.map_err(kit::error));
            },
        );
    }
}
