use crate::docs::{
    editors::DocsEditors,
    hosts::{DocsEngineHost, DocsShellHost, Fake},
};
use crate::{kit::IntoIpc, testing::Rt};
use serde_json::json;
use shell_contract::{IpcValue, Surface};
use std::sync::{Arc, Mutex};

#[test]
fn owned_print_requires_its_pending_token_and_exports_reject_foreign_tokens_before_effects() {
    let fake = Fake::new(&json!({}));
    let installed = fake.clone();
    let mut rt = Rt::new(move |hosts| {
        hosts.insert::<DocsEngineHost>(Arc::new(installed.clone()));
        hosts.insert::<DocsShellHost>(Arc::new(installed));
        hosts.insert(Arc::new(Mutex::new(
            main_core::packages::electron_utils::recovery_store::SharedRecoveryState::new("test"),
        )));
    });
    fake.bind_query_runtime(&rt.services);
    for directory in ["userData", "documents", "temp", "home"] {
        std::fs::create_dir_all(rt.dir.join(directory)).unwrap();
    }
    rt.register(crate::docs::entries).unwrap();
    let sender = rt.view(1, Surface::Docs);
    rt.provided().all::<DocsEditors>()[0]
        .register_view(sender.view)
        .unwrap();
    let missing = rt
        .invoke_now(
            "docs:owned-print-pdf-buffer",
            sender,
            vec![11906.0.into_ipc(), 16838.0.into_ipc()],
        )
        .unwrap_err();
    assert!(missing.message.contains("pending save token"));
    let foreign_surface = rt.view(2, Surface::Pdf);
    let mut table = shell_core::table::Table::default();
    let provided = shell_contract::ipc::Registry::provided(&table);
    for entry in crate::docs::entries(&rt.services, &provided).unwrap() {
        shell_contract::ipc::Registry::register(
            &mut table,
            shell_contract::Batch::D2,
            entry.channel,
            entry.kind,
            entry.handler,
        )
        .unwrap();
    }
    let dispatcher = shell_core::dispatch::Dispatcher::new(table);
    let (send, receive) = std::sync::mpsc::channel();
    dispatcher.dispatch(
        foreign_surface,
        shell_contract::ipc::ChannelKind::Invoke,
        Some(shell_contract::RequestId(1)),
        None,
        "docs:owned-print-pdf-buffer",
        vec![
            11906.0.into_ipc(),
            16838.0.into_ipc(),
            IpcValue::Undefined,
            "foreign-native-request".into_ipc(),
        ],
        Box::new(move |result| send.send(result).unwrap()),
    );
    assert!(
        receive
            .try_recv()
            .unwrap()
            .unwrap_err()
            .message
            .contains("not open to Pdf")
    );
    let target = rt
        .dir
        .join("not-written.html")
        .to_string_lossy()
        .into_owned();
    let foreign = || "foreign-native-request".into_ipc();
    let calls = vec![
        (
            "docs:owned-print-pdf-buffer",
            vec![
                11906.0.into_ipc(),
                16838.0.into_ipc(),
                IpcValue::Undefined,
                foreign(),
            ],
        ),
        (
            "docs:export-pdf",
            vec![
                "file.docx".into_ipc(),
                11906.0.into_ipc(),
                16838.0.into_ipc(),
                target.clone().into_ipc(),
                IpcValue::Undefined,
                IpcValue::Undefined,
                foreign(),
            ],
        ),
        (
            "docs:export-html",
            vec![
                "file.docx".into_ipc(),
                "<p>content</p>".into_ipc(),
                target.clone().into_ipc(),
                foreign(),
            ],
        ),
        (
            "docs:export-markdown",
            vec![
                "file.docx".into_ipc(),
                "content".into_ipc(),
                IpcValue::Array(vec![]),
                target.clone().into_ipc(),
                foreign(),
            ],
        ),
        (
            "docs:export-hwp",
            vec![
                "file.docx".into_ipc(),
                IpcValue::Undefined,
                target.clone().into_ipc(),
                false.into_ipc(),
                foreign(),
            ],
        ),
        (
            "docs:save-merged-pdf",
            vec![
                "file.docx".into_ipc(),
                IpcValue::Array(vec![]),
                target.clone().into_ipc(),
                IpcValue::Undefined,
                foreign(),
            ],
        ),
        ("docs:pick-export-images-target", vec![foreign()]),
        (
            "docs:take-export-pdf",
            vec![target.clone().into_ipc(), foreign()],
        ),
        (
            "docs:write-export-image",
            vec![
                target.clone().into_ipc(),
                "page.png".into_ipc(),
                "AA==".into_ipc(),
                foreign(),
            ],
        ),
    ];
    for (channel, arguments) in calls {
        let error = rt.invoke_now(channel, sender, arguments).unwrap_err();
        assert!(
            error.message.contains("no longer pending"),
            "{channel}: {}",
            error.message
        );
    }
    assert!(!std::path::Path::new(&target).exists());
    assert!(
        fake.calls
            .lock()
            .unwrap()
            .iter()
            .all(|call| !call.to_string().contains("merge_pdfs")
                && !call.to_string().contains("convert_to_hwp"))
    );
}

