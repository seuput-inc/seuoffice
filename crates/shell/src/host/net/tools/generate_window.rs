//! Generate's created window and first authenticated document are separate lifetimes.
//! Native callbacks enqueue core work only after matching that retained document.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use crate::native::{NativeEventKind, ViewEvent, WindowExt};
use main_core::shell::generate::generate_window::GenerateWindowOptions;
use main_core::shell::title_bar_overlay::TitleBarOverlay;
use shell_contract::ipc::{NativeContext, NativeRequestOwner};
use shell_contract::runtime::{CancelToken, RendererGeneration, Views};
use shell_contract::{Surface, ViewId, WindowId};
use shell_ipc_a::generate::host::{GenerateAuthority, GenerateOrigin, GenerateWindowEvent};
use shell_ipc_a::generate::{BoundGenerateWindowEvents, GenerateWindowEvents};
use tauri::Manager;

use super::generate_caption;
use crate::host::window::{Bridge, ViewSpec, WindowOptions, WindowSpec};
use crate::host::{HostContext, on_ui};
use crate::ipc::native_owner::{self, NativeOwnerAuthority};

const LOST: &str = "The generating document is no longer available.";

struct Created {
    window: WindowId,
    view: ViewId,
    generation: RendererGeneration,
    lifecycle: Arc<crate::views::NativeWindowLifecycle>,
    native: tauri::Window,
    handle: crate::native::WindowHandle,
}

struct Identity {
    created: OnceLock<Created>,
    closed: AtomicBool,
    ready: AtomicBool,
    watch_theme: AtomicBool,
    pending: CancelToken,
}

/// Private native-constructor proof, including the pre-registration close case.
struct WindowProof {
    window: WindowId,
    identity: Arc<Identity>,
}

#[derive(Clone)]
enum Binding {
    Waiting,
    Bound {
        owner: NativeRequestOwner,
        events: BoundGenerateWindowEvents,
    },
    Revoked,
}

#[derive(Clone)]
struct Entry {
    identity: Arc<Identity>,
    binding: Arc<Mutex<Binding>>,
    first_events: Arc<OnceLock<BoundGenerateWindowEvents>>,
}

impl Entry {
    fn created(&self) -> Result<&Created, String> {
        self.identity
            .created
            .get()
            .ok_or_else(|| "The generate window is not constructed.".into())
    }

    fn bound(&self) -> Result<NativeRequestOwner, String> {
        match &*self
            .binding
            .lock()
            .unwrap_or_else(|error| error.into_inner())
        {
            Binding::Bound { owner, .. } => Ok(owner.clone()),
            Binding::Waiting => Err("The generate renderer is not authenticated yet.".into()),
            Binding::Revoked => Err(LOST.into()),
        }
    }

    fn bound_events(&self) -> Option<BoundGenerateWindowEvents> {
        match &*self
            .binding
            .lock()
            .unwrap_or_else(|error| error.into_inner())
        {
            Binding::Bound { events, .. } if !self.identity.pending.is_cancelled() => {
                Some(events.clone())
            }
            _ => None,
        }
    }

    fn revoke(&self) {
        *self
            .binding
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Binding::Revoked;
        self.identity.pending.cancel();
        if let Some(created) = self.identity.created.get() {
            created.generation.revoke();
        }
    }
}

pub(super) struct GenerateWindows {
    cx: HostContext,
    entries: Arc<Mutex<HashMap<WindowId, Entry>>>,
}

fn proof_matches(context: &NativeContext, window: WindowId, entry: &Entry) -> bool {
    context.downcast_ref::<WindowProof>().is_some_and(|proof| {
        proof.window == window && Arc::ptr_eq(&proof.identity, &entry.identity)
    })
}

fn same_origin(left: &NativeContext, right: &NativeContext) -> bool {
    match (
        left.authority_ref::<NativeOwnerAuthority>(),
        right.authority_ref::<NativeOwnerAuthority>(),
    ) {
        (Some(left), Some(right)) => left.origin == right.origin,
        _ => false,
    }
}

fn cleanup_proof(authority: &GenerateAuthority) -> Result<&WindowProof, String> {
    let GenerateOrigin::WindowEvent {
        window,
        identity,
        event: GenerateWindowEvent::Closed,
    } = &authority.origin
    else {
        return Err("This authority is not a native close notification.".into());
    };
    identity
        .downcast_ref::<WindowProof>()
        .filter(|proof| proof.window == *window && proof.identity.closed.load(Ordering::Acquire))
        .ok_or_else(|| "The native window close proof does not match.".into())
}

