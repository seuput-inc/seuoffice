//! Stage 3 batch D2: Documents 2: docs, hwp, html, markdown, epub, pdf. Registers a handler for every channel whose
//! spec in `shell_contract::CHANNELS` names batch D2; nothing else.
//!
//! One module per app. Each gives its handlers ([`kit::Entry`]) from the
//! services and the hosts it takes, and keeps its per-view state the way the
//! TypeScript's module-level maps kept it. What the handlers reach that is
//! not in Rust yet (engines, `main-core` modules still to port) is a host
//! trait in the app's module; the shell implements it (crates/shell, D2's
//! host file), ipc-probe inserts the module's fakes.

// The app modules are public for the shell (crates/shell): it implements
// their host traits and calls their editors' shell-facing methods.
pub mod docs;
pub mod epub;
mod generated_mailbox;
pub mod html;
pub mod hwp;
mod kit;
pub mod markdown;
pub mod pdf;
mod saved_slots;
mod staged;
#[cfg(test)]
mod testing;
mod text_recovery;

use std::rc::Rc;

use shell_contract::ipc::{RegisterBatch, RegisterError, Registry};
use shell_contract::runtime::Services;
use shell_contract::{Batch, Provided};

/// An app's handlers; it adds its [`shell_contract::DocumentEditors`] to `provided`
type Entries = fn(&Services, &Rc<Provided>) -> Result<Vec<kit::Entry>, RegisterError>;

/// Every app's handlers
const APPS: &[Entries] = &[
    docs::entries,
    epub::entries,
    html::entries,
    hwp::entries,
    markdown::entries,
    pdf::entries,
];

pub fn register(registry: &mut dyn Registry, services: &Services) -> Result<(), RegisterError> {
    let provided = registry.provided();
    for app in APPS {
        for e in app(services, &provided)? {
            registry.register(Batch::D2, e.channel, e.kind, e.handler)?;
        }
    }
    Ok(())
}

const _: RegisterBatch = register;

/// Inserts the fakes of this batch's hosts, answering from the case's
/// `hosts.d2` value (Null when absent; tools/ipc-diff/README.md). Each app
/// reads its own member: `hosts.d2.docs`, `hosts.d2.pdf`, ...
#[cfg(feature = "probe")]
pub fn probe_hosts(hosts: &mut shell_contract::Hosts, case: &serde_json::Value) {
    fakes(hosts, case)
}

#[cfg(any(test, feature = "probe"))]
fn fakes(hosts: &mut shell_contract::Hosts, case: &serde_json::Value) {
    let app = |name: &str| case.get(name).unwrap_or(&serde_json::Value::Null);
    docs::fakes(hosts, app("docs"));
    epub::fakes(hosts, app("epub"));
    html::fakes(hosts, app("html"));
    hwp::fakes(hosts, app("hwp"));
    markdown::fakes(hosts, app("markdown"));
    pdf::fakes(hosts, app("pdf"));
}

/// An outbound call another area makes into a D2 editor, as a case of
/// tools/ipc-diff names it: `d2/<app>:<what>` (`d2/epub:request-save`).
/// `then` gets the call's outcome; false when no app knows the name.
#[cfg(feature = "probe")]
pub type Trigger = fn(
    what: &str,
    view: shell_contract::ViewId,
    args: &serde_json::Value,
    services: &Services,
    provided: &Rc<Provided>,
    then: shell_contract::editors::Then<shell_contract::IpcValue>,
) -> bool;

/// The triggers of each app, by the app part of the name
#[cfg(feature = "probe")]
const TRIGGERS: &[(&str, Trigger)] = &[
    ("docs", docs::probe_trigger),
    ("epub", epub::probe_trigger),
    ("html", html::probe_trigger),
    ("hwp", hwp::probe_trigger),
    ("markdown", markdown::probe_trigger),
    ("pdf", pdf::editor::probe::probe_trigger),
    ("pdf-tools", pdf::tools::probe_trigger),
];

