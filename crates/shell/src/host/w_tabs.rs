mod accepted;
mod activation;
mod activation_publication;
mod disposal;
mod generated_retry;
mod hooks;
mod native_command;
mod owned_creation;
mod recovery;
mod saved;
mod saved_creation;
mod saved_prepare;
mod spare_actor;
mod spare_seed;
pub(crate) use activation_publication::{
    indeterminate_retained_replies, pending_retained_replies, retry_retained_replies,
};
pub(crate) use hooks::default_save_dir;
pub(crate) use recovery::create_recovery_view_owned;

use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};

use crate::native::{
    BrowserCreation, BrowserState, NativeEvent, NativeEventKind, ViewEvent, WebviewExt,
};
use main_core::shell::detached_windows::WindowOptions as DetachedOptions;
use main_core::shell::split_layout::Rect;
use main_core::shell::tab_manager::{
    HwpUntitledCopy, TabError, TabResult, ViewSpec as EditorViewSpec,
};
use shell_contract::ids::{ViewId, WindowId};
use shell_contract::provided::Provided;
use shell_contract::runtime::Services;
use shell_contract::runtime::Views;
use shell_contract::{Hosts, Surface};
use shell_ipc_w::tabs::{
    HostEvent, HostEvents, MenuEntry, MenuEntryKind, ShellWindow, TimerKey, ViewHappened,
    ViewListen, WindowEvent, WindowState, WindowsHost, WindowsHostRef,
};
use tauri::Manager;

use super::window::{Bounds, Bridge, ViewSpec, WindowOptions, WindowSpec};
use super::{HostContext, on_ui};

struct Hooks {
    services: Services,
    provided: Rc<Provided>,
    cx: HostContext,
}

#[derive(Default)]
struct OrdinaryActorStages {
    slides: Option<shell_ipc_d1::slides::PreparedNativeSlides>,
    diagram: Option<shell_ipc_d1::diagram::PreparedNativeDiagram>,
}
impl OrdinaryActorStages {
    fn is_empty(&self) -> bool {
        self.slides.is_none() && self.diagram.is_none()
    }
    fn slides_rollback(
        &self,
        view: ViewId,
    ) -> Option<Rc<dyn shell_ipc_w::tabs::host::EditorSeedRollback>> {
        self.slides
            .as_ref()
            .map(|_| -> Rc<dyn shell_ipc_w::tabs::host::EditorSeedRollback> {
                Rc::new(hooks::SlidesActorRollback(view))
            })
    }
    fn with_validated(
        &mut self,
        transition: &shell_contract::ipc::NativeBrowserTransition<'_>,
        next: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        if self.slides.is_some() && self.diagram.is_some() {
            return Err("Multiple ordinary actors were prepared for one view".into());
        }
        if let Some(stage) = &mut self.slides {
            stage.commit(transition, next)?;
        } else if let Some(stage) = &mut self.diagram {
            stage.commit(transition, next)?;
        } else {
            next()?;
        }
        self.slides.take();
        self.diagram.take();
        Ok(())
    }
}