impl GenerateWindows {
    pub(super) fn new(cx: &HostContext) -> Self {
        Self {
            cx: cx.clone(),
            entries: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn closed_proof<'a>(
        &self,
        authority: &'a GenerateAuthority,
    ) -> Result<&'a WindowProof, String> {
        cleanup_proof(authority)
    }

    pub(super) fn check_authority(&self, authority: &GenerateAuthority) -> Result<(), String> {
        if authority.is_closed_event() {
            return self.closed_proof(authority).map(|_| ());
        }
        authority.check_cancelled()?;
        let context = authority
            .native_context()
            .ok_or("Generation requires an authenticated native origin.")?;
        let owner = native_owner::authenticated_owner(context)
            .ok_or("The generating origin is not authenticated.")?;
        match &authority.origin {
            GenerateOrigin::Renderer { sender, .. } => {
                if owner.view != sender.view
                    || self.cx.views.sender_of(sender.view) != Some(*sender)
                {
                    return Err("The generating renderer identity does not match.".into());
                }
            }
            GenerateOrigin::WindowEvent {
                window,
                identity,
                event,
            } => {
                let entry = self.entry(*window)?;
                if !proof_matches(identity, *window, &entry)
                    || !same_origin(identity, &entry.bound()?.context)
                {
                    return Err("The native generate event belongs to another document.".into());
                }
                if let GenerateWindowEvent::Ready { origin }
                | GenerateWindowEvent::OpenLink { origin } = event
                {
                    if !proof_matches(origin, *window, &entry) || !same_origin(origin, identity) {
                        return Err(
                            "The generate event origin does not match its first document.".into(),
                        );
                    }
                }
            }
            GenerateOrigin::NativeMenu { .. } => {}
        }
        native_owner::with_owner_effect(&owner, &authority.cancel, || ()).ok_or_else(|| LOST.into())
    }

    fn source(&self, authority: &GenerateAuthority) -> Result<NativeRequestOwner, String> {
        self.check_authority(authority)?;
        if authority.is_closed_event() {
            return Err("Closed-window authority is cleanup-only.".into());
        }
        authority
            .native_context()
            .and_then(native_owner::authenticated_owner)
            .ok_or_else(|| "The generating origin is not authenticated.".into())
    }

    fn entry(&self, window: WindowId) -> Result<Entry, String> {
        self.entries
            .lock()
            .map_err(|_| "The generate window registry is unavailable.")?
            .get(&window)
            .cloned()
            .ok_or_else(|| "The generate window is gone.".into())
    }

    pub(super) fn delivery_owner(
        &self,
        authority: &GenerateAuthority,
        view: ViewId,
    ) -> Result<NativeRequestOwner, String> {
        let source = self.source(authority)?;
        let entry = self
            .entries
            .lock()
            .map_err(|_| "The generate window registry is unavailable.")?
            .values()
            .find(|entry| {
                entry
                    .identity
                    .created
                    .get()
                    .is_some_and(|created| created.view == view)
            })
            .cloned()
            .ok_or("The generate window is gone.")?;
        let target = entry.bound()?;
        native_owner::with_paired_effect(
            &source,
            &target,
            &authority.cancel,
            &entry.identity.pending,
            || target.clone(),
        )
        .ok_or_else(|| LOST.into())
    }

    pub(super) fn prepare_committed_cleanup(
        &self,
        authority: &GenerateAuthority,
        window: WindowId,
    ) -> Result<shell_ipc_a::generate::host::GenerateWindowCleanup, String> {
        let source = self.source(authority)?;
        let entry = self.entry(window)?;
        let target = entry.bound()?;
        if !proof_matches(&target.context, window, &entry) {
            return Err("The original generate window identity does not match.".into());
        }
        let label = self
            .cx
            .views
            .label_of_window(window)
            .ok_or("The original generate window is gone.")?;
        let native = self
            .cx
            .app
            .get_window(&label)
            .ok_or("The original generate window is gone.")?;
        let identity = target.context.clone();
        let ui = self.cx.ui.clone();
        native_owner::with_paired_effect(
            &source,
            &target,
            &authority.cancel,
            &entry.identity.pending,
            || {
                shell_ipc_a::generate::host::GenerateWindowCleanup::new(
                    window,
                    Box::new(move |done| {
                        ui.post(Box::new(move || {
                            // This is only the retained original widget's terminal
                            // cleanup. A committed target never mints fresh authority.
                            let Some(proof) = identity.downcast_ref::<WindowProof>() else {
                                return done(Err(
                                    "The original generate cleanup proof is unavailable.".into(),
                                ));
                            };
                            if proof.identity.closed.load(Ordering::Acquire) {
                                return done(Ok(()));
                            }
                            done(native.close().map_err(|error| error.to_string()));
                        }));
                    }),
                )
            },
        )
        .ok_or_else(|| LOST.into())
    }

