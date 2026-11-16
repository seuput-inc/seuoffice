//! Editor file hosts backed by the shell's live tab manager.

use std::sync::Arc;

use main_core::packages::electron_utils::unsaved_files::{OpenFileState, OpenFilesSourceFailed};
use main_core::packages::i18n::{Lang, install_lang, normalize_lang};
use main_core::shell::app_settings::read_app_settings;
use main_core::shell::tab_manager::{OpenDocsOptions, OpenDocumentTab, TabKind};
use shell_contract::Hosts;
use shell_ipc_d1::sheets::engine::WorkbookEngineHost;
use shell_ipc_w::editor_files::{
    DocsTab, DocsTabs, DocsTabsHost, FileParse, FileParseHost, OpenFiles, OpenFilesHost,
    ParsedFile, ParsedKind, SheetsTabs, SheetsTabsHost, UiLanguage, UiLanguageHost,
};
use shell_ipc_w::tabs::TabsState;

use super::{HostContext, w::on_core};
use crate::paths::ShellPaths;

pub(super) mod markup;
mod readers;
mod zip;

pub(super) fn install(hosts: &mut Hosts, cx: &HostContext) {
    let tabs: DocsTabsHost = Arc::new(ShellDocsTabs);
    hosts.insert(tabs);
    let language: UiLanguageHost = Arc::new(ShellLanguage(cx.ui.clone()));
    hosts.insert(language);
    let open_files: OpenFilesHost = Arc::new(ShellOpenFiles);
    let parser: FileParseHost = Arc::new(ShellFileParser);
    hosts.insert(parser);
    hosts.insert(open_files);
    let engine = hosts
        .get::<WorkbookEngineHost>()
        .expect("workbook engine installed before file hosts")
        .clone();
    let sheets: SheetsTabsHost = Arc::new(ShellSheetsTabs(engine));
    hosts.insert(sheets);
}

struct ShellSheetsTabs(WorkbookEngineHost);

impl SheetsTabs for ShellSheetsTabs {
    fn is_tab(&self, view: shell_contract::ids::ViewId) -> bool {
        on_core(move |_| {
            shell_ipc_d1::sheets::views().is_some_and(|tabs| tabs.is_tab(view))
        })
        .unwrap_or(false)
    }

    fn external_link_source(
        &self,
        view: shell_contract::ids::ViewId,
        session_id: &str,
        index: u64,
    ) -> Option<String> {
        let id = session_id.to_owned();
        let (path, base) = on_core(move |_| {
            shell_ipc_d1::sheets::views()?.external_link_session(view, &id)
        })
        .flatten()?;
        let listing = self.0.list_external_links(&path, &base).ok()?;
        let shell_contract::IpcValue::Object(fields) = listing else {
            return None;
        };
        let shell_contract::IpcValue::Array(links) =
            &fields.iter().find(|(name, _)| name == "links")?.1
        else {
            return None;
        };
        links.iter().find_map(|link| {
            let shell_contract::IpcValue::Object(fields) = link else { return None; };
            let found = fields.iter().find(|(name, _)| name == "index")?;
            if !matches!(&found.1, shell_contract::IpcValue::Number(value) if *value == index as f64) { return None; }
            match &fields.iter().find(|(name, _)| name == "source")?.1 {
                shell_contract::IpcValue::String(source) => Some(source.clone()),
                _ => None,
            }
        })
    }
}
pub(super) struct ShellFileParser;

fn document_bytes(path: &str, ext: &str, diagram: bool) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let zip_document = matches!(
        ext,
        "docx" | "pptx" | "xlsx" | "xlsm" | "hwpx" | "odt" | "epub" | "xmind" | "doc" | "hwp"
    );
    if zip_document && !diagram {
        let metadata = std::fs::metadata(path)
            .map_err(|error| main_core::node_fs::node_error(&error, "stat", path).message)?;
        if metadata.len() > 50 * 1024 * 1024 {
            let mut file = std::fs::File::open(path)
                .map_err(|error| main_core::node_fs::node_error(&error, "open", path).message)?;
            let mut magic = [0; 2];
            let count = file
                .read(&mut magic)
                .map_err(|error| main_core::node_fs::node_error(&error, "read", path).message)?;
            if count == 2 && magic == *b"PK" {
                return zip::read_zip_text_parts(std::path::Path::new(path));
            }
        }
    }
    std::fs::read(path).map_err(|error| {
        let syscall = if error.kind() == std::io::ErrorKind::IsADirectory {
            "read"
        } else {
            "open"
        };
        main_core::node_fs::node_error(&error, syscall, path).message
    })
}

pub(super) fn parsed_value(path: &str) -> Result<serde_json::Value, String> {
    parsed_value_with_host(path, &seu_host::call)
}