#[test]
fn native_headless_export_rejects_wrong_path_and_format_before_any_publication() {
    use shell_contract::{
        hosts::OwnedEffectsHost,
        ipc::{NativeInvocationIssuer, NativeInvocationSelection},
        runtime::CancelToken,
    };
    let fake = Fake::new(&json!({"is_headless": true}));
    let installed = fake.clone();
    let mut rt = Rt::new(move |hosts| {
        hosts.insert::<DocsEngineHost>(Arc::new(installed.clone()));
        hosts.insert::<DocsShellHost>(Arc::new(installed));
        hosts.insert(Arc::new(Mutex::new(
            main_core::packages::electron_utils::recovery_store::SharedRecoveryState::new("test"),
        )));
    });
    fake.bind_query_runtime(&rt.services);
    for directory in ["userData", "documents", "temp", "home"] {
        std::fs::create_dir_all(rt.dir.join(directory)).unwrap();
    }
    rt.register(crate::docs::entries).unwrap();
    let sender = rt.view(1, Surface::Docs);
    let editor = rt.provided().all::<DocsEditors>()[0].clone();
    editor.register_view(sender.view).unwrap();
    let input = rt.dir.join("input.docx");
    let output = rt.dir.join("selected.html");
    let wrong = rt.dir.join("other.html");
    std::fs::write(&input, b"document").unwrap();
    let operation = NativeInvocationIssuer::new(CancelToken::default()).issue(
        NativeInvocationSelection::HeadlessExport {
            surface: Surface::Docs,
            input: input.clone(),
            output: output.clone(),
            format: "html".into(),
            page: None,
            ops: None,
            batch: None,
        },
        CancelToken::default(),
    );
    let renderer = rt.services.views.renderer_generation(sender.view).unwrap();
    let target = operation
        .reserve_target(sender.view, renderer.clone())
        .unwrap();
    editor.begin_headless_export_native(
        sender.view,
        input.to_string_lossy().into_owned(),
        output.to_string_lossy().into_owned(),
        "html",
        std::time::Duration::from_secs(10),
        &target,
        Box::new(|_| {}),
    );
    let owner = rt
        .services
        .hosts
        .get::<OwnedEffectsHost>()
        .unwrap()
        .capture_owner(sender.view, &renderer)
        .unwrap();
    let wrong_path = rt
        .invoke_native(
            "docs:export-html",
            sender,
            vec![
                "input.docx".into_ipc(),
                "<p>text</p>".into_ipc(),
                wrong.to_string_lossy().into_owned().into_ipc(),
            ],
            owner.context.clone(),
            CancelToken::default(),
        )
        .take()
        .unwrap()
        .unwrap_err();
    assert!(wrong_path.message.contains("selected output"));
    let wrong_format = rt
        .invoke_native(
            "docs:export-markdown",
            sender,
            vec![
                "input.docx".into_ipc(),
                "text".into_ipc(),
                IpcValue::Array(vec![]),
                output.to_string_lossy().into_owned().into_ipc(),
            ],
            owner.context.clone(),
            CancelToken::default(),
        )
        .take()
        .unwrap()
        .unwrap_err();
    assert!(wrong_format.message.contains("selected output"));
    let wrong_pdf = rt
        .invoke_native(
            "docs:export-pdf",
            sender,
            vec![
                "input.docx".into_ipc(),
                11906.0.into_ipc(),
                16838.0.into_ipc(),
                output.to_string_lossy().into_owned().into_ipc(),
            ],
            owner.context.clone(),
            CancelToken::default(),
        )
        .take()
        .unwrap()
        .unwrap_err();
    assert!(wrong_pdf.message.contains("selected output"));
    let wrong_merged = rt
        .invoke_native(
            "docs:save-merged-pdf",
            sender,
            vec![
                "input.docx".into_ipc(),
                IpcValue::Array(vec![]),
                output.to_string_lossy().into_owned().into_ipc(),
            ],
            owner.context,
            CancelToken::default(),
        )
        .take()
        .unwrap()
        .unwrap_err();
    assert!(wrong_merged.message.contains("selected output"));
    assert!(!output.exists());
    assert!(!wrong.exists());
    assert!(rt.script().calls.is_empty());
}

#[derive(Default)]
struct Deferred(Mutex<Vec<Box<dyn FnOnce() + Send>>>);
impl shell_contract::runtime::Workers for Deferred {
    fn spawn(&self, job: Box<dyn FnOnce() + Send>) {
        self.0.lock().unwrap().push(job);
    }
}