    fn discard_failed_window(
        views: Arc<crate::views::ViewTable>,
        lifecycle: Arc<crate::views::NativeWindowLifecycle>,
        window: WindowId,
        view: ViewId,
        generation: RendererGeneration,
    ) {
        let captured = lifecycle.clone();
        if let Err(error) = lifecycle.when_terminal(Box::new(move |_| {
            views.remove_view_if_generation(view, &generation);
            let _ = views.remove_window_captured(window, &captured);
        })) {
            eprintln!("[generate] cannot observe failed window cleanup: {error}");
        }
    }

    pub(super) fn create(
        &self,
        authority: &GenerateAuthority,
        options: &GenerateWindowOptions,
        events: GenerateWindowEvents,
    ) -> Result<WindowId, String> {
        let source = self.source(authority)?;
        let cancel = authority.cancel.clone();
        let cx = self.cx.clone();
        let options = options.clone();
        let entries = self.entries.clone();
        let reserve = native_owner::with_owner_effect(&source, &cancel, || {
            || {
                self.cx
                    .windows
                    .reserve_deferred_window(Surface::ShellGenerate)
            }
        })
        .ok_or_else(|| LOST.to_owned())?;
        let reserved = reserve()?;
        let lifecycle = reserved.window_lifecycle.clone();
        let failed_window = reserved.window;
        let failed_view = reserved.view;
        let failed_generation = reserved.created.clone();
        let permit = match lifecycle.admit_native_effect() {
            Ok(permit) => permit,
            Err(error) => {
                Self::discard_failed_window(
                    self.cx.views.clone(),
                    lifecycle,
                    failed_window,
                    failed_view,
                    failed_generation,
                );
                drop(reserved);
                return Err(error);
            }
        };
        let created_lifecycle = lifecycle.clone();
        let result = on_ui(&self.cx.ui, move || {
            native_owner::run_accepted_ui_pair(
                cx.ui.as_ref(),
                &source,
                &source,
                &cancel,
                &cancel,
                || {
                    let entry = Entry {
                        identity: Arc::new(Identity {
                            created: OnceLock::new(),
                            closed: AtomicBool::new(false),
                            ready: AtomicBool::new(false),
                            watch_theme: AtomicBool::new(false),
                            pending: CancelToken::default(),
                        }),
                        binding: Arc::new(Mutex::new(Binding::Waiting)),
                        first_events: Arc::new(OnceLock::new()),
                    };
                    let frame_entry = entry.clone();
                    let frame_views = cx.views.clone();
                    let frame_events = events.clone();
                    let focus_entry = entry.clone();
                    let open_entry = entry.clone();
                    let closed_entry = entry.clone();
                    let closed_entries = entries.clone();
                    let closed_events = events.clone();
                    let extras = WindowOptions {
                        center: options.center,
                        minimizable: options.minimizable,
                        maximizable: options.maximizable,
                        fullscreenable: options.fullscreenable,
                        on_frame: Some(Arc::new(move |view, event| {
                            let Some(created) = frame_entry.identity.created.get() else {
                                return;
                            };
                            if created.view != view {
                                return;
                            }
                            match &event.kind {
                                NativeEventKind::DocumentRegistered { context } => {
                                    let captured =
                                        frame_views.capture_native_owner(view, &created.generation);
                                    let Some(captured) =
                                        captured.filter(|captured| captured.origin == *context)
                                    else {
                                        return;
                                    };
                                    let mut binding = frame_entry
                                        .binding
                                        .lock()
                                        .unwrap_or_else(|error| error.into_inner());
                                    match &*binding {
                                        Binding::Waiting => {
                                            let proof = WindowProof {
                                                window: created.window,
                                                identity: frame_entry.identity.clone(),
                                            };
                                            let Some(owner) = native_owner::owner(
                                                view,
                                                captured.origin,
                                                created.generation.clone(),
                                                proof,
                                            ) else {
                                                return;
                                            };
                                            match frame_events.bind(
                                                created.window,
                                                owner.context.clone(),
                                                frame_entry.identity.pending.clone(),
                                            ) {
                                                Ok(events) => {
                                                    let _ = frame_entry
                                                        .first_events
                                                        .set(events.clone());
                                                    *binding = Binding::Bound { owner, events };
                                                }
                                                Err(error) => {
                                                    drop(binding);
                                                    frame_entry.revoke();
                                                    eprintln!(
                                                        "[generate] cannot bind its renderer: {error}"
                                                    );
                                                }
                                            }
                                        }
                                        Binding::Bound { owner, .. }
                                            if owner
                                                .context
                                                .authority_ref::<NativeOwnerAuthority>()
                                                .is_some_and(|proof| proof.origin == *context) => {}
                                        Binding::Bound { .. } => {
                                            drop(binding);
                                            frame_entry.revoke();
                                        }
                                        Binding::Revoked => {}
                                    }
                                }
                                NativeEventKind::DocumentRevoked { document, .. } => {
                                    let matches = frame_entry.bound().ok().is_some_and(|owner| {
                                        owner
                                            .context
                                            .authority_ref::<NativeOwnerAuthority>()
                                            .is_some_and(|proof| {
                                                proof.origin.identity() == document
                                            })
                                    });
                                    if matches {
                                        frame_entry.revoke();
                                    }
                                }
                                NativeEventKind::WebviewProcessFailed { .. } => {
                                    frame_entry.revoke()
                                }
                                NativeEventKind::DocumentReady { context } => {
                                    let binding = frame_entry
                                        .binding
                                        .lock()
                                        .unwrap_or_else(|error| error.into_inner())
                                        .clone();
                                    if let Binding::Bound { owner, events } = binding {
                                        if owner
                                            .context
                                            .authority_ref::<NativeOwnerAuthority>()
                                            .is_some_and(|proof| proof.origin == *context)
                                            && !frame_entry.identity.pending.is_cancelled()
                                            && !frame_entry
                                                .identity
                                                .ready
                                                .swap(true, Ordering::AcqRel)
                                        {
                                            events.ready_to_show(owner.context);
                                        }
                                    }
                                }
                                NativeEventKind::View(ViewEvent::Focus) => {
                                    if let Some(events) = focus_entry.bound_events() {
                                        events.focus();
                                    }
                                }
                                _ => {}
                            }
                        })),
                        on_open: Some(Arc::new(move |view, origin, url| {
                            let Some(created) = open_entry.identity.created.get() else {
                                return;
                            };
                            if created.view != view || open_entry.identity.pending.is_cancelled() {
                                return;
                            }
                            let binding = open_entry
                                .binding
                                .lock()
                                .unwrap_or_else(|error| error.into_inner())
                                .clone();
                            let Binding::Bound { owner, events } = binding else {
                                return;
                            };
                            if !owner
                                .context
                                .authority_ref::<NativeOwnerAuthority>()
                                .is_some_and(|proof| proof.origin == *origin)
                            {
                                return;
                            }
                            if let Some(target) = crate::host::window_open::external_target(
                                crate::host::window_open::OpenPolicy::Web,
                                url,
                            ) {
                                events.open_link(owner.context, target);
                            }
                        })),
                        on_closed: Some(Box::new(move |window| {
                            if closed_entry.identity.closed.swap(true, Ordering::AcqRel) {
                                return;
                            }
                            let bound = closed_entry.first_events.get().cloned();
                            closed_entry.revoke();
                            {
                                let mut entries = closed_entries
                                    .lock()
                                    .unwrap_or_else(|error| error.into_inner());
                                if entries.get(&window).is_some_and(|entry| {
                                    Arc::ptr_eq(&entry.identity, &closed_entry.identity)
                                }) {
                                    entries.remove(&window);
                                }
                            }
                            match bound {
                                Some(events) => events.closed(),
                                None => closed_events.closed(
                                    window,
                                    NativeContext::new(WindowProof {
                                        window,
                                        identity: closed_entry.identity.clone(),
                                    }),
                                ),
                            }
                        })),
                        ..WindowOptions::default()
                    };
                    let spec = WindowSpec {
                        title: options.title.clone(),
                        size: (options.width, options.height),
                        min_size: Some((options.min_width, options.min_height)),
                        visible: options.show,
                        decorations: cfg!(target_os = "macos"),
                        transparent: false,
                        resizable: true,
                        parent: options
                            .parent
                            .map(|parent| {
                                u32::try_from(parent)
                                    .map(WindowId)
                                    .map_err(|_| "invalid parent window identity".to_owned())
                            })
                            .transpose()?,
                        focus: false,
                        main: false,
                        view: ViewSpec {
                            url: "about:blank".to_owned(),
                            bridge: Bridge::Surface(Surface::ShellGenerate),
                        },
                    };
                    let (window, view) = cx
                        .windows
                        .open_reserved_deferred_window(spec, extras, reserved)?;
                    let configured = (|| {
                        let generation = cx
                            .views
                            .renderer_generation(view)
                            .ok_or("The generate renderer generation is unavailable.")?;
                        if entry.identity.closed.load(Ordering::Acquire) {
                            return Err("The generate window closed during creation.".to_owned());
                        }
                        let native = created_lifecycle.window_handle()?;
                        let handle = native
                            .native_window_handle()
                            .map_err(|error| error.to_string())?;
                        entry
                            .identity
                            .created
                            .set(Created {
                                window,
                                view,
                                generation,
                                lifecycle: created_lifecycle.clone(),
                                native: native.clone(),
                                handle: handle.clone(),
                            })
                            .map_err(|_| "The generate window identity was already assigned.")?;
                        generate_caption::configure(&native, &handle, &permit, &options)?;
                        let theme_entry = entry.clone();
                        native.on_window_event(move |event| {
                            if matches!(event, tauri::WindowEvent::ThemeChanged(_))
                                && theme_entry.identity.watch_theme.load(Ordering::Acquire)
                            {
                                if let Some(events) = theme_entry.bound_events() {
                                    events.theme_updated();
                                }
                            }
                        });
                        if entry.identity.closed.load(Ordering::Acquire) {
                            return Err("The generate window closed during creation.".to_owned());
                        }
                        entries
                            .lock()
                            .map_err(|_| "The generate window registry is unavailable.")?
                            .insert(window, entry);
                        Ok(window)
                    })();
                    if configured.is_err() {
                        if let Ok(native) = created_lifecycle.window_handle() {
                            if let Err(error) = native.close() {
                                eprintln!(
                                    "[generate] cannot close an incompletely configured window: {error}"
                                );
                            }
                        }
                    }
                    configured
                },
            )?
        });
        if result.is_err() {
            Self::discard_failed_window(
                self.cx.views.clone(),
                lifecycle,
                failed_window,
                failed_view,
                failed_generation,
            );
        }
        result
    }