fn parsed_value_with_host(
    path: &str,
    host: &file_parse::parse::HostCall<'_>,
) -> Result<serde_json::Value, String> {
    use file_parse::parse::{ParseFileOptions, parse_file_to_text_with_host};
    use main_core::packages::file_parse::diagram::diagram_format_of;
    use serde_json::json;

    let ext = jscompat::node_path::host::extname(path)
        .trim_start_matches('.')
        .to_lowercase();
    let image = matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp");
    let diagram = diagram_format_of(path).is_some();
    let bytes = if image && !diagram {
        Vec::new()
    } else {
        match document_bytes(path, &ext, diagram) {
            Ok(bytes) => bytes,
            Err(message) => return Ok(json!({ "ok": false, "kind": "text", "error": message })),
        }
    };
    let hosted = match ext.as_str() {
        "odt" => Some(readers::read_odt(&bytes)),
        "pdf" | "xmind" => {
            let step = if ext == "pdf" {
                "reader.pdf"
            } else {
                "reader.xmind"
            };
            Some(
                match host(step, &json!({ "bytesBase64": jscompat::base64(&bytes) })) {
                    Ok(result) => result,
                    Err(error) => {
                        json!({ "error": { "name": error.name, "message": error.message } })
                    }
                },
            )
        }
        _ => None,
    };
    let options = ParseFileOptions {
        whole_zip_max_bytes: Some(f64::INFINITY),
        ..Default::default()
    };
    Ok(parse_file_to_text_with_host(
        path,
        &bytes,
        options,
        hosted.as_ref(),
        host,
    ))
}

pub(super) fn parse_file_to_text(
    path: &str,
) -> Result<main_core::shell::generate::materials::ParsedFile, String> {
    parsed_file(parsed_value(path)?)
}

fn parsed_file(
    value: serde_json::Value,
) -> Result<main_core::shell::generate::materials::ParsedFile, String> {
    let string = |key: &str| {
        value
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    };
    Ok(main_core::shell::generate::materials::ParsedFile {
        ok: value
            .get("ok")
            .and_then(serde_json::Value::as_bool)
            .ok_or("file parser returned no ok flag")?,
        kind: string("kind"),
        text: string("text"),
        error: string("error"),
    })
}

/// Worker-only attachment parsing with the original authenticated request.
pub(super) fn parse_file_to_text_owned(
    path: &str,
    owner: &shell_contract::ipc::NativeRequestOwner,
    cancel: &shell_contract::runtime::CancelToken,
) -> Result<main_core::shell::generate::materials::ParsedFile, String> {
    // Admit the read, but never retain authority locks while doing file IO or
    // waiting for an engine page. Each actual engine eval has its own admission.
    crate::ipc::native_owner::with_owner_effect(owner, cancel, || ())
        .ok_or("the file parser request owner is no longer current")?;
    let result = parsed_value_with_host(path, &|name, input| {
        crate::engine_host::call_owned(name, input, owner, cancel)
    })
    .and_then(parsed_file);
    crate::ipc::native_owner::with_owner_effect(owner, cancel, || result)
        .ok_or_else(|| "the file parser request owner is no longer current".to_owned())?
}

pub(super) fn diagram_picture_ext(path: &std::path::Path) -> Option<&'static str> {
    use main_core::packages::file_parse::diagram::diagram_picture_ext_of_name;
    let name = path.file_name()?.to_str()?;
    let ext = diagram_picture_ext_of_name(name)?;
    let Ok(meta) = std::fs::metadata(path) else {
        return Some(ext);
    };
    if meta.len() > 50 * 1024 * 1024 {
        return Some(ext);
    }
    let Ok(bytes) = std::fs::read(path) else {
        return Some(ext);
    };
    let Ok(result) = seu_host::call(
        "fileparse.readDiagramText",
        &serde_json::json!({
            "fileName": name, "bytesBase64": jscompat::base64(&bytes),
        }),
    ) else {
        return Some(ext);
    };
    if result.get("reason").and_then(serde_json::Value::as_str) == Some("no_original") {
        None
    } else {
        Some(ext)
    }
}

impl FileParse for ShellFileParser {
    fn parse_file_to_text(&self, path: &std::path::Path) -> Result<ParsedFile, String> {
        let path = path.to_str().ok_or("file path is not UTF-8")?;
        let value = parsed_value(path)?;
        let string = |key: &str| {
            value
                .get(key)
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        };
        let kind = match value.get("kind").and_then(serde_json::Value::as_str) {
            Some("text") => ParsedKind::Text,
            Some("image") => ParsedKind::Image,
            Some("unsupported") => ParsedKind::Unsupported,
            _ => return Err("file parser returned an invalid kind".into()),
        };
        Ok(ParsedFile {
            ok: value
                .get("ok")
                .and_then(serde_json::Value::as_bool)
                .ok_or("file parser returned no ok flag")?,
            kind,
            text: string("text"),
            error: string("error"),
            damaged: value.get("damaged").and_then(serde_json::Value::as_bool) == Some(true),
        })
    }