#[test]
fn headless_html_and_markdown_use_real_invocation_and_abort_before_publication() {
    use crate::kit::field;
    use shell_contract::{
        hosts::synthetic::{
            SyntheticNativeOrigin, SyntheticOwnedEffects, register_native_invocation_fixture,
        },
        ipc::{NativeInvocationIssuer, NativeInvocationSelection},
        runtime::CancelToken,
    };
    for (format, abort) in [("html", false), ("md", false), ("md", true)] {
        let fake = Fake::new(&json!({"is_headless": true}));
        let installed = fake.clone();
        let mut rt = Rt::new(move |hosts| {
            hosts.insert::<DocsEngineHost>(Arc::new(installed.clone()));
            hosts.insert::<DocsShellHost>(Arc::new(installed));
            hosts.insert(Arc::new(Mutex::new(
                main_core::packages::electron_utils::recovery_store::SharedRecoveryState::new(
                    "test",
                ),
            )));
        });
        let workers = Arc::new(Deferred::default());
        rt.services.workers = workers.clone();
        fake.bind_query_runtime(&rt.services);
        for directory in ["userData", "documents", "temp", "home"] {
            std::fs::create_dir_all(rt.dir.join(directory)).unwrap();
        }
        rt.register(crate::docs::entries).unwrap();
        let sender = rt.view(1, Surface::Docs);
        let editor = rt.provided().all::<DocsEditors>()[0].clone();
        editor.register_view(sender.view).unwrap();
        let input = rt.dir.join("input.docx");
        let output = rt.dir.join(format!("selected.{format}"));
        let image = rt.dir.join("selected_files/image1.png");
        std::fs::write(&input, b"document").unwrap();
        let operation = NativeInvocationIssuer::new(CancelToken::default()).issue(
            NativeInvocationSelection::HeadlessExport {
                surface: Surface::Docs,
                input: input.clone(),
                output: output.clone(),
                format: format.into(),
                page: None,
                ops: None,
                batch: None,
            },
            CancelToken::default(),
        );
        let renderer = rt.services.views.renderer_generation(sender.view).unwrap();
        let target = operation
            .reserve_target(sender.view, renderer.clone())
            .unwrap();
        let effects = rt
            .services
            .hosts
            .get::<Arc<SyntheticOwnedEffects>>()
            .unwrap()
            .clone();
        let native = SyntheticNativeOrigin::new();
        let first = effects
            .capture_invocation_root(sender.view, &renderer, &native)
            .unwrap();
        register_native_invocation_fixture(&target, first).unwrap();
        editor.begin_headless_export_native(
            sender.view,
            input.to_string_lossy().into_owned(),
            output.to_string_lossy().into_owned(),
            format,
            std::time::Duration::from_secs(10),
            &target,
            Box::new(|_| {}),
        );
        let candidate = effects
            .capture_invocation_docs(sender.view, &renderer, &native)
            .unwrap();
        let (channel, args) = if format == "html" {
            (
                "docs:export-html",
                vec![
                    "input.docx".into_ipc(),
                    "<p>actual</p>".into_ipc(),
                    output.to_string_lossy().into_owned().into_ipc(),
                ],
            )
        } else {
            (
                "docs:export-markdown",
                vec![
                    "input.docx".into_ipc(),
                    "![x](__seuoffice_media__/image1.png)".into_ipc(),
                    IpcValue::Array(vec![crate::kit::obj([
                        ("name", "image1.png".into_ipc()),
                        ("src", "data:image/png;base64,AQID".into_ipc()),
                    ])]),
                    output.to_string_lossy().into_owned().into_ipc(),
                ],
            )
        };
        let pending = rt.invoke_native(
            channel,
            sender,
            args,
            candidate.context,
            CancelToken::default(),
        );
        if abort {
            assert!(editor.fail_headless_export(sender.view, "aborted".into()));
        }
        let job = workers.0.lock().unwrap().remove(0);
        let thread = std::thread::spawn(job);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let reply = loop {
            rt.drain();
            if let Some(reply) = pending.take() {
                break reply;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "headless export did not finish"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        };
        thread.join().unwrap();
        if abort {
            assert!(reply.is_err() || field(&reply.unwrap(), "ok") == &IpcValue::Bool(false));
            assert!(!output.exists());
            assert!(!image.exists());
        } else {
            assert_eq!(field(&reply.unwrap(), "ok"), &IpcValue::Bool(true));
            assert!(output.exists());
            if format == "md" {
                assert_eq!(std::fs::read(image).unwrap(), [1, 2, 3]);
            }
        }
    }
}

#[test]
fn owned_print_consumes_real_save_scope_and_waits_for_exact_ack_or_owner_loss() {
    use crate::{kit::field, testing::HostCall};
    use shell_contract::{
        editors::{DocumentEditors, SaveMode},
        hosts::OwnedEffectsHost,
        runtime::CancelToken,
    };
    use std::{cell::RefCell, rc::Rc};
    for scenario in ["normal-completion", "owner-loss", "effect-failure"] {
        let fake = Fake::new(&json!({}));
        let installed = fake.clone();
        let mut rt = Rt::new(move |hosts| {
            hosts.insert::<DocsEngineHost>(Arc::new(installed.clone()));
            hosts.insert::<DocsShellHost>(Arc::new(installed));
            hosts.insert(Arc::new(Mutex::new(
                main_core::packages::electron_utils::recovery_store::SharedRecoveryState::new(
                    "test",
                ),
            )));
        });
        fake.bind_query_runtime(&rt.services);
        for directory in ["userData", "documents", "temp", "home"] {
            std::fs::create_dir_all(rt.dir.join(directory)).unwrap();
        }
        let workers = Arc::new(Deferred::default());
        rt.services.workers = workers.clone();
        rt.register(crate::docs::entries).unwrap();
        let sender = rt.view(1, Surface::Docs);
        let editor = rt.provided().all::<DocsEditors>()[0].clone();
        editor.register_view(sender.view).unwrap();
        let renderer = rt.services.views.renderer_generation(sender.view).unwrap();
        let effects = rt.services.hosts.get::<OwnedEffectsHost>().unwrap().clone();
        let origin = effects.capture_owner(sender.view, &renderer).unwrap();
        let result = Rc::new(RefCell::new(None));
        let completed = result.clone();
        let request_cancel = CancelToken::default();
        editor.request_save_owned(
            sender.view,
            SaveMode::SaveAs,
            origin.clone(),
            request_cancel.clone(),
            Box::new(move |value| *completed.borrow_mut() = Some(value)),
        );
        rt.drain();
        assert!(result.borrow().is_none(), "{scenario}");
        let event = rt
            .events()
            .into_iter()
            .find(|event| event.channel == "docs:close-save-request")
            .unwrap();
        let token = field(&event.args[0], "requestId").clone();
        assert!(matches!(&token, IpcValue::String(_)));
        let destination = editor
            .0
            .pending_save_destination(crate::kit::as_str(&token).unwrap())
            .unwrap();
        if scenario != "effect-failure" {
            rt.script().pdf = Some(b"%PDF owned result".to_vec());
        }
        let printing = rt.invoke_native(
            "docs:owned-print-pdf-buffer",
            sender,
            vec![
                11906.0.into_ipc(),
                16838.0.into_ipc(),
                IpcValue::Undefined,
                token.clone(),
            ],
            destination.context.clone(),
            request_cancel.clone(),
        );
        assert!(printing.take().is_none());
        assert!(result.borrow().is_none());
        if scenario == "owner-loss" {
            rt.replace_renderer(sender.view);
        }
        let jobs = std::mem::take(&mut *workers.0.lock().unwrap());
        assert_eq!(jobs.len(), 1);
        for job in jobs {
            job();
        }
        rt.drain();
        let printed = printing.take().unwrap();
        if scenario == "owner-loss" {
            assert!(printed.is_err());
            assert!(result.borrow().as_ref().unwrap().is_err());
            assert!(
                !rt.script()
                    .calls
                    .contains(&HostCall::PrintToPdf(sender.view))
            );
        } else {
            let printed = printed.unwrap();
            assert_eq!(
                field(&printed, "ok"),
                &(scenario == "normal-completion").into_ipc()
            );
            if scenario == "normal-completion" {
                assert_eq!(
                    field(&printed, "base64"),
                    &"JVBERiBvd25lZCByZXN1bHQ=".into_ipc()
                );
            } else {
                assert!(
                    matches!(field(&printed, "error"), IpcValue::String(error) if error.contains("printToPDF failed"))
                );
            }
            assert!(result.borrow().is_none());
            assert_eq!(
                rt.script()
                    .calls
                    .iter()
                    .filter(|call| **call == HostCall::PrintToPdf(sender.view))
                    .count(),
                1
            );
            rt.send_native(
                "docs:close-save-result",
                sender,
                vec![true.into_ipc(), "foreign-token".into_ipc()],
                destination.context.clone(),
                request_cancel.clone(),
            );
            assert!(result.borrow().is_none());
            let saved = scenario == "normal-completion";
            rt.send_native(
                "docs:close-save-result",
                sender,
                vec![saved.into_ipc(), token],
                destination.context.clone(),
                request_cancel.clone(),
            );
            assert_eq!(result.borrow_mut().take().unwrap().unwrap(), saved);
        }
        assert!(!request_cancel.is_cancelled());
    }
}