impl Hooks {
    fn editor<T: 'static>(&self, name: &str) -> TabResult<Rc<T>> {
        self.provided
            .all::<T>()
            .into_iter()
            .next()
            .ok_or_else(|| TabError(format!("{name} editor is not registered")))
    }

    fn release_closed_renderer(&self, view: ViewId) {
        let cx = self.cx.clone();
        super::renderer_disposal::release(view, move || {
            std::thread::spawn(move || {
                let ui = cx.ui.clone();
                on_ui(&ui, move || {
                    if super::renderer_disposal::can_close(view)
                        && let Err(error) = cx.windows.close_view(view)
                    {
                        eprintln!("shell: closing retired view {}: {error}", view.0);
                    }
                });
            });
        });
    }

    /// Creates the chrome-free html present view of `owner`. It is the only view kind
    /// created through the unowned creation hook: it registers no editor actor, so no
    /// closure-receipt rollback entry exists to retire. Docs, Sheets and Slides views are
    /// created through the owned paths, which register their actors in an
    /// `EditorSeedRollbackStore`.
    fn create_html_present_view(
        &self,
        owner: i64,
        title: &str,
        done: Box<dyn FnOnce(shell_ipc_w::tabs::host::ViewCreationResult<ViewId>)>,
    ) {
        let spec = EditorViewSpec::HtmlPresent { owner, title };
        let mut actors = OrdinaryActorStages::default();
        match self.prepare_editor_view_impl(
            &spec,
            false,
            |_| Ok(()),
            None,
            None,
            false,
            None,
            &mut actors,
        ) {
            Ok(prepared) => {
                let browser = prepared.cx.views.prepare_browser_owner(
                    prepared.view,
                    &prepared.generation,
                    prepared.proof.owner(),
                );
                let adopted = browser.and_then(|browser| {
                    prepared
                        .generation
                        .with_active(|| {
                            prepared.proof.reservation().with_initial_owner(
                                prepared.proof.owner(),
                                |transition| {
                                    let mut browser = Some(browser.validate(transition)?);
                                    actors.with_validated(transition, &mut || {
                                        browser
                                            .take()
                                            .ok_or("The native browser guard was consumed")?
                                            .commit();
                                        Ok(())
                                    })
                                },
                            )
                        })
                        .ok_or_else(|| "The editor creation generation expired".to_owned())?
                });
                match adopted {
                    Ok(()) => accepted::schedule_plain(prepared, done),
                    Err(error) => hooks::fail_prepared_view(
                        prepared,
                        TabError(error),
                        Box::new(move |failure| done(Err(failure))),
                    ),
                }
            }
            Err(error) => done(Err(main_core::shell::tab_manager::ViewCreationFailure {
                error,
                cleanup: main_core::shell::tab_manager::ViewCreationCleanupState::Complete,
            })),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn prepare_editor_view_impl(
        &self,
        spec: &EditorViewSpec<'_>,
        deferred: bool,
        prepare: impl FnOnce(ViewId) -> Result<(), String>,
        docs_prepared: Option<&mut Option<shell_ipc_d2::docs::editors::DocsPreparedRegistration>>,
        sheets_prepared: Option<&mut Option<shell_ipc_d1::sheets::PreparedTabRegistration>>,
        recovery: bool,
        captured_window: Option<(WindowId, Arc<crate::views::NativeWindowLifecycle>)>,
        actors: &mut OrdinaryActorStages,
    ) -> TabResult<accepted::PreparedEditorView> {
        self.prepare_editor_view_source(
            spec,
            deferred,
            prepare,
            docs_prepared,
            sheets_prepared,
            recovery,
            captured_window,
            None,
            actors,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn prepare_editor_view_source(
        &self,
        spec: &EditorViewSpec<'_>,
        deferred: bool,
        prepare: impl FnOnce(ViewId) -> Result<(), String>,
        mut docs_prepared: Option<
            &mut Option<shell_ipc_d2::docs::editors::DocsPreparedRegistration>,
        >,
        mut sheets_prepared: Option<&mut Option<shell_ipc_d1::sheets::PreparedTabRegistration>>,
        recovery: bool,
        captured_window: Option<(WindowId, Arc<crate::views::NativeWindowLifecycle>)>,
        saved_input: Option<&shell_contract::ipc::NativeSavedInput>,
        actors: &mut OrdinaryActorStages,
    ) -> TabResult<accepted::PreparedEditorView> {
        let (surface, url) = match spec {
            EditorViewSpec::Docs { .. } => (
                Surface::Docs,
                "genoffice-app://docs/index.html?mode=tab".to_owned(),
            ),
            EditorViewSpec::Sheets { opening_workbook } => (
                Surface::Sheets,
                format!(
                    "genoffice-app://sheets/index.html?mode=tab{}",
                    if *opening_workbook == Some(true) {
                        "&openingWorkbook=1"
                    } else {
                        ""
                    }
                ),
            ),
            EditorViewSpec::Slides { .. } => (
                Surface::Slides,
                "genoffice-app://slides/index.html?mode=tab".to_owned(),
            ),
            EditorViewSpec::Pdf { .. } => {
                (Surface::Pdf, "genoffice-app://pdf/index.html".to_owned())
            }
            EditorViewSpec::Markdown { .. } => (
                Surface::Markdown,
                "genoffice-app://markdown/index.html".to_owned(),
            ),
            EditorViewSpec::Html { .. } => {
                (Surface::Html, "genoffice-app://html/index.html".to_owned())
            }
            EditorViewSpec::HtmlPresent { owner, title } => (
                Surface::Html,
                format!(
                    "genoffice-app://html/index.html?present={owner}&title={}",
                    percent_encoding::utf8_percent_encode(
                        title,
                        percent_encoding::NON_ALPHANUMERIC
                    )
                ),
            ),
            EditorViewSpec::Mindmap { .. } => (
                Surface::Mindmap,
                "genoffice-app://mindmap/index.html".to_owned(),
            ),
            EditorViewSpec::Epub { .. } => {
                (Surface::Epub, "genoffice-app://epub/index.html".to_owned())
            }
            EditorViewSpec::Diagram { .. } => (
                Surface::Diagram,
                "genoffice-app://diagram/index.html".to_owned(),
            ),
            EditorViewSpec::Hwp { .. } => {
                (Surface::Hwp, "genoffice-app://hwp/index.html".to_owned())
            }
        };
        let (window, window_lifecycle) = match captured_window {
            Some(captured) => captured,
            None => {
                let shell = NATIVE_WINDOWS
                    .get()
                    .and_then(|host| host.shell_window())
                    .ok_or_else(|| TabError("the shell window is not available".into()))?;
                let lifecycle = self
                    .cx
                    .views
                    .window_lifecycle(shell.window)
                    .ok_or_else(|| TabError("the shell window lifecycle is unavailable".into()))?;
                (shell.window, lifecycle)
            }
        };
        let request_policy = (surface == Surface::Diagram)
            .then(super::d1::diagram_headless::network::request_policy);
        let registration = self.cx.views.allocate_view().map_err(TabError)?;
        let (view, created) = self
            .cx
            .views
            .register_view(registration, window, surface)
            .map_err(TabError)?;
        let submission = BrowserCreation::default();
        let proof =
            super::window::NativeClosureProof::new(submission.receipt(), view, created.clone());
        if let Err(error) = self
            .cx
            .views
            .set_closure_proof(view, &created, proof.clone())
        {
            self.cx.views.remove_view_if_generation(view, &created);
            return Err(TabError(error));
        }
        let prepare_docs_later = docs_prepared.is_some();
        let docs_lease = None;
        let mut slides_lease = None;
        let mut asset_lease = None;
        let tracked = (|| -> TabResult<()> {
            if matches!(surface, Surface::Html | Surface::Markdown | Surface::Pdf) {
                asset_lease = Some(
                    self.cx
                        .views
                        .prepare_native_assets(view)
                        .map_err(TabError)?,
                );
            }
            // Saved-input construction reserves only the view and native
            // assets here. The editor's prepared token stages its actor and
            // barrier and publishes both in the saved adoption transition.
            if saved_input.is_some() {
                if surface == Surface::Slides {
                    slides_lease =
                        Some(self.cx.views.prepare_slides_media(view).map_err(TabError)?);
                }
                return Ok(());
            }
            match spec {
                EditorViewSpec::Docs { .. } => {
                    let editor = self.editor::<shell_ipc_d2::docs::editors::DocsEditors>("Docs")?;
                    if let Some(slot) = docs_prepared.as_deref_mut() {
                        *slot = Some(
                            editor
                                .prepare_view_registration(
                                    view,
                                    created.clone(),
                                    None,
                                    proof.reservation(),
                                    proof.owner(),
                                )
                                .map_err(TabError)?,
                        );
                    } else {
                        // every Docs view is created through an owned path, which stages its
                        // registration with the browser owner
                        return Err(TabError("A Docs view needs a prepared registration".into()));
                    }
                }
                EditorViewSpec::Sheets { .. } => {
                    let sheets = shell_ipc_d1::sheets::views()
                        .ok_or_else(|| TabError("Sheets editor is not registered".into()))?;
                    if let Some(slot) = sheets_prepared.as_deref_mut() {
                        *slot = Some(
                            sheets
                                .prepare_tab_registration(
                                    view,
                                    created.clone(),
                                    proof.reservation(),
                                    proof.owner(),
                                )
                                .map_err(TabError)?,
                        );
                    } else {
                        sheets.register_tab(view);
                    }
                }
                EditorViewSpec::Slides { path } => {
                    let slides = shell_ipc_d1::slides::views()
                        .ok_or_else(|| TabError("Slides editor is not registered".into()))?;
                    actors.slides = Some(
                        slides
                            .prepare_native_tab(view, &created, *path, proof.owner().clone())
                            .map_err(TabError)?,
                    );
                    slides_lease =
                        Some(self.cx.views.prepare_slides_media(view).map_err(TabError)?);
                }
                EditorViewSpec::Pdf { path } => {
                    if !recovery {
                        self.editor::<shell_ipc_d2::pdf::editor::editors::PdfEditors>("PDF")?
                            .open_view(view, saved_input.is_none().then_some(*path));
                    }
                }
                EditorViewSpec::Markdown { path } => self
                    .editor::<shell_ipc_d2::markdown::MarkdownEditors>("Markdown")?
                    .track_view(
                        view,
                        path.filter(|_| saved_input.is_none())
                            .map(std::path::Path::new),
                    ),
                EditorViewSpec::Html { path } => self
                    .editor::<shell_ipc_d2::html::HtmlEditors>("HTML")?
                    .track_view(
                        view,
                        path.filter(|_| saved_input.is_none())
                            .map(std::path::Path::new),
                    ),
                EditorViewSpec::HtmlPresent { owner, .. } => {
                    let owner = u32::try_from(*owner)
                        .map(ViewId)
                        .map_err(|_| TabError("invalid HTML present owner".into()))?;
                    self.editor::<shell_ipc_d2::html::HtmlEditors>("HTML")?
                        .bind_present(view, owner);
                }
                EditorViewSpec::Mindmap { path } => shell_ipc_d1::mindmap::handle()
                    .ok_or_else(|| TabError("Mindmap editor is not registered".into()))?
                    .create_view(view, path.filter(|_| saved_input.is_none())),
                EditorViewSpec::Epub { path } => self
                    .editor::<shell_ipc_d2::epub::EpubEditors>("EPUB")?
                    .open_view(
                        view,
                        saved_input.is_none().then(|| std::path::Path::new(path)),
                    ),
                EditorViewSpec::Diagram { path } => {
                    let diagrams = shell_ipc_d1::diagram::views()
                        .ok_or_else(|| TabError("Diagram editor is not registered".into()))?;
                    actors.diagram = Some(
                        diagrams
                            .prepare_native_tab(view, &created, *path, proof.owner().clone())
                            .map_err(TabError)?,
                    );
                }
                EditorViewSpec::Hwp {
                    path,
                    untitled_copy,
                } => {
                    let untitled = match untitled_copy {
                        HwpUntitledCopy::Flag(false) => shell_ipc_d2::hwp::UntitledOption::No,
                        HwpUntitledCopy::Flag(true) => shell_ipc_d2::hwp::UntitledOption::Copy,
                        HwpUntitledCopy::BaseName(base_name) => {
                            shell_ipc_d2::hwp::UntitledOption::Pdf {
                                base_name: base_name.clone(),
                            }
                        }
                    };
                    self.editor::<shell_ipc_d2::hwp::HwpEditors>("HWP")?
                        .grant_and_track(view, path.filter(|_| saved_input.is_none()), untitled);
                }
            }
            Ok(())
        })();
        if let Err(error) = tracked {
            self.cx.views.remove_view(view);
            return Err(error);
        }
        if let Err(error) = prepare(view) {
            self.cx.views.remove_view(view);
            return Err(TabError(error));
        }
        let deferred = deferred
            || docs_lease.is_some()
            || (surface == Surface::Docs && prepare_docs_later)
            || slides_lease.is_some()
            || asset_lease.is_some();
        if deferred {
            NATIVE_WINDOWS
                .get()
                .expect("window host is not installed")
                .deferred_views
                .lock()
                .unwrap()
                .insert(view);
        }
        Ok(accepted::PreparedEditorView {
            cx: self.cx.clone(),
            core: self.services.core.clone(),
            view,
            generation: created,
            proof,
            submission,
            window,
            window_lifecycle,
            surface,
            url,
            deferred,
            docs_lease,
            slides_lease,
            asset_lease,
            request_policy,
            invocation_on_frame: None,
            invocation_navigate: None,
        })
    }
}
fn fail_editor_view(cx: HostContext, view: ViewId, error: String) {
    if let Some(host) = NATIVE_WINDOWS.get() {
        host.deferred_views.lock().unwrap().remove(&view);
        host.fail_page_waiters(view);
    }
    let windows = cx.windows.clone();
    on_ui(&cx.ui, move || {
        let _ = windows.close_view(view);
    });
    eprintln!("shell: binding editor browser {}: {error}", view.0);
}

fn navigate_deferred(cx: HostContext, view: ViewId, url: String) {
    let windows = cx.windows.clone();
    let result = on_ui(&cx.ui, move || windows.load_view(view, &url));
    if let Err(error) = result {
        if let Some(host) = NATIVE_WINDOWS.get() {
            host.deferred_views.lock().unwrap().remove(&view);
            host.fail_page_waiters(view);
        }
        eprintln!("shell: loading deferred view {}: {error}", view.0);
    }
}

pub(crate) fn open_html_present_window(
    cx: &HostContext,
    provided: &Provided,
    owner: ViewId,
    title: &str,
    parent: Option<WindowId>,
) -> Result<ViewId, String> {
    if !cx
        .views
        .sender_of(owner)
        .is_some_and(|sender| sender.surface == Surface::Html)
    {
        return Err("the HTML owner is no longer open".into());
    }
    let editor = provided
        .all::<shell_ipc_d2::html::HtmlEditors>()
        .into_iter()
        .next()
        .ok_or_else(|| "the HTML editor is unavailable".to_owned())?;
    let platform = cx.clone();
    let (window, view) = on_ui(&cx.ui, move || {
        let size = parent
            .and_then(|id| native_window(&platform, id))
            .and_then(|window| {
                let scale = window.scale_factor().ok()?;
                let size = window.inner_size().ok()?.to_logical::<f64>(scale);
                Some((size.width, size.height))
            })
            .unwrap_or((1200.0, 850.0));
        platform.windows.open_deferred_window(
            WindowSpec {
                title: "SeuOffice".into(),
                size,
                min_size: None,
                visible: false,
                decorations: true,
                transparent: false,
                resizable: true,
                parent: None,
                focus: true,
                main: false,
                view: ViewSpec {
                    url: String::new(),
                    bridge: Bridge::Surface(Surface::Html),
                },
            },
            WindowOptions {
                on_frame: Some(Arc::new(frame_event)),
                ..Default::default()
            },
        )
    })?;
    editor.bind_present(view, owner);
    let url = format!(
        "genoffice-app://html/index.html?present={}&title={}",
        owner.0,
        percent_encoding::utf8_percent_encode(title, percent_encoding::NON_ALPHANUMERIC)
    );
    let result = (|| -> Result<(), String> {
        let lease = cx.views.prepare_native_assets(view)?;
        let webview = cx.windows.webview(view).ok_or("the present view is gone")?;
        let core = NATIVE_WINDOWS
            .get()
            .and_then(|host| host.core.get())
            .cloned()
            .ok_or("the tab core is unavailable")?;
        let platform = cx.clone();
        let ready_view = webview.clone();
        let browser = webview.native_browser()?;
        browser.navigator().when_ready(move |ready| {
            if let Err(error) = ready {
                post_present_failure(core, platform, window, error.to_string());
                return;
            }
            let failed_core = core.clone();
            let failed_platform = platform.clone();
            if let Err(error) = ready_view.with_native_async(move |native| {
                let bound = native.and_then(|native| {
                    platform.views.bind_native_assets_browser(
                        view,
                        &lease,
                        native.identity.id(),
                        native.identity.clone(),
                    )
                });
                core.post(Box::new(move || {
                    let result = bound.and_then(|()| {
                        let on_platform = platform.clone();
                        on_ui(&platform.ui, move || {
                            on_platform.windows.load_view(view, &url)?;
                            let native = native_window(&on_platform, window)
                                .ok_or("the present window is gone")?;
                            native.show().map_err(|error| error.to_string())?;
                            native.set_focus().map_err(|error| error.to_string())
                        })
                    });
                    if let Err(error) = result {
                        fail_present_window(platform, window, error);
                    }
                }));
            }) {
                post_present_failure(failed_core, failed_platform, window, error.to_string());
            }
        });
        Ok(())
    })();
    if let Err(error) = result {
        let platform = cx.clone();
        on_ui(&cx.ui, move || {
            if let Some(native) = native_window(&platform, window) {
                let _ = native.destroy();
            }
        });
        return Err(error);
    }
    Ok(view)
}
fn post_present_failure(
    core: Arc<dyn shell_contract::runtime::Thread>,
    cx: HostContext,
    window: WindowId,
    error: String,
) {
    core.post(Box::new(move || fail_present_window(cx, window, error)));
}

fn fail_present_window(cx: HostContext, window: WindowId, error: String) {
    let platform = cx.clone();
    on_ui(&cx.ui, move || {
        if let Some(native) = native_window(&platform, window) {
            let _ = native.destroy();
        }
    });
    eprintln!("shell: opening HTML presentation: {error}");
}

pub(crate) fn close_html_present_view(
    cx: &HostContext,
    provided: &Provided,
    view: ViewId,
) -> Result<(), String> {
    if let Some(tabs) = provided
        .all::<shell_ipc_w::tabs::TabsState>()
        .into_iter()
        .next()
        && tabs.tab_id_for(view).is_some()
    {
        return if tabs.close_document_without_prompt(view) {
            Ok(())
        } else {
            Err("the present tab could not be closed".into())
        };
    }
    let sender = cx
        .views
        .sender_of(view)
        .ok_or_else(|| "the present view is gone".to_owned())?;
    let platform = cx.clone();
    on_ui(&cx.ui, move || {
        if platform.views.views_of_window(sender.window).len() == 1 {
            native_window(&platform, sender.window)
                .ok_or_else(|| "the present window is gone".to_owned())?
                .destroy()
                .map_err(|error| error.to_string())
        } else {
            platform.windows.close_view(view)
        }
    })
}

#[cfg(target_os = "macos")]
pub(crate) fn set_html_simple_fullscreen(
    cx: &HostContext,
    id: WindowId,
    on: bool,
) -> Result<(), String> {
    let window = native_window(cx, id).ok_or_else(|| "the present window is gone".to_owned())?;
    set_html_simple_fullscreen_window(cx, &window, on)
}

#[cfg(target_os = "macos")]
pub(crate) fn set_html_simple_fullscreen_window(
    cx: &HostContext,
    window: &tauri::Window,
    on: bool,
) -> Result<(), String> {
    if on && window.is_fullscreen().map_err(|error| error.to_string())? {
        return Ok(());
    }
    let set_fullscreenable = |enabled| {
        let window = window.clone();
        on_ui(&cx.ui, move || {
            use crate::native::WindowExt;
            use objc2::rc::Retained;
            use objc2_app_kit::{NSWindow, NSWindowCollectionBehavior};
            let handle = window
                .native_window_handle()
                .map_err(|error| error.to_string())?;
            handle
                .with_window(|_| {
                    let _main = objc2::MainThreadMarker::new()
                        .ok_or_else(|| "not on the main thread".to_owned())?;
                    let pointer = window.ns_window().map_err(|error| error.to_string())?;
                    // SAFETY: the creation-bound native window remains admitted for this operation.
                    let native = unsafe { Retained::retain(pointer.cast::<NSWindow>()) }
                        .ok_or_else(|| "window has no native NSWindow".to_owned())?;
                    let mut behavior = native.collectionBehavior();
                    if enabled {
                        behavior.remove(NSWindowCollectionBehavior::FullScreenNone);
                        behavior.insert(NSWindowCollectionBehavior::FullScreenPrimary);
                    } else {
                        behavior.remove(
                            NSWindowCollectionBehavior::FullScreenPrimary
                                | NSWindowCollectionBehavior::FullScreenAuxiliary,
                        );
                        behavior.insert(NSWindowCollectionBehavior::FullScreenNone);
                    }
                    native.setCollectionBehavior(behavior);
                    Ok::<(), String>(())
                })
                .map_err(|error| error.to_string())?
        })
    };
    if on {
        set_fullscreenable(false)?;
    }
    window
        .set_simple_fullscreen(on)
        .map_err(|error| error.to_string())?;
    if !on {
        set_fullscreenable(true)?;
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn set_html_simple_fullscreen(
    _: &HostContext,
    _: WindowId,
    _: bool,
) -> Result<(), String> {
    Ok(())
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn set_html_simple_fullscreen_window(
    _: &HostContext,
    _: &tauri::Window,
    _: bool,
) -> Result<(), String> {
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn prepare_recovery_view(
    services: &Services,
    provided: Rc<Provided>,
    kind: main_core::shell::tab_manager::TabKind,
    path: Option<&str>,
    untitled: bool,
    docs: &mut Option<shell_ipc_d2::docs::editors::DocsPreparedRegistration>,
    sheets: &mut Option<shell_ipc_d1::sheets::PreparedTabRegistration>,
    actors: &mut OrdinaryActorStages,
) -> Result<accepted::PreparedEditorView, String> {
    use main_core::shell::tab_manager::TabKind;
    let spec = match kind {
        TabKind::Docs => EditorViewSpec::Docs { path },
        TabKind::Sheets => EditorViewSpec::Sheets {
            opening_workbook: Some(untitled || path.is_some()),
        },
        TabKind::Slides => EditorViewSpec::Slides { path },
        TabKind::Pdf => EditorViewSpec::Pdf {
            path: path.unwrap_or(""),
        },
        TabKind::Markdown => EditorViewSpec::Markdown { path },
        TabKind::Html => EditorViewSpec::Html { path },
        TabKind::Mindmap => EditorViewSpec::Mindmap { path },
        TabKind::Epub => EditorViewSpec::Epub {
            path: path.unwrap_or(""),
        },
        TabKind::Diagram => EditorViewSpec::Diagram { path },
        TabKind::Hwp => EditorViewSpec::Hwp {
            path,
            untitled_copy: HwpUntitledCopy::Flag(false),
        },
        TabKind::Home | TabKind::Help => {
            return Err("this tab kind is not a recoverable editor".into());
        }
    };
    let cx = services
        .hosts
        .need::<HostContext>(shell_contract::Batch::W)
        .map_err(|error| format!("{error:?}"))?;
    Hooks {
        services: services.clone(),
        provided,
        cx,
    }
    .prepare_editor_view_impl(
        &spec,
        true,
        |_| Ok(()),
        Some(docs),
        Some(sheets),
        true,
        None,
        actors,
    )
    .map_err(|error| error.0)
}

pub(crate) fn nudge_recovery_sheets() -> Result<(), String> {
    shell_ipc_d1::sheets::views()
        .ok_or_else(|| "Sheets editor is not registered".to_owned())?
        .start_queued_workbook_nudge();
    Ok(())
}
pub(super) fn hooks(
    services: &Services,
    provided: Rc<Provided>,
) -> Result<Rc<dyn shell_ipc_w::tabs::TabHooks>, shell_contract::ipc::RegisterError> {
    let cx = services
        .hosts
        .need::<HostContext>(shell_contract::Batch::W)?;
    let host = NATIVE_WINDOWS
        .get()
        .ok_or(shell_contract::ipc::RegisterError::MissingHost {
            batch: shell_contract::Batch::W,
            host: "WindowsHost",
        })?;
    let _ = host.core.set(services.core.clone());
    Ok(Rc::new(Hooks {
        services: services.clone(),
        provided,
        cx,
    }))
}

#[derive(Clone)]
struct ParkingWindow {
    id: WindowId,
    window: tauri::Window,
    lifecycle: Arc<crate::views::NativeWindowLifecycle>,
}

impl ParkingWindow {
    fn same(&self, other: &Self) -> bool {
        self.id == other.id && Arc::ptr_eq(&self.lifecycle, &other.lifecycle)
    }
}

type ViewListener = (
    ViewId,
    ViewListen,
    shell_contract::runtime::RendererGeneration,
    Option<BrowserState>,
);

struct NativeWindows {
    cx: HostContext,
    core: OnceLock<Arc<dyn shell_contract::runtime::Thread>>,
    events: Mutex<Option<Arc<dyn HostEvents>>>,
    listeners: Mutex<Vec<ViewListener>>,
    page_waiters: Mutex<HashMap<ViewId, Vec<u64>>>,
    finished_loads: Mutex<HashMap<ViewId, bool>>,
    deferred_views: Mutex<HashSet<ViewId>>,
    timers: Mutex<Vec<(TimerKey, u64)>>,
    next_timer: AtomicU64,
    parking: Mutex<Option<ParkingWindow>>,
    detached: Mutex<HashSet<WindowId>>,
}

static NATIVE_WINDOWS: OnceLock<Arc<NativeWindows>> = OnceLock::new();

pub(super) fn install(hosts: &mut Hosts, cx: &HostContext) {
    let host = Arc::new(NativeWindows {
        cx: cx.clone(),
        core: OnceLock::new(),
        events: Mutex::new(None),
        listeners: Mutex::new(Vec::new()),
        page_waiters: Mutex::new(HashMap::new()),
        finished_loads: Mutex::new(HashMap::new()),
        deferred_views: Mutex::new(HashSet::new()),
        timers: Mutex::new(Vec::new()),
        next_timer: AtomicU64::new(1),
        detached: Mutex::new(HashSet::new()),
        parking: Mutex::new(None),
    });
    NATIVE_WINDOWS
        .set(host.clone())
        .unwrap_or_else(|_| panic!("tab window host installed twice"));
    cx.views.on_destroyed(Box::new(|view| {
        accepted::generated_load_failed(view);
        if let Some(host) = NATIVE_WINDOWS.get() {
            host.listeners
                .lock()
                .unwrap()
                .retain(|(id, _, generation, _)| *id != view || generation.is_active());
            host.deferred_views.lock().unwrap().remove(&view);
            let mut finished = host.finished_loads.lock().unwrap();
            finished.remove(&view);
            let tokens = host.page_waiters.lock().unwrap().remove(&view);
            drop(finished);
            if let Some(tokens) = tokens {
                for token in tokens {
                    host.send(HostEvent::PageLoad {
                        token,
                        loaded: false,
                    });
                }
            }
            host.cleanup_parking();
        }
    }));
    cx.views.on_renderer_replaced(Box::new(|view, old| {
        if let Some(host) = NATIVE_WINDOWS.get() {
            host.retire_listeners(view, &old);
        }
    }));
    let windows: WindowsHostRef = host;
    hosts.insert(windows);
}

impl NativeWindows {
    fn register_listener_captured(
        &self,
        view: ViewId,
        listen: ViewListen,
        generation: &shell_contract::runtime::RendererGeneration,
        browser: Option<BrowserState>,
    ) -> Result<(), String> {
        if listen == ViewListen::PaneFocus && browser.is_none() {
            return Err("The captured focus browser is unavailable.".into());
        }
        let mut listeners = self
            .listeners
            .lock()
            .map_err(|_| "Tab listener state is unavailable.")?;
        if !listeners.iter().any(|(id, kind, existing, _)| {
            *id == view && *kind == listen && existing.same_generation(generation)
        }) {
            listeners.push((view, listen, generation.clone(), browser));
        }
        Ok(())
    }

    fn retire_listeners(
        &self,
        view: ViewId,
        generation: &shell_contract::runtime::RendererGeneration,
    ) {
        self.listeners
            .lock()
            .unwrap()
            .retain(|(id, kind, captured, _)| {
                *id != view
                    || *kind == ViewListen::PaneFocus
                    || !captured.same_generation(generation)
            });
    }

    fn cleanup_parking(&self) {
        let mut parking = self.parking.lock().unwrap();
        let Some(window) = parking.as_ref().cloned() else {
            return;
        };
        if !self.cx.views.views_of_window(window.id).is_empty() {
            return;
        }
        *parking = None;
        drop(parking);
        on_ui(&self.cx.ui, move || {
            window
                .window
                .destroy()
                .unwrap_or_else(|error| panic!("destroying parking window: {error}"));
        });
    }

    fn send(&self, event: HostEvent) {
        if let Some(events) = self.events.lock().unwrap().clone() {
            events.send(event);
        }
    }

    fn fail_page_waiters(&self, view: ViewId) {
        accepted::generated_load_failed(view);
        let mut finished = self.finished_loads.lock().unwrap();
        finished.insert(view, false);
        let tokens = self.page_waiters.lock().unwrap().remove(&view);
        drop(finished);
        if let Some(tokens) = tokens {
            for token in tokens {
                self.send(HostEvent::PageLoad {
                    token,
                    loaded: false,
                });
            }
        }
    }

    fn frame_event(&self, view: ViewId, event: &NativeEvent) {
        match &event.kind {
            NativeEventKind::DocumentRegistered { context }
                if context.browser().is_same_browser(&event.browser) =>
            {
                accepted::generated_native_registered(view, context.clone());
            }
            NativeEventKind::DocumentReady { context }
                if context.browser().is_same_browser(&event.browser) =>
            {
                accepted::generated_document_ready(view, context.clone());
            }
            NativeEventKind::DocumentRevoked { document, .. } => {
                accepted::generated_native_revoked(view, document.clone());
            }
            NativeEventKind::NavigationStarted {
                url,
                same_document: false,
                ..
            } => {
                if url.as_str() != "about:blank" {
                    self.deferred_views.lock().unwrap().remove(&view);
                }
                self.finished_loads.lock().unwrap().remove(&view);
            }
            NativeEventKind::LoadFinished { ok, .. } => {
                if self.deferred_views.lock().unwrap().contains(&view) {
                    return;
                }
                let loaded = *ok;
                if !loaded {
                    accepted::generated_load_failed(view);
                }
                let mut finished = self.finished_loads.lock().unwrap();
                finished.insert(view, loaded);
                let tokens = self.page_waiters.lock().unwrap().remove(&view);
                drop(finished);
                if let Some(tokens) = tokens {
                    for token in tokens {
                        self.send(HostEvent::PageLoad { token, loaded });
                    }
                }
            }
            NativeEventKind::WebviewProcessFailed { .. } => {
                self.fail_page_waiters(view);
                // Generation-bound notification is delivered by window's native callback.
            }
            NativeEventKind::View(ViewEvent::Focus) => self.focus_event(view, &event.browser),
            _ => {}
        }
    }

    fn focus_event(&self, view: ViewId, browser: &BrowserState) {
        let subscribed = self
            .listeners
            .lock()
            .unwrap()
            .iter()
            .any(|(id, kind, _, captured)| {
                *id == view
                    && *kind == ViewListen::PaneFocus
                    && captured
                        .as_ref()
                        .is_some_and(|captured| captured.is_same_browser(browser))
            });
        if subscribed {
            self.send(HostEvent::View(view, ViewHappened::Focus));
        }
    }
}

fn native_window(cx: &HostContext, id: WindowId) -> Option<tauri::Window> {
    cx.app.get_window(&cx.views.label_of_window(id)?)
}

fn window_action(
    host: &NativeWindows,
    id: WindowId,
    action: impl FnOnce(tauri::Window) -> tauri::Result<()> + Send + 'static,
) {
    let cx = host.cx.clone();
    on_ui(&host.cx.ui, move || {
        let window = native_window(&cx, id).unwrap_or_else(|| panic!("window {} is gone", id.0));
        action(window).unwrap_or_else(|error| panic!("window {}: {error}", id.0));
    });
}

fn view_action(
    host: &NativeWindows,
    id: ViewId,
    action: impl FnOnce(tauri::Webview) -> tauri::Result<()> + Send + 'static,
) {
    let cx = host.cx.clone();
    on_ui(&host.cx.ui, move || {
        let view = cx
            .windows
            .webview(id)
            .unwrap_or_else(|| panic!("view {} is gone", id.0));
        action(view).unwrap_or_else(|error| panic!("view {}: {error}", id.0));
    });
}

fn monitor_rect(monitor: &tauri::window::Monitor) -> Rect {
    let scale = monitor.scale_factor();
    let area = monitor.work_area();
    Rect {
        x: area.position.x as f64 / scale,
        y: area.position.y as f64 / scale,
        width: area.size.width as f64 / scale,
        height: area.size.height as f64 / scale,
    }
}

impl WindowsHost for NativeWindows {
    fn attach(&self, events: Arc<dyn HostEvents>) {
        *self.events.lock().unwrap() = Some(events);
    }

    fn shell_window(&self) -> Option<ShellWindow> {
        self.cx.views.marked_main().map(|sender| ShellWindow {
            window: sender.window,
            view: sender.view,
        })
    }

    fn create_shell_window(&self) -> ShellWindow {
        let cx = self.cx.clone();
        on_ui(&self.cx.ui, move || {
            let (window, view) = cx
                .windows
                .open_window_with_options(
                    WindowSpec {
                        title: "SeuOffice".into(),
                        size: (1280.0, 860.0),
                        min_size: None,
                        visible: true,
                        decorations: true,
                        transparent: false,
                        resizable: true,
                        parent: None,
                        focus: true,
                        main: true,
                        view: ViewSpec {
                            url: "genoffice-app://shell/index.html".into(),
                            bridge: Bridge::Surface(Surface::Shell),
                        },
                    },
                    WindowOptions {
                        on_frame: Some(Arc::new(frame_event)),
                        ..Default::default()
                    },
                )
                .unwrap_or_else(|error| panic!("creating shell window: {error}"));
            ShellWindow { window, view }
        })
    }

    fn window_state(&self, id: WindowId) -> WindowState {
        let cx = self.cx.clone();
        on_ui(&self.cx.ui, move || {
            let Some(window) = native_window(&cx, id) else {
                return WindowState {
                    destroyed: true,
                    minimized: false,
                    focused: false,
                    visible: false,
                    content_bounds: Rect {
                        x: 0.0,
                        y: 0.0,
                        width: 0.0,
                        height: 0.0,
                    },
                    position: (0.0, 0.0),
                    title: String::new(),
                };
            };
            let scale = window.scale_factor().expect("window scale");
            let origin = window
                .inner_position()
                .expect("window content position")
                .to_logical::<f64>(scale);
            let size = window
                .inner_size()
                .expect("window content size")
                .to_logical::<f64>(scale);
            let position = window
                .outer_position()
                .expect("window position")
                .to_logical::<f64>(scale);
            WindowState {
                destroyed: false,
                minimized: window.is_minimized().expect("window minimized state"),
                focused: window.is_focused().expect("window focus state"),
                visible: window.is_visible().expect("window visibility"),
                content_bounds: Rect {
                    x: origin.x,
                    y: origin.y,
                    width: size.width,
                    height: size.height,
                },
                position: (position.x, position.y),
                title: window.title().expect("window title"),
            }
        })
    }

    fn create_window(
        &self,
        options: &DetachedOptions,
    ) -> shell_ipc_w::tabs::window_snapshot::WindowReservation {
        let options = options.clone();
        let show = options.show;
        let cx = self.cx.clone();
        let (id, window, lifecycle) = on_ui(&self.cx.ui, move || {
            cx.windows
                .open_empty_window_handle(
                    &options.title,
                    (options.width, options.height),
                    (options.min_width, options.min_height),
                    options.origin,
                    false,
                )
                .unwrap_or_else(|error| panic!("creating detached window: {error}"))
        });
        self.detached.lock().unwrap().insert(id);
        if show {
            on_ui(&self.cx.ui, move || {
                window
                    .show()
                    .and_then(|()| window.set_focus())
                    .unwrap_or_else(|error| panic!("showing detached window {}: {error}", id.0));
            });
        }
        shell_ipc_w::tabs::window_snapshot::WindowReservation::new(id, lifecycle)
    }

    fn window_restore(&self, id: WindowId) {
        window_action(self, id, |window| window.unminimize());
    }
    fn window_show(&self, id: WindowId) {
        window_action(self, id, |window| window.show());
    }
    fn window_show_inactive(&self, id: WindowId) {
        use crate::native::WindowExt;
        assert!(
            !self.cx.ui.is_current(),
            "show_inactive must not block the UI thread"
        );
        let window =
            native_window(&self.cx, id).unwrap_or_else(|| panic!("window {} is gone", id.0));
        window
            .show_inactive()
            .unwrap_or_else(|error| panic!("showing window without activation: {error}"));
    }
    fn window_focus(&self, id: WindowId) {
        window_action(self, id, |window| window.set_focus());
    }
    fn window_destroy(&self, id: WindowId) {
        if let Some(lifecycle) = self.cx.views.window_lifecycle(id) {
            lifecycle.begin_close();
        }
        window_action(self, id, |window| window.destroy());
    }
    fn window_set_title(
        &self,
        window: &shell_ipc_w::tabs::window_snapshot::WindowReservation,
        title: &str,
    ) {
        let captured = window.clone();
        let title = title.to_owned();
        on_ui(&self.cx.ui, move || {
            captured
                .set_title(&title)
                .unwrap_or_else(|error| panic!("window {}: {error}", captured.window().0))
        });
    }
    fn window_set_position(&self, id: WindowId, x: f64, y: f64) {
        window_action(self, id, move |window| {
            window.set_position(tauri::LogicalPosition::new(x, y))
        });
    }
    fn window_set_frame(&self, id: WindowId, frame: &Rect) {
        let frame = *frame;
        window_action(self, id, move |window| {
            if window.is_maximized()? {
                window.unmaximize()?;
            }
            if window.is_fullscreen()? {
                window.set_fullscreen(false)?;
            }
            window.set_position(tauri::LogicalPosition::new(frame.x, frame.y))?;
            window.set_size(tauri::LogicalSize::new(frame.width, frame.height))
        });
    }
    fn window_work_area(&self, id: WindowId) -> Rect {
        let cx = self.cx.clone();
        on_ui(&self.cx.ui, move || {
            let window =
                native_window(&cx, id).unwrap_or_else(|| panic!("window {} is gone", id.0));
            monitor_rect(
                &window
                    .current_monitor()
                    .expect("monitor query")
                    .expect("window has no monitor"),
            )
        })
    }

    fn add_child_view(&self, window: WindowId, view: ViewId) {
        let cx = self.cx.clone();
        on_ui(&self.cx.ui, move || {
            cx.windows
                .reparent_view(view, window)
                .unwrap_or_else(|error| panic!("{error}"));
        });
        self.cleanup_parking();
    }

    fn remove_child_view(&self, _: WindowId, view: ViewId) {
        let mut parking = self.parking.lock().unwrap();
        let cx = self.cx.clone();
        let target = match parking.as_ref() {
            Some(window) => window.id,
            None => {
                let created = on_ui(&self.cx.ui, move || {
                    let (id, window, lifecycle) = cx
                        .windows
                        .open_empty_window_handle("SeuOffice", (1.0, 1.0), (1.0, 1.0), None, false)
                        .unwrap_or_else(|error| panic!("creating parking window: {error}"));
                    ParkingWindow {
                        id,
                        window,
                        lifecycle,
                    }
                });
                let id = created.id;
                *parking = Some(created);
                id
            }
        };
        drop(parking);
        let cx = self.cx.clone();
        on_ui(&self.cx.ui, move || {
            let webview = cx
                .windows
                .webview(view)
                .unwrap_or_else(|| panic!("view {} is gone", view.0));
            webview
                .hide()
                .unwrap_or_else(|error| panic!("hiding view: {error}"));
            cx.windows
                .reparent_view(view, target)
                .unwrap_or_else(|error| panic!("{error}"));
        });
    }

    fn view_destroyed(&self, view: ViewId) -> bool {
        self.cx.views.sender_of(view).is_none()
    }
    fn view_set_visible(&self, view: ViewId, visible: bool) {
        view_action(
            self,
            view,
            move |view| if visible { view.show() } else { view.hide() },
        );
    }
    fn view_set_bounds(&self, view: ViewId, bounds: &Rect) {
        let bounds = *bounds;
        view_action(self, view, move |view| {
            view.set_position(tauri::LogicalPosition::new(bounds.x, bounds.y))?;
            view.set_size(tauri::LogicalSize::new(bounds.width, bounds.height))
        });
    }
    fn view_focus(&self, view: ViewId) {
        crate::trace::trace!("focus", "the tabs give view {} the focus", view.0);
        view_action(self, view, |view| view.set_focus());
    }
    fn view_close(&self, view: ViewId) {
        let cx = self.cx.clone();
        on_ui(&self.cx.ui, move || {
            cx.windows
                .close_view(view)
                .unwrap_or_else(|error| panic!("{error}"))
        });
        self.cleanup_parking();
    }
    fn view_reload(&self, view: ViewId) {
        let cx = self.cx.clone();
        on_ui(&self.cx.ui, move || {
            assert!(cx.windows.reload(view), "reload failed for view {}", view.0)
        });
    }
    fn view_load_url(&self, view: ViewId, url: &str) {
        let url = crate::app_scheme::webview_url(url)
            .unwrap_or_else(|| panic!("invalid renderer URL {url}"));
        view_action(self, view, move |view| view.navigate(url));
    }
    fn view_listen(&self, view: ViewId, listen: ViewListen) {
        let Some(generation) = self.cx.views.renderer_generation(view) else {
            return;
        };
        if listen != ViewListen::PaneFocus {
            let _ = generation
                .with_active(|| self.register_listener_captured(view, listen, &generation, None));
            return;
        }
        let Some(webview) = self.cx.windows.webview(view) else {
            return;
        };
        let Some(host) = NATIVE_WINDOWS.get().cloned() else {
            return;
        };
        if let Err(error) = webview.with_native_async(move |result| match result {
            Ok(native) => {
                let browser = native.identity.clone();
                let _ = generation.with_active(|| {
                    host.register_listener_captured(view, listen, &generation, Some(browser))
                });
            }
            Err(error) => eprintln!("shell: capturing focus listener browser: {error}"),
        }) {
            eprintln!("shell: scheduling focus listener browser capture: {error}");
        }
    }
    fn watch_page_load(&self, view: ViewId, token: u64) {
        let finished = self.finished_loads.lock().unwrap();
        if self.view_destroyed(view) {
            drop(finished);
            self.send(HostEvent::PageLoad {
                token,
                loaded: false,
            });
        } else if let Some(loaded) = finished.get(&view).copied() {
            drop(finished);
            self.send(HostEvent::PageLoad { token, loaded });
        } else {
            self.page_waiters
                .lock()
                .unwrap()
                .entry(view)
                .or_default()
                .push(token);
        }
    }
    fn create_help_view(
        &self,
        done: Box<dyn FnOnce(shell_ipc_w::tabs::host::ViewCreationResult<ViewId>)>,
    ) {
        use main_core::shell::tab_manager::{ViewCreationCleanupState, ViewCreationFailure};
        let Some(shell) = self.shell_window() else {
            return done(Err(ViewCreationFailure {
                error: TabError("no shell window for help view".into()),
                cleanup: ViewCreationCleanupState::Complete,
            }));
        };
        let Some(window_lifecycle) = self.cx.views.window_lifecycle(shell.window) else {
            return done(Err(ViewCreationFailure {
                error: TabError("the shell window is unavailable".into()),
                cleanup: ViewCreationCleanupState::Complete,
            }));
        };
        let Some(core) = self.core.get().cloned() else {
            return done(Err(ViewCreationFailure {
                error: TabError("the core thread is unavailable".into()),
                cleanup: ViewCreationCleanupState::Complete,
            }));
        };
        let registration = match self.cx.views.allocate_view() {
            Ok(registration) => registration,
            Err(error) => {
                return done(Err(ViewCreationFailure {
                    error: TabError(error),
                    cleanup: ViewCreationCleanupState::Complete,
                }));
            }
        };
        let (view, generation) =
            match self
                .cx
                .views
                .register_view(registration, shell.window, Surface::Shell)
            {
                Ok(registered) => registered,
                Err(error) => {
                    return done(Err(ViewCreationFailure {
                        error: TabError(error),
                        cleanup: ViewCreationCleanupState::Complete,
                    }));
                }
            };
        let child_generation = generation.clone();
        let cx = self.cx.clone();
        let create_cx = cx.clone();
        accepted::schedule_child(
            cx,
            core,
            view,
            generation,
            None,
            Box::new(move || {
                create_cx
                    .windows
                    .add_reserved_view(
                        view,
                        &child_generation,
                        shell.window,
                        &window_lifecycle,
                        ViewSpec {
                            url: "genoffice-app://help/index.html?mode=help".into(),
                            bridge: Bridge::Surface(Surface::Shell),
                        },
                        Bounds {
                            x: 0.0,
                            y: 0.0,
                            width: 1.0,
                            height: 1.0,
                        },
                        WindowOptions::default(),
                        None,
                        None,
                        Some(Arc::new(frame_event)),
                    )
                    .map_err(|error| accepted::InstallError {
                        error: TabError(error.message),
                        close: error.close,
                        proof: error.proof,
                    })
            }),
            done,
        );
    }

    fn create_help_native_command(
        &self,
        operation: shell_contract::ipc::NativeInvocationOperation,
        participants: Vec<shell_contract::ipc::NativeRequestOwner>,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        done: Box<
            dyn FnOnce(
                shell_ipc_w::tabs::host::ViewCreationResult<
                    shell_contract::ipc::NativeInvocationDocument,
                >,
            ),
        >,
    ) {
        use main_core::shell::tab_manager::{ViewCreationCleanupState, ViewCreationFailure};
        use shell_contract::ipc::NativeEditorCommand;
        let fail = |message: String| ViewCreationFailure {
            error: TabError(message),
            cleanup: ViewCreationCleanupState::Complete,
        };
        let command = match operation.selection() {
            shell_contract::ipc::NativeInvocationSelection::NativeCommand(
                command @ NativeEditorCommand::OpenHelp { .. },
            ) => command.clone(),
            _ => return done(Err(fail("The native command is not a help request".into()))),
        };
        let topic = match &command {
            NativeEditorCommand::OpenHelp { topic } => topic.as_deref(),
            _ => unreachable!(),
        };
        let dev_url = std::env::var("ELECTRON_RENDERER_URL").ok();
        let url = match main_core::shell::tab_manager::help_view_url(dev_url.as_deref(), topic) {
            Ok(url) => url,
            Err(error) => return done(Err(fail(error.0))),
        };
        let Some(shell) = self.shell_window() else {
            return done(Err(fail("no shell window for help view".into())));
        };
        let Some(window_lifecycle) = self.cx.views.window_lifecycle(shell.window) else {
            return done(Err(fail("the shell window is unavailable".into())));
        };
        let Some(core) = self.core.get().cloned() else {
            return done(Err(fail("the core thread is unavailable".into())));
        };
        let Some(prepare) = crate::ipc::native_owner::with_native_command_preparation_effect(
            &operation,
            &command,
            &participants,
            &cancel,
            &pending,
            || {
                || {
                    let registration = self.cx.views.allocate_view()?;
                    let (view, created) =
                        self.cx
                            .views
                            .register_view(registration, shell.window, Surface::Shell)?;
                    Ok::<_, String>((view, created))
                }
            },
        ) else {
            return done(Err(fail(
                "The native help command expired before preparation".into(),
            )));
        };
        let (view, created) = match prepare() {
            Ok(value) => value,
            Err(error) => return done(Err(fail(error))),
        };
        let target = match operation.reserve_target(view, created.clone()) {
            Ok(target) => target,
            Err(error) => {
                self.cx.views.remove_view_if_generation(view, &created);
                return done(Err(fail(error)));
            }
        };
        let child_generation = created.clone();
        let cx = self.cx.clone();
        let create_cx = cx.clone();
        native_command::start(
            cx,
            core,
            target,
            command,
            participants,
            cancel,
            pending,
            None,
            Box::new(move |frame: native_command::FrameCallback| {
                create_cx
                    .windows
                    .add_reserved_view(
                        view,
                        &child_generation,
                        shell.window,
                        &window_lifecycle,
                        ViewSpec {
                            url,
                            bridge: Bridge::Surface(Surface::Shell),
                        },
                        Bounds {
                            x: 0.0,
                            y: 0.0,
                            width: 1.0,
                            height: 1.0,
                        },
                        WindowOptions::default(),
                        None,
                        None,
                        Some(Arc::new(move |view, event| {
                            frame_event(view, event);
                            frame(view, event);
                        })),
                    )
                    .map_err(|error| accepted::InstallError {
                        error: TabError(error.message),
                        close: error.close,
                        proof: error.proof,
                    })
            }),
            done,
        );
    }

    fn cursor_point(&self) -> (f64, f64) {
        let cx = self.cx.clone();
        on_ui(&self.cx.ui, move || {
            let point = cx.app.cursor_position().expect("cursor position");
            let monitors = cx.app.available_monitors().expect("display list");
            let monitor = monitors
                .iter()
                .min_by(|a, b| {
                    let distance = |monitor: &tauri::window::Monitor| {
                        let area = monitor.work_area();
                        let x = point.x.clamp(
                            area.position.x as f64,
                            (area.position.x as f64) + area.size.width as f64,
                        );
                        let y = point.y.clamp(
                            area.position.y as f64,
                            (area.position.y as f64) + area.size.height as f64,
                        );
                        (point.x - x).powi(2) + (point.y - y).powi(2)
                    };
                    distance(a).total_cmp(&distance(b))
                })
                .expect("no displays");
            (
                point.x / monitor.scale_factor(),
                point.y / monitor.scale_factor(),
            )
        })
    }
    fn display_work_area(&self, at: (f64, f64)) -> Rect {
        let cx = self.cx.clone();
        on_ui(&self.cx.ui, move || {
            let monitors = cx.app.available_monitors().expect("display list");
            let monitor = monitors
                .iter()
                .min_by(|a, b| {
                    let distance = |monitor: &tauri::window::Monitor| {
                        let area = monitor_rect(monitor);
                        let x = at.0.clamp(area.x, area.x + area.width);
                        let y = at.1.clamp(area.y, area.y + area.height);
                        (at.0 - x).powi(2) + (at.1 - y).powi(2)
                    };
                    distance(a).total_cmp(&distance(b))
                })
                .expect("no displays");
            monitor_rect(monitor)
        })
    }
    fn set_timer(&self, key: TimerKey, delay_ms: f64) {
        let generation = self.next_timer.fetch_add(1, Ordering::Relaxed);
        let mut timers = self.timers.lock().unwrap();
        timers.retain(|(old, _)| *old != key);
        timers.push((key, generation));
        drop(timers);
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs_f64(
                delay_ms.max(0.0) / 1000.0,
            ));
            if let Some(host) = NATIVE_WINDOWS.get() {
                let mut timers = host.timers.lock().unwrap();
                let Some(index) = timers.iter().position(|entry| *entry == (key, generation))
                else {
                    return;
                };
                timers.remove(index);
                drop(timers);
                host.send(HostEvent::Timer(key));
            }
        });
    }
    fn clear_timer(&self, key: TimerKey) {
        self.timers.lock().unwrap().retain(|(old, _)| *old != key);
    }
    fn popup_menu(
        &self,
        window: WindowId,
        entries: &[MenuEntry],
        at: Option<(f64, f64)>,
        token: u64,
    ) {
        let host = NATIVE_WINDOWS.get().expect("window host is not installed");
        popup(host, window, entries, at, token);
    }
}

fn frame_event(view: ViewId, event: &NativeEvent) {
    super::renderer_disposal::on_frame(view, event);
    if matches!(event.kind, NativeEventKind::WebviewProcessFailed { .. }) {
        hwp_lifecycle(view, true);
    }
    if let Some(host) = NATIVE_WINDOWS.get() {
        host.frame_event(view, event);
    }
}
fn hwp_lifecycle(view: ViewId, crashed: bool) {
    let Some(host) = NATIVE_WINDOWS.get() else {
        return;
    };
    if !host
        .cx
        .views
        .sender_of(view)
        .is_some_and(|sender| sender.surface == Surface::Hwp)
    {
        return;
    }
    let views = host.cx.views.clone();
    super::w::post_core(move |provided, _| {
        if !views
            .sender_of(view)
            .is_some_and(|sender| sender.surface == Surface::Hwp)
        {
            return;
        }
        if let Some(editors) = provided.all::<shell_ipc_d2::hwp::HwpEditors>().first() {
            if crashed {
                editors.render_process_gone(view, "renderer terminated");
            } else {
                editors.did_start_navigation(view, true, false);
            }
        }
    });
}

static POPUPS: LazyLock<Mutex<HashMap<u64, Option<u32>>>> = LazyLock::new(Mutex::default);

pub(crate) fn menu_event(id: &str) -> bool {
    let Some(rest) = id.strip_prefix("tabs-popup:") else {
        return false;
    };
    let Some((token, choice)) = rest.split_once(':') else {
        return false;
    };
    let (Ok(token), Ok(choice)) = (token.parse::<u64>(), choice.parse::<u32>()) else {
        return false;
    };
    if let Some(selected) = POPUPS.lock().unwrap().get_mut(&token) {
        *selected = Some(choice);
        true
    } else {
        false
    }
}

fn menu_item(
    app: &tauri::AppHandle,
    token: u64,
    entry: &MenuEntry,
) -> tauri::Result<tauri::menu::MenuItemKind<tauri::Wry>> {
    use tauri::menu::{CheckMenuItem, MenuItem, MenuItemKind, PredefinedMenuItem, Submenu};
    if !entry.submenu.is_empty() {
        let submenu = Submenu::new(app, &entry.label, entry.enabled)?;
        for child in &entry.submenu {
            submenu.append(&menu_item(app, token, child)?)?;
        }
        return Ok(MenuItemKind::Submenu(submenu));
    }
    let id = format!("tabs-popup:{token}:{}", entry.id);
    match entry.kind {
        MenuEntryKind::Separator => Ok(MenuItemKind::Predefined(PredefinedMenuItem::separator(
            app,
        )?)),
        MenuEntryKind::Checkbox => Ok(MenuItemKind::Check(CheckMenuItem::with_id(
            app,
            id,
            &entry.label,
            entry.enabled,
            entry.checked,
            None::<&str>,
        )?)),
        MenuEntryKind::Normal | MenuEntryKind::Header => {
            Ok(MenuItemKind::MenuItem(MenuItem::with_id(
                app,
                id,
                &entry.label,
                entry.enabled && entry.kind != MenuEntryKind::Header,
                None::<&str>,
            )?))
        }
    }
}

fn popup(
    host: &Arc<NativeWindows>,
    window: WindowId,
    entries: &[MenuEntry],
    at: Option<(f64, f64)>,
    token: u64,
) {
    let Some(events) = host.events.lock().unwrap().clone() else {
        return;
    };
    let app = host.cx.app.clone();
    let label = host.cx.views.label_of_window(window);
    let entries = entries.to_vec();
    POPUPS.lock().unwrap().insert(token, None);
    let app_for_popup = app.clone();
    let result = app.run_on_main_thread(move || {
        let run = || -> Result<(), String> {
            let label = label.ok_or_else(|| format!("no window {}", window.0))?;
            let window = app_for_popup
                .get_window(&label)
                .ok_or_else(|| format!("window {} is gone", window.0))?;
            let menu = tauri::menu::Menu::new(&app_for_popup).map_err(|error| error.to_string())?;
            for entry in &entries {
                menu.append(
                    &menu_item(&app_for_popup, token, entry).map_err(|error| error.to_string())?,
                )
                .map_err(|error| error.to_string())?;
            }
            match at {
                Some((x, y)) => window.popup_menu_at(&menu, tauri::LogicalPosition::new(x, y)),
                None => window.popup_menu(&menu),
            }
            .map_err(|error| error.to_string())
        };
        if let Err(error) = run() {
            eprintln!("shell: tab menu: {error}");
        }
        let app = app_for_popup.clone();
        std::thread::spawn(move || {
            let delivered = events.clone();
            if let Err(error) = app.run_on_main_thread(move || {
                let selected = POPUPS.lock().unwrap().remove(&token).flatten();
                delivered.send(HostEvent::Menu {
                    token,
                    chosen: selected,
                });
            }) {
                eprintln!("shell: tab menu completion: {error}");
                POPUPS.lock().unwrap().remove(&token);
                events.send(HostEvent::Menu {
                    token,
                    chosen: None,
                });
            }
        });
    });
    if let Err(error) = result {
        eprintln!("shell: tab menu: {error}");
        POPUPS.lock().unwrap().remove(&token);
        if let Some(events) = host.events.lock().unwrap().clone() {
            events.send(HostEvent::Menu {
                token,
                chosen: None,
            });
        }
    }
}

/// The window the OS has activated and not deactivated since, where `Focused` alone does not
/// say so.
///
/// Electron's window `focus` is the window's activation. On Windows Tauri makes `Focused` out
/// of the focus of the window's webviews, so it also comes when the focus leaves a webview and
/// returns while the window stays in the foreground. The tabs answer a window focus by
/// focusing the active view. S116 runs show an editor page there receiving window `focus`
/// events several times a second with nothing going on, and an editor that ends an edit when
/// it loses the focus (a slide's text box, a mind map topic) kept none.
static ACTIVE_WINDOW: Mutex<Option<WindowId>> = Mutex::new(None);

/// Whether `Focused(focused)` of `window` is its activation. `foreground`: the OS still has
/// the window in the foreground, so a focus that was lost stayed inside the window.
fn is_activation(
    active: &mut Option<WindowId>,
    window: WindowId,
    focused: bool,
    foreground: bool,
) -> bool {
    if focused {
        // one window is active at a time: the activation of another ends this one's, also
        // when its own loss of the focus was read while it was still in the foreground
        return active.replace(window) != Some(window);
    }
    if !foreground && *active == Some(window) {
        *active = None;
    }
    false
}

pub(crate) fn window_event(window: WindowId, event: &tauri::WindowEvent) {
    let Some(host) = NATIVE_WINDOWS.get() else {
        return;
    };
    let shell = host
        .cx
        .views
        .marked_main()
        .is_some_and(|sender| sender.window == window);
    let detached = host.detached.lock().unwrap().contains(&window);
    if !shell && !detached {
        return;
    }
    let Some(events) = host.events.lock().unwrap().clone() else {
        return;
    };
    match event {
        tauri::WindowEvent::CloseRequested { api, .. } if detached || shell => {
            api.prevent_close();
            events.send(HostEvent::Window(window, WindowEvent::Close));
        }
        tauri::WindowEvent::Destroyed => {
            host.detached.lock().unwrap().remove(&window);
            let mut active = ACTIVE_WINDOW.lock().unwrap();
            if *active == Some(window) {
                *active = None;
            }
            drop(active);
            events.send(HostEvent::Window(window, WindowEvent::Closed));
            // Electron's `closed` handler checks `appQuitting` once, as the
            // window goes; a quit that starts later must not be judged by it
            let closed_into_session = shell && !crate::host::quit::is_quitting();
            if let Some(core) = host.core.get() {
                core.post(Box::new(move || {
                    super::recovery_openers::close_owner(window);
                    if closed_into_session {
                        super::recovery_launch::note_shell_window_closed();
                    }
                }));
            }
        }
        tauri::WindowEvent::Focused(focused) => {
            let activation = if cfg!(windows) {
                let foreground = host.cx.views.label_of_window(window).is_some_and(|label| {
                    crate::native::window::native_app_key_window_handle(&host.cx.app)
                        .ok()
                        .flatten()
                        .is_some_and(|key| key.label() == label.as_str())
                });
                let activation = is_activation(
                    &mut ACTIVE_WINDOW.lock().unwrap(),
                    window,
                    *focused,
                    foreground,
                );
                crate::trace::trace!(
                    "focus",
                    "window {} focused={focused}, in the foreground={foreground}: {}",
                    window.0,
                    if activation {
                        "its activation"
                    } else {
                        "not an activation"
                    }
                );
                activation
            } else {
                *focused
            };
            if activation {
                events.send(HostEvent::Window(window, WindowEvent::Focus));
            }
        }
        tauri::WindowEvent::Moved(_) => {
            events.send(HostEvent::Window(window, WindowEvent::Move));
            if cfg!(target_os = "windows") {
                events.send(HostEvent::Window(window, WindowEvent::Moved));
            }
        }
        tauri::WindowEvent::Resized(_) => {
            events.send(HostEvent::Window(window, WindowEvent::Resize));
        }
        _ => {}
    }
}

fn blank_pdf() -> Vec<u8> {
    let mut pdf = String::from("%PDF-1.4\n");
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>",
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595.28 841.89] /Resources << >> >>",
    ];
    let mut offsets = [0usize; 3];
    for (index, object) in objects.iter().enumerate() {
        offsets[index] = pdf.len();
        write!(pdf, "{} 0 obj\n{}\nendobj\n", index + 1, object).expect("writing to a String");
    }
    let xref = pdf.len();
    pdf.push_str("xref\n0 4\n0000000000 65535 f \n");
    for offset in offsets {
        writeln!(pdf, "{offset:010} 00000 n ").expect("writing to a String");
    }
    write!(
        pdf,
        "trailer\n<< /Size 4 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n"
    )
    .expect("writing to a String");
    pdf.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::{WindowId, blank_pdf, is_activation};

    #[test]
    fn a_focus_that_returns_inside_the_foreground_window_is_no_activation() {
        let (shell, other) = (WindowId(1), WindowId(2));
        let mut active = None;
        assert!(is_activation(&mut active, shell, true, true));
        // the focus left a webview and came back; the window never left the foreground
        assert!(!is_activation(&mut active, shell, false, true));
        assert!(!is_activation(&mut active, shell, true, true));
        // another application took the foreground, then the window came back
        assert!(!is_activation(&mut active, shell, false, false));
        assert!(is_activation(&mut active, shell, true, true));
        // another window of the app is activated while this one's loss was read too early
        assert!(!is_activation(&mut active, shell, false, true));
        assert!(is_activation(&mut active, other, true, true));
        assert!(is_activation(&mut active, shell, true, true));
        // a loss of the focus reported for a window that is not the active one changes nothing
        assert!(!is_activation(&mut active, other, false, false));
        assert!(!is_activation(&mut active, shell, true, true));
    }

    #[test]
    fn blank_pdf_has_one_a4_page_and_valid_cross_reference() {
        let bytes = blank_pdf();
        let text = std::str::from_utf8(&bytes).unwrap();
        assert!(text.contains("/Count 1"));
        assert!(text.contains("/MediaBox [0 0 595.28 841.89]"));
        let xref = text.find("xref\n").unwrap();
        for (index, line) in text[xref..].lines().skip(3).take(3).enumerate() {
            let offset: usize = line[..10].parse().unwrap();
            assert!(text[offset..].starts_with(&format!("{} 0 obj\n", index + 1)));
        }
        assert!(text.contains(&format!("startxref\n{xref}\n")));
    }
}