    fn diagram_picture_ext(&self, path: &std::path::Path) -> Option<&'static str> {
        diagram_picture_ext(path)
    }
}
struct ShellDocsTabs;

struct ShellOpenFiles;

pub(super) fn open_documents() -> Result<Vec<OpenDocumentTab>, String> {
    let (send, receive) = std::sync::mpsc::sync_channel(1);
    on_core(move |provided| {
        let tabs = provided
            .all::<TabsState>()
            .into_iter()
            .next()
            .ok_or_else(|| "The tab manager is not available".to_owned())?;
        tabs.open_documents(Box::new(move |result| {
            let _ = send.send(result);
        }));
        Ok::<(), String>(())
    })
    .ok_or_else(|| "Core services are not available".to_owned())??;
    receive
        .recv()
        .map_err(|_| "Document listing was interrupted".to_owned())?
}

impl OpenFiles for ShellOpenFiles {
    fn open_files(&self) -> Result<Vec<OpenFileState>, OpenFilesSourceFailed> {
        open_documents()
            .map(|documents| {
                documents
                    .into_iter()
                    .map(|document| OpenFileState {
                        file_path: document.file_path,
                        dirty: document.dirty,
                    })
                    .collect()
            })
            .map_err(|_| OpenFilesSourceFailed)
    }
}

pub(super) struct ShellLanguage(pub(super) Arc<dyn shell_contract::runtime::Thread>);

impl UiLanguage for ShellLanguage {
    fn lang(&self) -> Lang {
        if let Ok(forced) = std::env::var("SEUOFFICE_LANG") {
            return normalize_lang(Some(&forced));
        }
        let settings = read_app_settings(&ShellPaths::new().settings_file().to_string_lossy());
        let locale = super::menu::first_accept_language(&self.0);
        install_lang(settings.get("language"), locale.as_deref())
    }
}

impl DocsTabs for ShellDocsTabs {
    fn open_tab_owned(
        &self,
        path: Option<String>,
        new_blank: bool,
        origin: shell_contract::ipc::NativeRequestOwner,
        cancel: shell_contract::runtime::CancelToken,
        pending: shell_contract::runtime::CancelToken,
        done: Box<dyn FnOnce(Result<(), String>) + Send>,
    ) {
        let completion = Arc::new(std::sync::Mutex::new(Some(done)));
        let unavailable = completion.clone();
        super::w::post_core_checked(
            move |provided, _| {
                let Some(tabs) = provided.all::<TabsState>().into_iter().next() else {
                    let done = completion
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .take();
                    if let Some(done) = done {
                        done(Err("The tab manager is not available".into()));
                    }
                    return;
                };
                let request = main_core::shell::tab_manager::EditorOpen::Docs {
                    path,
                    options: Some(OpenDocsOptions {
                        new_blank,
                        ai_content: None,
                    }),
                };
                let request_origin = origin.clone();
                let request_cancel = cancel.clone();
                let request_pending = pending.clone();
                tabs.open_initialized_editor_owned(
                    request,
                    origin,
                    Vec::new(),
                    cancel,
                    pending,
                    None,
                    Box::new(move |result| {
                        let result = result.and_then(|receipt| {
                            receipt.with_terminal_commit_owned(
                                &request_origin,
                                &request_cancel,
                                &request_pending,
                                &mut || Ok(()),
                            )
                        });
                        let done = completion
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .take();
                        if let Some(done) = done {
                            done(result);
                        }
                    }),
                );
            },
            move || {
                let done = unavailable
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .take();
                if let Some(done) = done {
                    done(Err("Core services are not available".into()));
                }
            },
        );
    }

    fn list_tabs(&self) -> Vec<DocsTab> {
        on_core(|provided| {
            provided
                .all::<TabsState>()
                .into_iter()
                .next()
                .and_then(|tabs| tabs.with_manager(|manager, _| manager.list()))
                .unwrap_or_default()
                .into_iter()
                .filter(|tab| tab.kind == TabKind::Docs)
                .map(|tab| DocsTab {
                    id: tab.id,
                    title: tab.title,
                    focused: tab.active,
                })
                .collect()
        })
        .unwrap_or_default()
    }

    fn focus_tab(&self, id: &str) {
        let id = id.to_owned();
        on_core(move |provided| {
            if let Some(tabs) = provided.all::<TabsState>().into_iter().next() {
                tabs.with_manager(|manager, host| manager.activate_tab(host, &id));
            }
        });
    }
}