/// Runs a `trigger` case of tools/ipc-diff (README): `name` is
/// `d2/<app>:<what>`; the app's own `probe_trigger` knows `<what>`
#[cfg(feature = "probe")]
pub fn probe_trigger(
    name: &str,
    view: shell_contract::ViewId,
    args: &serde_json::Value,
    services: &Services,
    provided: &Rc<Provided>,
    then: shell_contract::editors::Then<shell_contract::IpcValue>,
) -> bool {
    let Some((app, what)) = name.strip_prefix("d2/").and_then(|n| n.split_once(':')) else {
        return false;
    };
    TRIGGERS
        .iter()
        .find(|(name, _)| *name == app)
        .is_some_and(|(_, trigger)| trigger(what, view, args, services, provided, then))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::{Arc, Mutex};

    use main_core::packages::electron_utils::recovery_store::SharedRecoveryState;
    use shell_contract::ipc::ChannelKind;
    use shell_contract::{Batch, CHANNELS};

    use crate::testing::Rt;

    /// The shell's start-up check for this batch: every D2 channel a renderer
    /// invokes or sends to has a handler of that kind, and nothing else is
    /// registered (the registry refuses unknown, twice, other batches' and
    /// wrong-kind channels)
    #[test]
    fn registers_every_d2_channel_and_nothing_else() {
        let mut rt = Rt::new(|hosts| {
            // the shell inserts the process's one recovery state
            hosts.insert::<Arc<Mutex<SharedRecoveryState>>>(Arc::new(Mutex::new(
                SharedRecoveryState::new("0000000000000000"),
            )));
            super::fakes(hosts, &serde_json::Value::Null)
        });
        for app in super::APPS {
            rt.register(*app).unwrap();
        }
        let got: BTreeSet<_> = rt
            .registered()
            .into_iter()
            .map(|(c, k)| (c, format!("{k:?}")))
            .collect();
        let want: BTreeSet<_> = CHANNELS
            .iter()
            .filter(|s| s.batch == Batch::D2)
            .flat_map(|s| {
                s.kinds
                    .iter()
                    .filter(|k| **k != ChannelKind::Event)
                    .map(move |k| (s.name, format!("{k:?}")))
            })
            .collect();
        let missing: Vec<_> = want.difference(&got).collect();
        let extra: Vec<_> = got.difference(&want).collect();
        assert!(
            missing.is_empty() && extra.is_empty(),
            "missing {missing:?}, extra {extra:?}"
        );
    }

    /// The folder confirmed in one app's dialog is where another app's dialog
    /// opens: the TypeScript keeps one memory for the whole process
    #[test]
    fn a_folder_picked_in_one_app_is_where_another_apps_dialog_opens() {
        use shell_contract::{IpcValue, Surface};

        use crate::testing::HostCall;

        let mut rt = Rt::new(|hosts| {
            hosts.insert::<Arc<Mutex<SharedRecoveryState>>>(Arc::new(Mutex::new(
                SharedRecoveryState::new("0000000000000000"),
            )));
            super::fakes(hosts, &serde_json::Value::Null)
        });
        for app in super::APPS {
            rt.register(*app).unwrap();
        }
        let picked = rt.dir.join("picked");
        std::fs::create_dir_all(&picked).unwrap();
        let html = rt.view(1, Surface::Html);
        let docs = rt.view(2, Surface::Docs);
        rt.script().save.push_back(Some(picked.join("a.html")));
        let request = IpcValue::Object(vec![
            ("text".into(), IpcValue::String("x".into())),
            ("mode".into(), IpcValue::String("save".into())),
        ]);
        rt.invoke_now("html:save", html, vec![request]).unwrap();
        // the other editor's save dialog starts in that folder
        rt.script().save.push_back(None);
        rt.invoke_now(
            "docs:save-as-pick",
            docs,
            vec![IpcValue::String("b.docx".into())],
        )
        .unwrap();
        let calls = rt.script().calls.clone();
        let Some(HostCall::Save(_, second)) = calls
            .iter()
            .filter(|c| matches!(c, HostCall::Save(..)))
            .nth(1)
        else {
            panic!("{calls:?}")
        };
        let start = second.default_path.clone().unwrap();
        assert!(start.starts_with(&picked), "{start:?} not in {picked:?}");
    }

    /// A saved output opens in a new view of its editor (crates/shell, w_tabs.rs
    /// `open_in_path_editor`): an editor that tracks its views takes the saved
    /// token only on a view it has open, and the shell opens the view in it on
    /// no document, as here, before the token
    #[cfg(unix)]
    #[test]
    fn a_saved_token_needs_the_view_open_in_its_editor() {
        use shell_contract::editors::DocumentEditors;
        use shell_contract::ipc::{NativeClosureIssuer, NativeSavedTarget};
        use shell_contract::runtime::CancelToken;
        use shell_contract::{Surface, ViewId};

        let mut rt = Rt::new(|hosts| {
            hosts.insert::<Arc<Mutex<SharedRecoveryState>>>(Arc::new(Mutex::new(
                SharedRecoveryState::new("0000000000000000"),
            )));
            super::fakes(hosts, &serde_json::Value::Null)
        });
        for app in super::APPS {
            rt.register(*app).unwrap();
        }
        let provided = rt.provided();
        let markdown = provided.all::<crate::markdown::MarkdownEditors>()[0].clone();
        let html = provided.all::<crate::html::HtmlEditors>()[0].clone();
        let pdf = provided.all::<crate::pdf::editor::editors::PdfEditors>()[0].clone();
        let epub = provided.all::<crate::epub::EpubEditors>()[0].clone();
        let hwp = provided.all::<crate::hwp::HwpEditors>()[0].clone();
        let source = rt.view(1, Surface::Markdown);
        markdown.track_view(source.view, None);
        let existing = markdown
            .capture_document_owner(source.view, rt.native_root(source.view))
            .unwrap();
        let saved_target = |view: ViewId, surface: Surface, name: &str| {
            let path = rt.dir.join(name);
            std::fs::write(&path, "old").unwrap();
            let (input, saved_path) =
                crate::html::ack_evidence::saved_input(&rt, Surface::Markdown, &existing, &path);
            let generation = rt.services.views.renderer_generation(view).unwrap();
            let (issuer, reservation, browser) =
                NativeClosureIssuer::new_with_owner(view, generation.clone());
            let target = NativeSavedTarget::prepare(
                input,
                saved_path,
                surface,
                view,
                generation,
                reservation,
                browser,
                vec![],
                CancelToken::default(),
                CancelToken::default(),
                vec![],
            )
            .unwrap();
            (target, issuer)
        };
        type OpenCase<'a> = (Surface, &'static str, &'a dyn Fn(ViewId));
        let opens: [OpenCase<'_>; 5] = [
            (Surface::Markdown, "saved.md", &|view| {
                markdown.track_view(view, None)
            }),
            (Surface::Html, "saved.html", &|view| {
                html.track_view(view, None)
            }),
            (Surface::Pdf, "saved.pdf", &|view| pdf.open_view(view, None)),
            (Surface::Epub, "saved.epub", &|view| {
                epub.open_view(view, None)
            }),
            (Surface::Hwp, "saved.hwp", &|view| {
                hwp.grant_and_track(view, None, crate::hwp::UntitledOption::No)
            }),
        ];
        for (id, (surface, name, open)) in (10..).zip(opens) {
            let view = rt.view(id, surface).view;
            let (target, _issuer) = saved_target(view, surface, name);
            let editor = provided
                .all::<dyn DocumentEditors>()
                .into_iter()
                .find(|editor| editor.saved_surface() == surface)
                .unwrap();
            assert!(
                editor.prepare_saved_document(target.clone()).is_err(),
                "{surface:?}: a view its editor never opened takes no saved token"
            );
            open(view);
            let prepared = editor
                .prepare_saved_document(target)
                .map(|_| ())
                .map_err(|error| error.message);
            assert_eq!(
                prepared,
                Ok(()),
                "{surface:?}: the view opened on no document takes the saved token"
            );
        }
    }
}