    /// Source and target are admitted on the actual UI task; the immutable
    /// action runs only after every native, renderer and cancellation gate drops.
    fn with_entry<T: Send + 'static>(
        &self,
        authority: &GenerateAuthority,
        entry: Entry,
        action: impl FnOnce(HostContext, WindowId) -> Result<T, String> + Send + 'static,
    ) -> Result<T, String> {
        let source = self.source(authority)?;
        let cancel = authority.cancel.clone();
        let cx = self.cx.clone();
        on_ui(&self.cx.ui, move || {
            let created = entry.created()?;
            let window = created.window;
            let binding = entry
                .binding
                .lock()
                .map_err(|_| "The generate window binding is unavailable.")?
                .clone();
            let work_cx = cx.clone();
            let work = move || action(work_cx, window);
            match binding {
                Binding::Bound { owner, .. } => native_owner::run_accepted_ui_pair(
                    cx.ui.as_ref(),
                    &source,
                    &owner,
                    &cancel,
                    &entry.identity.pending,
                    work,
                )?,
                Binding::Waiting => native_owner::run_accepted_created_ui(
                    cx.ui.as_ref(),
                    &source,
                    &created.generation,
                    &cancel,
                    &entry.identity.pending,
                    work,
                )?,
                Binding::Revoked => Err(LOST.into()),
            }
        })
    }

    fn with_window<T: Send + 'static>(
        &self,
        authority: &GenerateAuthority,
        window: WindowId,
        action: impl FnOnce(tauri::WebviewWindow) -> Result<T, String> + Send + 'static,
    ) -> Result<T, String> {
        self.with_entry(authority, self.entry(window)?, move |cx, window| {
            let label = cx
                .views
                .label_of_window(window)
                .ok_or("The window is gone.")?;
            let native = cx
                .app
                .get_webview_window(&label)
                .ok_or("The window is gone.")?;
            action(native)
        })
    }

    pub(super) fn view(
        &self,
        authority: &GenerateAuthority,
        window: WindowId,
    ) -> Result<Option<ViewId>, String> {
        if authority.is_closed_event() {
            return self.closed_proof(authority).and_then(|proof| {
                if proof.window != window {
                    return Err("The closed window does not match.".into());
                }
                Ok(proof.identity.created.get().map(|created| created.view))
            });
        }
        let source = self.source(authority)?;
        let entry = self
            .entries
            .lock()
            .map_err(|_| "The generate window registry is unavailable.")?
            .get(&window)
            .cloned();
        match entry {
            Some(entry) => {
                let view = entry.created()?.view;
                self.with_entry(authority, entry, move |_, _| Ok(Some(view)))
            }
            None => native_owner::with_owner_effect(&source, &authority.cancel, || None)
                .ok_or_else(|| LOST.into()),
        }
    }

    pub(super) fn is_destroyed(
        &self,
        authority: &GenerateAuthority,
        window: WindowId,
    ) -> Result<bool, String> {
        if authority.is_closed_event() {
            return self.closed_proof(authority).and_then(|proof| {
                if proof.window == window {
                    Ok(true)
                } else {
                    Err("The closed window does not match.".into())
                }
            });
        }
        let source = self.source(authority)?;
        let entry = self
            .entries
            .lock()
            .map_err(|_| "The generate window registry is unavailable.")?
            .get(&window)
            .cloned();
        match entry {
            Some(entry) => self.with_entry(authority, entry, |cx, window| {
                Ok(cx.views.label_of_window(window).is_none())
            }),
            None => native_owner::with_owner_effect(&source, &authority.cancel, || true)
                .ok_or_else(|| LOST.into()),
        }
    }

    pub(super) fn is_minimized(
        &self,
        authority: &GenerateAuthority,
        window: WindowId,
    ) -> Result<bool, String> {
        self.with_window(authority, window, |native| {
            native.is_minimized().map_err(|error| error.to_string())
        })
    }
    pub(super) fn restore(
        &self,
        authority: &GenerateAuthority,
        window: WindowId,
    ) -> Result<(), String> {
        self.with_window(authority, window, |native| {
            native.unminimize().map_err(|error| error.to_string())
        })
    }
    pub(super) fn show(
        &self,
        authority: &GenerateAuthority,
        window: WindowId,
    ) -> Result<(), String> {
        self.with_entry(authority, self.entry(window)?, |cx, window| {
            cx.windows.show(window, true)
        })
    }
    pub(super) fn focus(
        &self,
        authority: &GenerateAuthority,
        window: WindowId,
    ) -> Result<(), String> {
        self.with_window(authority, window, |native| {
            native.set_focus().map_err(|error| error.to_string())
        })
    }
    pub(super) fn close(
        &self,
        authority: &GenerateAuthority,
        window: WindowId,
    ) -> Result<(), String> {
        self.with_entry(authority, self.entry(window)?, |cx, window| {
            cx.windows.close_window(window)
        })
    }
    pub(super) fn remove_menu(
        &self,
        authority: &GenerateAuthority,
        window: WindowId,
    ) -> Result<(), String> {
        self.with_window(authority, window, |native| {
            native
                .remove_menu()
                .map(|_| ())
                .map_err(|error| error.to_string())
        })
    }
    pub(super) fn update_caption(
        &self,
        authority: &GenerateAuthority,
        window: WindowId,
        overlay: &TitleBarOverlay,
    ) -> Result<(), String> {
        let entry = self.entry(window)?;
        let created = entry.created()?;
        let permit = created.lifecycle.admit_native_effect()?;
        let native = created.native.clone();
        let handle = created.handle.clone();
        let overlay = overlay.clone();
        self.with_entry(authority, entry, move |_, _| {
            generate_caption::update(&native, &handle, &permit, &overlay)
        })
    }
    pub(super) fn watch_theme(
        &self,
        authority: &GenerateAuthority,
        window: WindowId,
        watch: bool,
    ) -> Result<(), String> {
        if authority.is_closed_event() {
            let proof = self.closed_proof(authority)?;
            if proof.window != window || watch {
                return Err("Closed-window authority only removes its own theme watch.".into());
            }
            proof.identity.watch_theme.store(false, Ordering::Release);
            return Ok(());
        }
        let entry = self.entry(window)?;
        let identity = entry.identity.clone();
        self.with_entry(authority, entry, move |_, _| {
            identity.watch_theme.store(watch, Ordering::Release);
            Ok(())
        })
    }
    pub(super) fn load(
        &self,
        authority: &GenerateAuthority,
        window: WindowId,
        url: &str,
    ) -> Result<(), String> {
        let url = url.to_owned();
        let entry = self.entry(window)?;
        let view = entry.created()?.view;
        self.with_entry(authority, entry, move |cx, _| {
            cx.windows.load_view(view, &url)
        })
    }
    pub(super) fn dark_colors(&self, authority: &GenerateAuthority) -> Result<bool, String> {
        let source = self.source(authority)?;
        let cancel = authority.cancel.clone();
        let cx = self.cx.clone();
        on_ui(&self.cx.ui, move || {
            native_owner::run_accepted_ui_pair(
                cx.ui.as_ref(),
                &source,
                &source,
                &cancel,
                &cancel,
                || {
                    let window = cx
                        .views
                        .main()
                        .and_then(|sender| cx.views.label_of_window(sender.window))
                        .and_then(|label| cx.app.get_window(&label))
                        .or_else(|| cx.app.windows().into_values().next())
                        .ok_or(
                            "The native theme is unavailable without a live application window.",
                        )?;
                    window
                        .theme()
                        .map(|theme| theme == tauri::Theme::Dark)
                        .map_err(|error| error.to_string())
                },
            )?
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry() -> Entry {
        Entry {
            identity: Arc::new(Identity {
                created: OnceLock::new(),
                closed: AtomicBool::new(false),
                ready: AtomicBool::new(false),
                watch_theme: AtomicBool::new(true),
                pending: CancelToken::default(),
            }),
            binding: Arc::new(Mutex::new(Binding::Waiting)),
            first_events: Arc::new(OnceLock::new()),
        }
    }

    #[test]
    fn cleanup_proof_is_exact_and_never_grants_renderer_authority() {
        let entry = entry();
        let window = WindowId(7);
        let native = NativeContext::new(WindowProof {
            window,
            identity: entry.identity.clone(),
        });
        let mut authority = GenerateAuthority {
            origin: GenerateOrigin::WindowEvent {
                window,
                identity: native.clone(),
                event: GenerateWindowEvent::Closed,
            },
            cancel: CancelToken::default(),
        };
        assert!(cleanup_proof(&authority).is_err());
        entry.identity.closed.store(true, Ordering::Release);
        authority.cancel.cancel();
        assert!(cleanup_proof(&authority).is_ok());
        assert!(native_owner::authenticated_owner(&native).is_none());
        authority.origin = GenerateOrigin::WindowEvent {
            window: WindowId(8),
            identity: native.clone(),
            event: GenerateWindowEvent::Closed,
        };
        assert!(cleanup_proof(&authority).is_err());
        authority.origin = GenerateOrigin::WindowEvent {
            window,
            identity: native,
            event: GenerateWindowEvent::Focus,
        };
        assert!(cleanup_proof(&authority).is_err());
    }

    #[test]
    fn a_reused_window_number_does_not_reuse_its_native_proof() {
        let old = entry();
        let next = entry();
        let window = WindowId(7);
        let proof = NativeContext::new(WindowProof {
            window,
            identity: old.identity.clone(),
        });
        assert!(proof_matches(&proof, window, &old));
        assert!(!proof_matches(&proof, window, &next));
        assert!(!proof_matches(&proof, WindowId(8), &old));
    }

    #[test]
    fn revocation_keeps_the_created_generation_and_never_relabels_it() {
        let entry = entry();
        let original = RendererGeneration::new();
        let replacement = RendererGeneration::new();
        assert!(
            entry
                .identity
                .created
                .set(Created {
                    window: WindowId(7),
                    view: ViewId(9),
                    generation: original.clone()
                })
                .is_ok()
        );
        assert!(
            entry
                .identity
                .created
                .set(Created {
                    window: WindowId(7),
                    view: ViewId(9),
                    generation: replacement.clone()
                })
                .is_err()
        );
        entry.revoke();
        assert!(entry.identity.pending.is_cancelled());
        assert!(!original.is_active());
        assert!(replacement.is_active());
        assert!(
            entry
                .created()
                .unwrap()
                .generation
                .same_generation(&original)
        );
        assert!(entry.bound().is_err());
    }
}
