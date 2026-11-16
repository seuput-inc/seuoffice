//! The generate window host, including its tabs, settings and conversion engines.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::ipc::native_owner;
use jscompat::{JsonValue, random_uuid};
use main_core::packages::electron_utils::generate_handoff::GenerateHandoff;
use main_core::packages::i18n::Lang;
use main_core::shell::app_routing::DocumentTabKind;
use main_core::shell::app_settings::read_app_settings;
use main_core::shell::generate::convert::{ConvertedDocument, OpOutcome, XmindTopic};
use main_core::shell::generate::generate_api::GenerateWindowState;
use main_core::shell::generate::generate_window::{
    GenerateText, GenerateWindowOptions, NewDocumentOpener,
};
use main_core::shell::generate::materials::ParsedFile;
use main_core::shell::generate::open_materials::{OpenDocumentTab, SaveAnswer, SaveNeed};
use main_core::shell::renderer_disposal::Failure;
use main_core::shell::title_bar_overlay::TitleBarOverlay;
use shell_contract::hosts::OwnedEffectsHost;
use shell_contract::hosts::{DialogsHost, FileDialog, OsShellHost};
use shell_contract::ipc::EventSink;
use shell_contract::ipc::NativeRequestOwner;
use shell_contract::runtime::{CancelToken, RendererGeneration};
use shell_contract::{Hosts, ViewId, WindowId};
use shell_ipc_a::Factory;
use shell_ipc_a::generate::host::{
    GenerateAuthority, GenerateCommittedOpen, GenerateCommittedTarget, GenerateOpenRequest,
    GenerateOpenResult, GenerateShell, GenerateShellHost, GenerateWindowCleanup, TabContents,
};
use shell_ipc_a::generate::{GenerateWindowEvents, GenerateWindowPort};
use shell_ipc_a::ports::GenerateWindow as GenerateWindowChannels;
use std::path::PathBuf;

#[path = "generate_effects.rs"]
mod effects;

use super::{current_lang, generate_outline, generate_window::GenerateWindows, now_ms};
use crate::host::HostContext;
use crate::host::menu::attach_make_window;
use crate::paths::ShellPaths;

pub(crate) fn queue_generate_handoff(
    provided: &shell_contract::Provided,
    view: ViewId,
    value: &serde_json::Value,
) -> Result<(), String> {
    let handoff: GenerateHandoff =
        jscompat::serde::Deserialize::deserialize(value).map_err(|error| error.to_string())?;
    shell_ipc_a::user_skills::shared_handoff_store(provided)
        .borrow_mut()
        .queue_generate_handoff(f64::from(view.0), handoff);
    Ok(())
}
type ClosureReceiptHook =
    Box<dyn Fn(&shell_contract::ipc::NativeClosureReceipt) -> Result<(), String>>;
type SupersededHook =
    Box<dyn Fn(&shell_contract::ipc::NativeBrowserSuperseded) -> Result<(), String>>;
struct SeedRollback {
    rollback: Box<dyn Fn() -> Result<(), String>>,
    retire: ClosureReceiptHook,
    adopted: ClosureReceiptHook,
    superseded: SupersededHook,
}
impl shell_ipc_w::tabs::host::EditorSeedRollback for SeedRollback {
    fn rollback(&self, done: Box<dyn FnOnce(Result<(), String>)>) {
        done((self.rollback)())
    }
    fn retire_closed(
        &self,
        proof: shell_contract::ipc::NativeClosureReceipt,
        done: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        done((self.retire)(&proof))
    }
    fn retire_adopted(
        &self,
        proof: shell_contract::ipc::NativeClosureReceipt,
        done: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        done((self.adopted)(&proof))
    }
    fn retire_superseded(
        &self,
        proof: shell_contract::ipc::NativeBrowserSuperseded,
        done: Box<dyn FnOnce(Result<(), String>)>,
    ) {
        done((self.superseded)(&proof))
    }
}
fn seed_rollback(
    rollback: impl Fn() -> Result<(), String> + 'static,
    retire: impl Fn(&shell_contract::ipc::NativeClosureReceipt) -> Result<(), String> + 'static,
    adopted: impl Fn(&shell_contract::ipc::NativeClosureReceipt) -> Result<(), String> + 'static,
    superseded: impl Fn(&shell_contract::ipc::NativeBrowserSuperseded) -> Result<(), String> + 'static,
) -> std::rc::Rc<dyn shell_ipc_w::tabs::host::EditorSeedRollback> {
    std::rc::Rc::new(SeedRollback {
        rollback: Box::new(rollback),
        retire: Box::new(retire),
        adopted: Box::new(adopted),
        superseded: Box::new(superseded),
    })
}

pub(crate) struct PreparedGeneratedSeed {
    commit: PreparedConversionActor,
    receipt: std::rc::Rc<dyn shell_ipc_w::tabs::host::EditorSeedRollback>,
}

impl PreparedGeneratedSeed {
    pub(crate) fn receipt(&self) -> std::rc::Rc<dyn shell_ipc_w::tabs::host::EditorSeedRollback> {
        self.receipt.clone()
    }
    pub(crate) fn commit(
        self,
        transition: &shell_contract::ipc::NativeBrowserTransition<'_>,
        initialize: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<std::rc::Rc<dyn shell_ipc_w::tabs::host::EditorSeedRollback>, String> {
        (self.commit)(transition, initialize)?;
        Ok(self.receipt)
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_generate_handoff_owned(
    provided: &shell_contract::Provided,
    view: ViewId,
    value: &serde_json::Value,
    source: &NativeRequestOwner,
    created: &RendererGeneration,
    reservation: &shell_contract::ipc::NativeClosureReservation,
    browser: &shell_contract::ipc::NativeBrowserOwner,
    effects: OwnedEffectsHost,
) -> Result<PreparedGeneratedSeed, String> {
    let handoff: GenerateHandoff =
        jscompat::serde::Deserialize::deserialize(value).map_err(|error| error.to_string())?;
    let prepared = shell_ipc_a::user_skills::prepare_generate_handoff(
        provided,
        view,
        handoff,
        source.clone(),
        created.clone(),
        reservation.clone(),
        browser.clone(),
        effects,
    )?;
    let slot = std::rc::Rc::new(RefCell::new(
        None::<shell_ipc_a::user_skills::GenerateRollback>,
    ));
    let cancelled = std::rc::Rc::new(std::cell::Cell::new(false));
    let rollback = slot.clone();
    let closing = slot.clone();
    let adopted = slot.clone();
    let superseded = slot.clone();
    let rollback_cancelled = cancelled.clone();
    let receipt = seed_rollback(
        move || match rollback.borrow().as_ref() {
            Some(receipt) => receipt.rollback(),
            None => {
                rollback_cancelled.set(true);
                Ok(())
            }
        },
        move |proof| {
            closing
                .borrow()
                .as_ref()
                .ok_or("The generate receipt is not committed")?
                .retire_closed(proof)
        },
        move |proof| {
            adopted
                .borrow()
                .as_ref()
                .ok_or("The generate receipt is not committed")?
                .retire_adopted(proof)
        },
        move |proof| {
            superseded
                .borrow()
                .as_ref()
                .ok_or("The generate receipt is not committed")?
                .retire_superseded(proof)
        },
    );
    Ok(PreparedGeneratedSeed {
        receipt,
        commit: Box::new(move |transition, initialize| {
            if cancelled.get() {
                return Err("The generate seed was cancelled before publication".into());
            }
            *slot.borrow_mut() = Some(prepared.commit(transition, initialize)?);
            Ok(())
        }),
    })
}

pub(crate) fn drop_generate_handoff(provided: &shell_contract::Provided, view_id: i64) {
    shell_ipc_a::user_skills::shared_handoff_store(provided)
        .borrow_mut()
        .drop_legacy_handoff(view_id as f64);
}

enum ConversionSeed {
    MindmapToMarkdown(String),
    MindmapToSlides(Vec<u8>),
    MarkdownToMindmap(String),
}

struct PreparedConversion {
    id: String,
    sources: Vec<String>,
    seed: ConversionSeed,
}

impl PreparedConversion {
    fn parse(doc: &serde_json::Value) -> Result<Self, String> {
        let content = doc
            .get("content")
            .and_then(serde_json::Value::as_object)
            .ok_or("missing conversion content")?;
        let id = doc
            .get("id")
            .and_then(serde_json::Value::as_str)
            .ok_or("missing conversion id")?
            .to_owned();
        let sources = doc
            .get("sources")
            .and_then(serde_json::Value::as_array)
            .ok_or("missing conversion sources")?
            .iter()
            .map(|source| {
                source
                    .as_str()
                    .map(str::to_owned)
                    .ok_or("invalid conversion source")
            })
            .collect::<Result<Vec<_>, _>>()?;
        let target = content
            .get("target")
            .and_then(serde_json::Value::as_str)
            .ok_or("missing conversion target")?;
        let kind = content
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .ok_or("missing conversion kind")?;
        let seed = match (target, kind) {
            ("markdown", "mindmap-to-markdown") => ConversionSeed::MindmapToMarkdown(
                content
                    .get("markdown")
                    .and_then(serde_json::Value::as_str)
                    .ok_or("missing converted markdown")?
                    .to_owned(),
            ),
            ("slides", "mindmap-to-slides") => ConversionSeed::MindmapToSlides(
                content
                    .get("deck")
                    .and_then(serde_json::Value::as_array)
                    .ok_or("missing converted deck")?
                    .iter()
                    .map(|byte| {
                        byte.as_u64()
                            .and_then(|value| u8::try_from(value).ok())
                            .ok_or("invalid deck byte")
                    })
                    .collect::<Result<Vec<_>, _>>()?,
            ),
            ("mindmap", "markdown-to-mindmap") => ConversionSeed::MarkdownToMindmap(
                content
                    .get("outline")
                    .and_then(serde_json::Value::as_str)
                    .ok_or("missing converted outline")?
                    .to_owned(),
            ),
            _ => return Err("invalid conversion target or kind".into()),
        };
        Ok(Self { id, sources, seed })
    }

    fn metadata(
        &self,
    ) -> (
        main_core::packages::electron_utils::generate_handoff::GenerateConvertTarget,
        main_core::packages::electron_utils::generate_handoff::GenerateConvertKind,
    ) {
        use main_core::packages::electron_utils::generate_handoff::{
            GenerateConvertKind as Kind, GenerateConvertTarget as Target,
        };
        match &self.seed {
            ConversionSeed::MindmapToMarkdown(_) => (Target::Markdown, Kind::MindmapToMarkdown),
            ConversionSeed::MindmapToSlides(_) => (Target::Slides, Kind::MindmapToSlides),
            ConversionSeed::MarkdownToMindmap(_) => (Target::Mindmap, Kind::MarkdownToMindmap),
        }
    }
}

pub(crate) fn queue_converted_content(
    provided: &shell_contract::Provided,
    view: ViewId,
    doc: &serde_json::Value,
) -> Result<(), String> {
    let prepared = PreparedConversion::parse(doc)?;
    let (target, kind) = prepared.metadata();
    match prepared.seed {
        ConversionSeed::MindmapToMarkdown(text) => provided
            .all::<shell_ipc_d2::markdown::MarkdownEditors>()
            .into_iter()
            .next()
            .ok_or("markdown editor is unavailable")?
            .queue_seed(view, &text),
        ConversionSeed::MindmapToSlides(bytes) => shell_ipc_d1::slides::views()
            .ok_or("slides editor is unavailable")?
            .queue_seed(view, &bytes, ""),
        ConversionSeed::MarkdownToMindmap(outline) => shell_ipc_d1::mindmap::handle()
            .ok_or("mindmap editor is unavailable")?
            .queue_seed(view, shell_ipc_d1::mindmap::Seed::Outline(outline)),
    }
    shell_ipc_a::user_skills::shared_handoff_store(provided)
        .borrow_mut()
        .record_generate_converted(
            f64::from(view.0),
            target,
            kind,
            &prepared.sources,
            &prepared.id,
            now_ms(),
        )
        .map(|_| ())
        .map_err(|error| error.to_string())
}

type ConversionActorReceipt = (
    Box<dyn Fn() -> Result<(), String>>,
    Box<dyn Fn() -> bool>,
    Box<dyn Fn(&shell_contract::ipc::NativeClosureReceipt) -> Result<(), String>>,
    Box<dyn Fn(&shell_contract::ipc::NativeClosureReceipt) -> Result<(), String>>,
    Box<dyn Fn(&shell_contract::ipc::NativeBrowserSuperseded) -> Result<(), String>>,
);
type PreparedConversionActor = Box<
    dyn for<'a> FnOnce(
        &shell_contract::ipc::NativeBrowserTransition<'a>,
        &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String>,
>;

#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_converted_content_owned(
    provided: &shell_contract::Provided,
    view: ViewId,
    doc: &serde_json::Value,
    source: &NativeRequestOwner,
    cancel: &CancelToken,
    created: &RendererGeneration,
    reservation: &shell_contract::ipc::NativeClosureReservation,
    browser: &shell_contract::ipc::NativeBrowserOwner,
    effects: OwnedEffectsHost,
) -> Result<PreparedGeneratedSeed, String> {
    let prepared = PreparedConversion::parse(doc)?;
    let (target, kind) = prepared.metadata();
    let PreparedConversion { id, sources, seed } = prepared;
    let metadata = shell_ipc_a::user_skills::prepare_generate_converted(
        provided,
        view,
        target,
        kind,
        &sources,
        &id,
        now_ms(),
        source.clone(),
        created.clone(),
        reservation.clone(),
        browser.clone(),
        effects,
    )?;
    let (actor, actor_receipt): (PreparedConversionActor, ConversionActorReceipt) = match seed {
        ConversionSeed::MindmapToMarkdown(text) => {
            let mut prepared = provided
                .all::<shell_ipc_d2::markdown::MarkdownEditors>()
                .into_iter()
                .next()
                .ok_or("markdown editor is unavailable")?
                .prepare_seed_in_transition(
                    view,
                    text,
                    source.clone(),
                    created.clone(),
                    reservation.clone(),
                    browser.clone(),
                )?;
            let receipt = prepared.receipt();
            let observed = receipt.clone();
            let closing = receipt.clone();
            let adopted = receipt.clone();
            let superseded = receipt.clone();
            (
                Box::new(move |transition, initialize| {
                    prepared.commit(transition, initialize).map(|_| ())
                }),
                (
                    Box::new(move || receipt.rollback()),
                    Box::new(move || observed.was_consumed()),
                    Box::new(move |proof| closing.retire_closed(proof)),
                    Box::new(move |proof| adopted.retire_adopted(proof)),
                    Box::new(move |proof| superseded.retire_superseded(proof)),
                ),
            )
        }
        ConversionSeed::MindmapToSlides(bytes) => {
            let prepared = shell_ipc_d1::slides::views()
                .ok_or("slides editor is unavailable")?
                .prepare_seed_in_transition(
                    view,
                    bytes,
                    String::new(),
                    source.clone(),
                    cancel.clone(),
                    created.clone(),
                    reservation.clone(),
                    browser.clone(),
                )?;
            let receipt = prepared.receipt();
            let observed = receipt.clone();
            let closing = receipt.clone();
            let adopted = receipt.clone();
            let superseded = receipt.clone();
            (
                Box::new(move |transition, initialize| {
                    prepared.commit(transition, initialize).map(|_| ())
                }),
                (
                    Box::new(move || receipt.rollback()),
                    Box::new(move || observed.was_consumed()),
                    Box::new(move |proof| closing.retire_closed(proof)),
                    Box::new(move |proof| adopted.retire_adopted(proof)),
                    Box::new(move |proof| superseded.retire_superseded(proof)),
                ),
            )
        }
        ConversionSeed::MarkdownToMindmap(outline) => {
            let prepared = shell_ipc_d1::mindmap::handle()
                .ok_or("mindmap editor is unavailable")?
                .prepare_seed_in_transition(
                    view,
                    shell_ipc_d1::mindmap::Seed::Outline(outline),
                    source.clone(),
                    cancel.clone(),
                    created.clone(),
                    reservation.clone(),
                    browser.clone(),
                )?;
            let receipt = prepared.receipt();
            let observed = receipt.clone();
            let closing = receipt.clone();
            let adopted = receipt.clone();
            let superseded = receipt.clone();
            (
                Box::new(move |transition, initialize| {
                    prepared.commit(transition, initialize).map(|_| ())
                }),
                (
                    Box::new(move || receipt.rollback()),
                    Box::new(move || observed.was_consumed()),
                    Box::new(move |proof| closing.retire_closed(proof)),
                    Box::new(move |proof| adopted.retire_adopted(proof)),
                    Box::new(move |proof| superseded.retire_superseded(proof)),
                ),
            )
        }
    };
    let actor_receipt = std::rc::Rc::new(actor_receipt);
    let closing_actor = actor_receipt.clone();
    let adopted_actor = actor_receipt.clone();
    let superseded_actor = actor_receipt.clone();
    let metadata_receipt = std::rc::Rc::new(RefCell::new(
        None::<shell_ipc_a::user_skills::ConvertedRollback>,
    ));
    let cancelled = std::rc::Rc::new(std::cell::Cell::new(false));
    let rollback_cancelled = cancelled.clone();
    let rollback_metadata = metadata_receipt.clone();
    let closing_metadata = metadata_receipt.clone();
    let adopted_metadata = metadata_receipt.clone();
    let superseded_metadata = metadata_receipt.clone();
    let receipt = seed_rollback(
        move || {
            (actor_receipt.0)()?;
            if (actor_receipt.1)() {
                Ok(())
            } else {
                match rollback_metadata.borrow().as_ref() {
                    Some(receipt) => receipt.rollback(),
                    None => {
                        rollback_cancelled.set(true);
                        Ok(())
                    }
                }
            }
        },
        move |proof| {
            (closing_actor.2)(proof)?;
            closing_metadata
                .borrow()
                .as_ref()
                .ok_or("The converted receipt is not committed")?
                .retire_closed(proof)
        },
        move |proof| {
            let consumed = (adopted_actor.1)();
            (adopted_actor.3)(proof)?;
            adopted_metadata
                .borrow()
                .as_ref()
                .ok_or("The converted receipt is not committed")?
                .retire_adopted(proof, consumed)
        },
        move |proof| {
            (superseded_actor.4)(proof)?;
            superseded_metadata
                .borrow()
                .as_ref()
                .ok_or("The converted receipt is not committed")?
                .retire_superseded(proof)
        },
    );
    Ok(PreparedGeneratedSeed {
        receipt,
        commit: Box::new(move |transition, initialize| {
            if cancelled.get() {
                return Err("The converted seed was cancelled before publication".into());
            }
            let mut metadata = Some(metadata);
            let mut commit_metadata = || {
                let plan = metadata
                    .take()
                    .ok_or("converted metadata initializer repeated")?;
                *metadata_receipt.borrow_mut() = Some(plan.commit(transition, initialize)?);
                Ok(())
            };
            actor(transition, &mut commit_metadata)
        }),
    })
}

pub(super) fn install(hosts: &mut Hosts, cx: &HostContext) {
    let shell: GenerateShellHost = Arc::new(ShellGenerate {
        cx: cx.clone(),
        windows: GenerateWindows::new(cx),
    });
    hosts.insert(shell);
    // the port, then, on the same core thread, its hand-off store and make
    // window handed to the menus' document links (M)
    let build = shell_ipc_a::generate::factory().0;
    hosts.insert(Factory::<dyn GenerateWindowChannels>::new(
        move |services, provided| {
            let port = build(services, provided)?;
            if let Some(window) = provided.all::<GenerateWindowPort>().into_iter().next() {
                attach_make_window(window.handoff_store(), window.make_window_opener());
            }
            Ok(port)
        },
    ));
}

struct ShellGenerate {
    cx: HostContext,
    windows: GenerateWindows,
}

impl ShellGenerate {
    fn worker_authority(&self, authority: &GenerateAuthority) -> Result<(), String> {
        if self.cx.ui.is_current() || crate::host::w::on_core_local(|_| ()) {
            return Err("Generation engines require the owned worker, not UI/core.".into());
        }
        self.check_authority(authority)
    }
}

struct OwnedDeck {
    authority: GenerateAuthority,
    deck: pptx_engine::deck::OpenedPptx,
}

thread_local! {
    static DECKS: RefCell<(u64, HashMap<u64, OwnedDeck>)> =
        RefCell::new((0, HashMap::new()));
}

fn take_deck(authority: &GenerateAuthority, id: u64) -> Result<OwnedDeck, String> {
    DECKS.with(|decks| {
        let mut decks = decks.borrow_mut();
        let entry = decks.1.get(&id).ok_or("deck not found")?;
        if !effects::same_request(&entry.authority, authority) {
            return Err("deck belongs to another generation request".into());
        }
        decks.1.remove(&id).ok_or_else(|| "deck disappeared".into())
    })
}

fn generate_document_tab(
    tab: main_core::shell::tab_manager::OpenDocumentTab,
) -> Option<OpenDocumentTab> {
    use main_core::shell::tab_manager::TabKind;
    let kind = match tab.kind {
        TabKind::Docs => DocumentTabKind::Docs,
        TabKind::Sheets => DocumentTabKind::Sheets,
        TabKind::Slides => DocumentTabKind::Slides,
        TabKind::Pdf => DocumentTabKind::Pdf,
        TabKind::Markdown => DocumentTabKind::Markdown,
        TabKind::Html => DocumentTabKind::Html,
        TabKind::Mindmap => DocumentTabKind::Mindmap,
        TabKind::Hwp => DocumentTabKind::Hwp,
        TabKind::Epub => DocumentTabKind::Epub,
        TabKind::Diagram => DocumentTabKind::Diagram,
        TabKind::Home | TabKind::Help => return None,
    };
    Some(OpenDocumentTab {
        id: tab.id,
        kind,
        title: tab.title,
        file_path: tab.file_path,
        active: tab.active,
        dirty: tab.dirty,
    })
}

/// The generate window names its opener with a float id; the tab host with an integer one
fn tab_opener(
    opener: Option<NewDocumentOpener>,
) -> Option<main_core::shell::projects::new_documents::NewDocumentOpener> {
    use main_core::shell::projects::new_documents::NewDocumentOpener as Tab;
    opener.map(|opener| match opener {
        NewDocumentOpener::Home => Tab::Home,
        NewDocumentOpener::Window { web_contents_id } => Tab::Window {
            web_contents_id: web_contents_id as i64,
        },
    })
}

fn failed_save(to_path: bool, message: String) -> SaveAnswer {
    let error = Failure(message);
    if to_path {
        SaveAnswer::ToPath(Err(error))
    } else {
        SaveAnswer::Editor(Err(error))
    }
}

struct NativeCommittedTarget(shell_ipc_w::tabs::CommittedGeneratedTab);

impl GenerateCommittedTarget for NativeCommittedTarget {
    fn created(&self) -> &RendererGeneration {
        self.0.created()
    }
    fn validate_target(&self) -> Result<(), String> {
        self.0.validate_target()
    }
    fn with_terminal_commit(
        &self,
        effect: &mut dyn FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        self.0.with_terminal_commit(effect)
    }
}

fn open_generated_tab(
    authority: &GenerateAuthority,
    editor: shell_ipc_w::tabs::GeneratedEditorOpen,
    request: GenerateOpenRequest,
    cleanup: GenerateWindowCleanup,
    then: Box<dyn FnOnce(GenerateOpenResult) + Send>,
) {
    let owner = match effects::owner(authority) {
        Ok(owner) => owner,
        Err(error) => return then(Err(error)),
    };
    let cancel = authority.cancel.clone();
    let mut then = Some(then);
    let result = effects::on_core(|provided| {
        let tabs = provided
            .all::<shell_ipc_w::tabs::TabsState>()
            .into_iter()
            .next()
            .ok_or("tab manager is unavailable")?;
        let done = then
            .take()
            .ok_or("tab open continuation already consumed")?;
        tabs.open_generated_editor_owned(
            editor,
            owner,
            cancel,
            Box::new(move |answer| {
                // Only the actual constructor/queue/load receipt consumes the core
                // token. Source closure after transfer must not re-authorize it.
                done(answer.and_then(|target| {
                    GenerateCommittedOpen::new(
                        request.token,
                        Box::new(NativeCommittedTarget(target)),
                        cleanup,
                    )
                }));
            }),
        );
        Ok(())
    });
    if let Err(error) = result
        && let Some(then) = then
    {
        then(Err(error));
    }
}

impl GenerateShell for ShellGenerate {
    fn check_authority(&self, authority: &GenerateAuthority) -> Result<(), String> {
        self.windows.check_authority(authority)?;
        if authority.is_closed_event() {
            return Ok(());
        }
        effects::owner(authority).map(|_| ())
    }

    fn show_open_dialog(
        &self,
        authority: &GenerateAuthority,
        dialogs: DialogsHost,
        parent: WindowId,
        options: &FileDialog,
    ) -> Result<Option<Vec<PathBuf>>, String> {
        self.check_authority(authority)?;
        let owner = effects::owner(authority)?;
        let cancel = authority.cancel.clone();
        let ui = self.cx.ui.clone();
        let options = options.clone();
        let picked = crate::host::on_ui(&self.cx.ui, move || {
            native_owner::run_accepted_ui(ui.as_ref(), &owner, &cancel, || {
                dialogs.open(Some(parent), &options)
            })
        })?;
        self.check_authority(authority)?;
        Ok(picked)
    }

    fn open_external(
        &self,
        authority: &GenerateAuthority,
        os_shell: OsShellHost,
        url: &str,
    ) -> Result<(), String> {
        self.check_authority(authority)?;
        let owner = effects::owner(authority)?;
        let cancel = authority.cancel.clone();
        let ui = self.cx.ui.clone();
        let url = url.to_owned();
        crate::host::on_ui(&self.cx.ui, move || {
            native_owner::run_accepted_ui(ui.as_ref(), &owner, &cancel, || {
                os_shell.open_external(&url)
            })
        })?
    }

    fn send_state(
        &self,
        authority: &GenerateAuthority,
        events: Arc<dyn EventSink>,
        view: ViewId,
        state: &GenerateWindowState,
    ) -> Result<(), String> {
        self.check_authority(authority)?;
        let target = self.windows.delivery_owner(authority, view)?;
        let value = serde_json::to_value(state).map_err(|error| error.to_string())?;
        let args = vec![shell_ipc_a::generate::ipc_of(&value)];
        // EventSink takes its own target-generation fence. Do not recursively
        // acquire that lease while holding the source's authority fence.
        let deliver = effects::commit(authority, || {
            move || events.emit_to_owner_checked(&target, "generate:state", args)
        })?;
        deliver()
    }

    fn create_window(
        &self,
        authority: &GenerateAuthority,
        options: &GenerateWindowOptions,
        events: GenerateWindowEvents,
    ) -> Result<WindowId, String> {
        self.windows.create(authority, options, events)
    }

    fn window_view(
        &self,
        authority: &GenerateAuthority,
        window: WindowId,
    ) -> Result<Option<ViewId>, String> {
        self.windows.view(authority, window)
    }

    fn window_is_destroyed(
        &self,
        authority: &GenerateAuthority,
        window: WindowId,
    ) -> Result<bool, String> {
        self.windows.is_destroyed(authority, window)
    }

    fn window_is_minimized(
        &self,
        authority: &GenerateAuthority,
        window: WindowId,
    ) -> Result<bool, String> {
        self.windows.is_minimized(authority, window)
    }

    fn window_restore(
        &self,
        authority: &GenerateAuthority,
        window: WindowId,
    ) -> Result<(), String> {
        self.windows.restore(authority, window)
    }

    fn window_show(&self, authority: &GenerateAuthority, window: WindowId) -> Result<(), String> {
        self.windows.show(authority, window)
    }

    fn window_focus(&self, authority: &GenerateAuthority, window: WindowId) -> Result<(), String> {
        self.windows.focus(authority, window)
    }

    fn window_close(&self, authority: &GenerateAuthority, window: WindowId) -> Result<(), String> {
        self.windows.close(authority, window)
    }

    fn window_remove_menu(
        &self,
        authority: &GenerateAuthority,
        window: WindowId,
    ) -> Result<(), String> {
        self.windows.remove_menu(authority, window)
    }

    fn window_set_title_bar_overlay(
        &self,
        authority: &GenerateAuthority,
        window: WindowId,
        overlay: &TitleBarOverlay,
    ) -> Result<(), String> {
        self.windows.update_caption(authority, window, overlay)
    }

    fn watch_theme(&self, authority: &GenerateAuthority, window: WindowId) -> Result<(), String> {
        self.windows.watch_theme(authority, window, true)
    }

    fn unwatch_theme(&self, authority: &GenerateAuthority, window: WindowId) -> Result<(), String> {
        self.windows.watch_theme(authority, window, false)
    }

    /// `process.env.ELECTRON_RENDERER_URL`: only the Electron dev server sets it
    fn renderer_url(&self) -> Option<String> {
        std::env::var("ELECTRON_RENDERER_URL").ok()
    }

    fn window_load_url(
        &self,
        authority: &GenerateAuthority,
        window: WindowId,
        url: &str,
    ) -> Result<(), String> {
        self.windows.load(authority, window, url)
    }

    fn window_load_generate_page(
        &self,
        authority: &GenerateAuthority,
        window: WindowId,
    ) -> Result<(), String> {
        self.windows
            .load(authority, window, "genoffice-app://shell/generate.html")
    }

    fn should_use_dark_colors(&self, authority: &GenerateAuthority) -> Result<bool, String> {
        self.windows.dark_colors(authority)
    }

    // ---- `GenerateWindowDeps` ----

    fn lang(&self) -> Lang {
        current_lang()
    }

    /// `currentTheme()`: the saved choice, else `system`
    fn theme(&self) -> String {
        let path = ShellPaths::new().settings_file();
        let settings = read_app_settings(&path.to_string_lossy());
        match settings.get("theme") {
            Some(JsonValue::String(theme)) if theme == "light" || theme == "dark" => theme.clone(),
            _ => "system".to_owned(),
        }
    }

    fn generate_text(&self, lang: Lang, key: GenerateText) -> String {
        match (lang, key) {
            (Lang::En, GenerateText::WindowTitle) => "Make a new document",
            (Lang::En, GenerateText::DialogAddTitle) => "Add sources",
            (Lang::En, GenerateText::FilterSupported) => "Supported files",
            (Lang::En, GenerateText::Untitled) => "Untitled",
            (Lang::Ko, GenerateText::WindowTitle) => "새 문서 만들기",
            (Lang::Ko, GenerateText::DialogAddTitle) => "자료 더하기",
            (Lang::Ko, GenerateText::FilterSupported) => "지원하는 파일",
            (Lang::Ko, GenerateText::Untitled) => "제목 없음",
        }
        .to_owned()
    }

    fn starred_group_paths(&self, group: &str) -> Vec<String> {
        let path = ShellPaths::new()
            .settings_file()
            .with_file_name("starred.json");
        main_core::docs::starred_files::read_starred_items(&path.to_string_lossy())
            .into_iter()
            .filter(|item| item.group.as_deref() == Some(group))
            .map(|item| item.path)
            .collect()
    }

    fn tab_id_for(&self, web_contents_id: f64) -> Option<String> {
        if !web_contents_id.is_finite()
            || web_contents_id.fract() != 0.0
            || !(0.0..=u32::MAX as f64).contains(&web_contents_id)
        {
            return None;
        }
        let view = ViewId(web_contents_id as u32);
        effects::on_core(|provided| {
            Ok(
                crate::host::shell_tabs::shell_tabs(provided)
                    .and_then(|tabs| tabs.tab_id_for(view)),
            )
        })
        .ok()
        .flatten()
    }

    fn open_target_tab(
        &self,
        authority: &GenerateAuthority,
        handoff: GenerateHandoff,
        opener: Option<NewDocumentOpener>,
        request: GenerateOpenRequest,
        then: Box<dyn FnOnce(GenerateOpenResult) + Send>,
    ) {
        if let Err(error) = self.check_authority(authority) {
            return then(Err(error));
        }
        let cleanup = match self
            .windows
            .prepare_committed_cleanup(authority, request.window)
        {
            Ok(cleanup) => cleanup,
            Err(error) => return then(Err(error)),
        };
        use main_core::packages::electron_utils::generate_handoff::GenerateHandoffTarget as Target;
        let target = match &handoff.target {
            Target::Slides => Target::Slides,
            Target::Html => Target::Html,
            Target::Mindmap => Target::Mindmap,
        };
        let value = match serde_json::to_value(handoff) {
            Ok(value) => value,
            Err(error) => return then(Err(error.to_string())),
        };
        let editor = shell_ipc_w::tabs::GeneratedEditorOpen::Generate {
            target,
            handoff: value,
            opener: tab_opener(opener),
        };
        open_generated_tab(authority, editor, request, cleanup, then)
    }

    fn can_open_converted(&self) -> bool {
        true
    }

    fn open_converted_tab(
        &self,
        authority: &GenerateAuthority,
        doc: ConvertedDocument,
        opener: Option<NewDocumentOpener>,
        request: GenerateOpenRequest,
        then: Box<dyn FnOnce(GenerateOpenResult) + Send>,
    ) {
        if let Err(error) = self.check_authority(authority) {
            return then(Err(error));
        }
        let cleanup = match self
            .windows
            .prepare_committed_cleanup(authority, request.window)
        {
            Ok(cleanup) => cleanup,
            Err(error) => return then(Err(error)),
        };
        use main_core::packages::electron_utils::generate_handoff::GenerateConvertTarget as Target;
        use main_core::shell::generate::convert::ConvertedContent;
        let (target, content) = match &doc.content {
            ConvertedContent::MindmapToMarkdown { markdown } => (
                Target::Markdown,
                serde_json::json!({
                    "kind": "mindmap-to-markdown", "target": "markdown", "markdown": markdown
                }),
            ),
            ConvertedContent::MindmapToSlides { deck, slide_count } => (
                Target::Slides,
                serde_json::json!({
                    "kind": "mindmap-to-slides", "target": "slides", "deck": deck, "slideCount": slide_count
                }),
            ),
            ConvertedContent::MarkdownToMindmap { outline } => (
                Target::Mindmap,
                serde_json::json!({
                    "kind": "markdown-to-mindmap", "target": "mindmap", "outline": outline
                }),
            ),
        };
        let value = serde_json::json!({
            "id": doc.id, "content": content, "sources": doc.sources, "createdAt": doc.created_at
        });
        let editor = shell_ipc_w::tabs::GeneratedEditorOpen::Conversion {
            target,
            document: value,
            opener: tab_opener(opener),
        };
        open_generated_tab(authority, editor, request, cleanup, then)
    }

    // ---- `deps.openMaterials` ----

    fn open_documents(
        &self,
        authority: &GenerateAuthority,
        then: Box<dyn FnOnce(Result<Vec<OpenDocumentTab>, String>) + Send>,
    ) {
        let owner = match self
            .check_authority(authority)
            .and_then(|_| effects::owner(authority))
        {
            Ok(owner) => owner,
            Err(error) => return then(Err(error)),
        };
        let authority = authority.clone();
        let mut then = Some(then);
        let result = effects::on_core(|provided| {
            let tabs = provided
                .all::<shell_ipc_w::tabs::TabsState>()
                .into_iter()
                .next()
                .ok_or("tab manager is unavailable")?;
            let done = then
                .take()
                .ok_or("document list continuation already consumed")?;
            tabs.open_documents_owned(
                owner,
                authority.cancel.clone(),
                Box::new(move |result| {
                    let result = result.and_then(|tabs| {
                        effects::owner(&authority)?;
                        Ok(tabs.into_iter().filter_map(generate_document_tab).collect())
                    });
                    done(result);
                }),
            );
            Ok(())
        });
        if let Err(error) = result
            && let Some(then) = then
        {
            then(Err(error));
        }
    }

    fn tab_contents(
        &self,
        authority: &GenerateAuthority,
        tab_id: &str,
    ) -> Result<Option<TabContents>, String> {
        self.check_authority(authority)?;
        let view = effects::on_core(|provided| {
            let tabs = provided
                .all::<shell_ipc_w::tabs::TabsState>()
                .into_iter()
                .next()
                .ok_or("tab manager is unavailable")?;
            Ok(tabs.tab_contents(tab_id))
        })?;
        Ok(view.map(|view| TabContents {
            view,
            destroyed: self.cx.views.label_of_view(view).is_none(),
        }))
    }

    fn reveal_tab(
        &self,
        authority: &GenerateAuthority,
        tab_id: String,
        then: Box<dyn FnOnce(Result<(), String>) + Send>,
    ) {
        let owner = match self
            .check_authority(authority)
            .and_then(|_| effects::owner(authority))
        {
            Ok(owner) => owner,
            Err(error) => return then(Err(error)),
        };
        let cancel = authority.cancel.clone();
        let mut then = Some(then);
        let result = effects::on_core(|provided| {
            let tabs = provided
                .all::<shell_ipc_w::tabs::TabsState>()
                .into_iter()
                .next()
                .ok_or("tab manager is unavailable")?;
            let done = then
                .take()
                .ok_or("tab reveal continuation already consumed")?;
            tabs.reveal_tab_owned(&tab_id, owner, cancel, done);
            Ok(())
        });
        if let Err(error) = result
            && let Some(then) = then
        {
            then(Err(error));
        }
    }

    fn has_save(&self, kind: DocumentTabKind) -> bool {
        matches!(
            kind,
            DocumentTabKind::Docs
                | DocumentTabKind::Sheets
                | DocumentTabKind::Slides
                | DocumentTabKind::Markdown
                | DocumentTabKind::Html
                | DocumentTabKind::Mindmap
        )
    }

    fn save(
        &self,
        authority: &GenerateAuthority,
        need: SaveNeed,
        then: Box<dyn FnOnce(SaveAnswer) + Send>,
    ) {
        let to_path = matches!(&need, SaveNeed::ToPath { .. });
        let owner = match self
            .check_authority(authority)
            .and_then(|_| effects::owner(authority))
        {
            Ok(owner) => owner,
            Err(error) => return then(failed_save(to_path, error)),
        };
        let authority = authority.clone();
        let mut then = Some(then);
        let result = effects::on_core(|provided| {
            let tabs = provided
                .all::<shell_ipc_w::tabs::TabsState>()
                .into_iter()
                .next()
                .ok_or("tab manager is unavailable")?;
            let done = then.take().ok_or("save continuation already consumed")?;
            tabs.save_material_owned(
                owner,
                authority.cancel.clone(),
                need,
                Box::new(move |answer| {
                    done(match effects::owner(&authority) {
                        Ok(_) => answer,
                        Err(error) => failed_save(to_path, error),
                    });
                }),
            );
            Ok(())
        });
        if let Err(error) = result
            && let Some(then) = then
        {
            then(failed_save(to_path, error));
        }
    }

    /// A timer wakes the owned core continuation, including on owner loss so
    /// that it can acknowledge cancellation. It grants no fresh authority.
    fn set_timeout(&self, authority: &GenerateAuthority, ms: f64, then: Box<dyn FnOnce() + Send>) {
        if let Err(error) = self.check_authority(authority) {
            eprintln!("[generate] timer admission refused: {error}");
            then();
            return;
        }
        let authority = authority.clone();
        let wait = Duration::from_millis(ms.max(0.0) as u64);
        std::thread::spawn(move || {
            std::thread::sleep(wait);
            then();
            drop(authority);
        });
    }

    // ---- `randomUUID()` and `Date.now()` of session.ts ----

    fn new_id(&self) -> String {
        random_uuid()
    }

    fn now_ms(&self) -> f64 {
        now_ms()
    }

    // ---- engines not in main-core ----

    fn parse_file_to_text(
        &self,
        authority: &GenerateAuthority,
        path: &str,
    ) -> Result<ParsedFile, String> {
        self.worker_authority(authority)?;
        let owner = effects::owner(authority)?;
        crate::host::w_files::parse_file_to_text_owned(path, &owner, &authority.cancel)
    }

    /// `OUTLINE_KIND_MARKS.boundary` and `.relationship` of
    /// packages/xmind-engine/src/outline-marks.ts, two constants of the map editor
    fn not_topic_marks(&self) -> [String; 2] {
        ["(boundary)".to_owned(), "(relationship)".to_owned()]
    }

    fn first_sheet_root(
        &self,
        authority: &GenerateAuthority,
        outline: &str,
        untitled: &str,
    ) -> Result<XmindTopic, String> {
        self.worker_authority(authority)?;
        effects::compute(authority, || {
            generate_outline::first_sheet_root(outline, untitled)
        })
    }

    fn create_xmind_document_from_outline(
        &self,
        authority: &GenerateAuthority,
        outline: &str,
        untitled: &str,
    ) -> Result<(), String> {
        self.worker_authority(authority)?;
        effects::compute(authority, || {
            generate_outline::create_xmind_document_from_outline(outline, untitled)
        })
    }

    fn open_blank_deck(&self, authority: &GenerateAuthority) -> Result<u64, String> {
        self.worker_authority(authority)?;
        let deck = effects::compute(authority, || {
            let bytes = pptx_engine::blank::create_blank_pptx(now_ms())?;
            pptx_engine::deck::open_pptx(&bytes)
        })?;
        let owned = OwnedDeck {
            authority: authority.clone(),
            deck,
        };
        effects::commit(authority, || {
            DECKS.with(|decks| {
                let mut decks = decks.borrow_mut();
                let id = decks.0.checked_add(1).ok_or("deck id exhausted")?;
                decks.0 = id;
                decks.1.insert(id, owned);
                Ok(id)
            })
        })?
    }

    fn apply_deck_op(
        &self,
        authority: &GenerateAuthority,
        id: u64,
        op: &JsonValue,
    ) -> Result<OpOutcome, String> {
        self.worker_authority(authority)?;
        let mut owned = effects::commit(authority, || take_deck(authority, id))??;
        let op = jscompat::json_value_to_serde(op);
        let result = apply_outline_deck_op(&mut owned.deck, &op);
        effects::commit(authority, || {
            DECKS.with(|decks| {
                decks.borrow_mut().1.insert(id, owned);
            })
        })?;
        Ok(match result {
            Ok(()) => OpOutcome {
                applied: true,
                first_failure_error: None,
            },
            Err(error) => OpOutcome {
                applied: false,
                first_failure_error: Some(error),
            },
        })
    }

    fn save_deck(&self, authority: &GenerateAuthority, id: u64) -> Result<Vec<u8>, String> {
        self.worker_authority(authority)?;
        let mut owned = effects::commit(authority, || take_deck(authority, id))??;
        effects::compute(authority, || pptx_engine::deck::save_pptx(&mut owned.deck))
    }

    fn release_conversion_resources(&self, authority: &GenerateAuthority) {
        DECKS.with(|decks| {
            decks
                .borrow_mut()
                .1
                .retain(|_, entry| !effects::same_request(&entry.authority, authority))
        });
    }
}

fn apply_outline_deck_op(
    deck: &mut pptx_engine::deck::OpenedPptx,
    op: &serde_json::Value,
) -> Result<(), String> {
    let kind = op
        .get("op")
        .and_then(serde_json::Value::as_str)
        .ok_or("missing operation name")?;
    let index = op
        .get("target")
        .and_then(|target| target.get("slide"))
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or("invalid slide target")?;
    if index >= deck.deck.slides.len() {
        return Err(format!("slide {index} does not exist"));
    }
    match kind {
        "addBlankSlide" => {
            pptx_engine::deck::insert_blank_slide(deck, index as f64)?
                .ok_or_else(|| format!("a blank slide could not be inserted after {index}"))?;
        }
        "addElement" => {
            let mut options: pptx_engine::insert::NewElementOptions =
                serde_json::from_value(op.clone()).map_err(|error| error.to_string())?;
            if options.kind.is_empty() {
                return Err("addElement requires a kind".to_owned());
            }
            let rect = options.offset;
            if ![rect.x.0, rect.y.0, rect.cx.0, rect.cy.0]
                .iter()
                .all(|n| n.is_finite())
                || rect.cx.0 < 0.0
                || rect.cy.0 < 0.0
            {
                return Err("addElement requires a finite nonnegative-sized rectangle".to_owned());
            }
            if let Some(paragraphs) = options.paragraphs.as_mut()
                && let Some(tagged) = pptx_engine::generate::tag_hangul_paragraphs(paragraphs)
            {
                *paragraphs = tagged;
            }
            pptx_engine::insert::add_element(
                &mut deck.deck.slides[index],
                &options,
                &random_uuid(),
                now_ms(),
            )?;
        }
        _ => return Err(format!("unsupported generation operation {kind}")),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outline_deck_operations_survive_save_and_reopen() {
        let bytes = pptx_engine::blank::create_blank_pptx(0.0).unwrap();
        let mut deck = pptx_engine::deck::open_pptx(&bytes).unwrap();
        apply_outline_deck_op(
            &mut deck,
            &serde_json::json!({
                "op": "addBlankSlide", "target": { "slide": 0 }
            }),
        )
        .unwrap();
        apply_outline_deck_op(
            &mut deck,
            &serde_json::json!({
                "op": "addElement", "target": { "slide": 1 }, "kind": "textbox",
                "offset": { "x": 100, "y": 100, "cx": 1000000, "cy": 300000 },
                "paragraphs": [{ "runs": [{ "text": "자료", "fontSize": 20 }] }],
                "bodyPr": { "autoFit": "shrink" }
            }),
        )
        .unwrap();
        let saved = pptx_engine::deck::save_pptx(&mut deck).unwrap();
        let reopened = pptx_engine::deck::open_pptx(&saved).unwrap();
        assert_eq!(reopened.deck.slides.len(), 2);
        assert!(
            serde_json::to_string(&reopened.deck.slides[1])
                .unwrap()
                .contains("자료")
        );
    }

    #[test]
    fn outline_deck_rejects_invalid_slide_before_mutating() {
        let bytes = pptx_engine::blank::create_blank_pptx(0.0).unwrap();
        let mut deck = pptx_engine::deck::open_pptx(&bytes).unwrap();
        assert!(
            apply_outline_deck_op(
                &mut deck,
                &serde_json::json!({
                    "op": "addBlankSlide", "target": { "slide": 9 }
                })
            )
            .is_err()
        );
        assert_eq!(deck.deck.slides.len(), 1);
    }

    #[test]
    fn converted_seed_rejects_mismatched_target_and_invalid_bytes_before_queueing() {
        let invalid = serde_json::json!({
            "id": "conversion", "sources": ["/synthetic/input.xmind"],
            "content": { "kind": "mindmap-to-slides", "target": "markdown", "deck": [80, 75] }
        });
        assert!(PreparedConversion::parse(&invalid).is_err());
        for byte in [
            serde_json::json!(-1),
            serde_json::json!(256),
            serde_json::json!(1.5),
        ] {
            let invalid = serde_json::json!({
                "id": "conversion", "sources": ["/synthetic/input.xmind"],
                "content": { "kind": "mindmap-to-slides", "target": "slides", "deck": [byte] }
            });
            assert!(PreparedConversion::parse(&invalid).is_err());
        }
    }

    #[test]
    fn converted_seed_retains_real_deck_bytes_and_original_sources() {
        let bytes = pptx_engine::blank::create_blank_pptx(0.0).unwrap();
        let document = serde_json::json!({
            "id": "conversion", "sources": ["/synthetic/자료.xmind"],
            "content": { "kind": "mindmap-to-slides", "target": "slides", "deck": bytes }
        });
        let prepared = PreparedConversion::parse(&document).unwrap();
        assert_eq!(prepared.sources, ["/synthetic/자료.xmind"]);
        let ConversionSeed::MindmapToSlides(bytes) = prepared.seed else {
            panic!("wrong target");
        };
        let reopened = pptx_engine::deck::open_pptx(&bytes).unwrap();
        assert_eq!(reopened.deck.slides.len(), 1);
    }
}
