//! Knowledge-base services backed by the shell's shared file and project services.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use jscompat::{JsonValue, json_parse};
use main_core::packages::electron_utils::atomic_write::write_json_atomic;
use main_core::packages::electron_utils::default_save_dir::{
    PathProvider, Thrown, configured_default_save_dir,
};
use main_core::packages::electron_utils::unsaved_files::{OpenFileState, OpenFilesSourceFailed};
use main_core::packages::project_store::ProjectReadResult;
use main_core::shell::generate::materials::ParsedFile;
use main_core::shell::knowledge_base::{IndexedHit, KbWithin};
use main_core::shell::mcp::headless_cli::{CreateFileOptions, create_file_via_cli};
use main_core::shell::projects::new_documents::NewDocumentPlacement;
use main_core::shell::projects::project_tools::IndexSearchOptions;
use main_core::shell::renderer_disposal::Failure;
use shell_contract::runtime::{AppPaths, PathName};
use shell_contract::{Hosts, ViewId};
use shell_ipc_a::knowledge_base::{KbServices, KbServicesHost, Placement, VisibleTest};
use shell_ipc_w::SharedSaveDirState;
use shell_ipc_w::editor_files::{FileParseHost, OpenFilesHost, ParsedKind};
use shell_ipc_w::home_files::{FileSearchHost, HomeFilesHost};
use shell_ipc_w::projects::{ProjectFilesHost, Projects};

use super::now_ms;
use crate::host::HostContext;
use crate::host::cli::BundledCli;
use crate::host::w::on_core;
use crate::paths::{ShellPaths, resources_path};

pub(super) fn install(hosts: &mut Hosts, cx: &HostContext) {
    let save_dir = hosts
        .get::<SharedSaveDirState>()
        .cloned()
        .unwrap_or_default();
    let services: KbServicesHost = Arc::new(ShellKb {
        paths: ShellPaths::new(),
        resources: resources_path(&cx.app),
        save_dir,
        parser: hosts
            .get::<FileParseHost>()
            .cloned()
            .expect("W file parser"),
        open_files: hosts.get::<OpenFilesHost>().cloned().expect("W open files"),
        search: hosts
            .get::<FileSearchHost>()
            .cloned()
            .expect("W file search"),
        project_files: hosts
            .get::<ProjectFilesHost>()
            .cloned()
            .expect("W project files"),
        home: hosts.get::<HomeFilesHost>().cloned().expect("W home files"),
    });
    hosts.insert(services);
    hosts.insert(shell_ipc_a::knowledge_base::factory());
}

struct ShellKb {
    paths: ShellPaths,
    resources: Option<PathBuf>,
    save_dir: SharedSaveDirState,
    parser: FileParseHost,
    open_files: OpenFilesHost,
    search: FileSearchHost,
    project_files: ProjectFilesHost,
    home: HomeFilesHost,
}

/// `app.getPath` for `default-save-dir.ts`
struct Paths<'a>(&'a ShellPaths);

impl PathProvider for Paths<'_> {
    fn get_path(&self, name: &str) -> Result<String, Thrown> {
        let name = match name {
            "home" => PathName::Home,
            "documents" => PathName::Documents,
            _ => PathName::UserData,
        };
        Ok(self.0.path(name).to_string_lossy().into_owned())
    }
}

impl KbServices for ShellKb {
    fn has_file_index(&self) -> Result<bool, Thrown> {
        self.search.available().map_err(Thrown)
    }

    fn refresh_file_index_if_stale(&self, max_age_ms: u64) {
        self.search.refresh_if_stale(max_age_ms as f64);
    }

    fn paths_indexed_as_on_disk(&self, paths: &[String]) -> Result<Vec<String>, Thrown> {
        self.project_files.paths_indexed_as_on_disk(paths)
    }

    fn index_search(
        &self,
        q: &str,
        limit: f64,
        within: KbWithin<'_>,
    ) -> Result<Vec<IndexedHit>, Thrown> {
        let mut within = within;
        let mut filter = |path: &str| match within.as_mut() {
            Some(test) => test(path),
            None => Ok(true),
        };
        let mut options = IndexSearchOptions {
            offset: 0.0,
            limit,
            exts: None,
            within: &mut filter,
        };
        Ok(self
            .project_files
            .search(q, &mut options)?
            .hits
            .into_iter()
            .map(|hit| IndexedHit {
                path: hit.path,
                name: hit.name,
                ext: hit.ext,
                snippet_text: hit.snippet.map(|parts| parts.concat()),
            })
            .collect())
    }

    fn open_files(&self) -> Result<Vec<OpenFileState>, OpenFilesSourceFailed> {
        self.open_files.open_files()
    }

    fn parse_file_to_text(&self, path: &str) -> Result<ParsedFile, Failure> {
        let parsed = self
            .parser
            .parse_file_to_text(Path::new(path))
            .map_err(Failure)?;
        Ok(ParsedFile {
            ok: parsed.ok,
            error: parsed.error,
            kind: Some(
                match parsed.kind {
                    ParsedKind::Text => "text",
                    ParsedKind::Image => "image",
                    ParsedKind::Unsupported => "unsupported",
                }
                .into(),
            ),
            text: parsed.text,
        })
    }

    fn project_available(&self, project_id: &str) -> Result<bool, Thrown> {
        let project_id = project_id.to_owned();
        on_core(move |provided| {
            let projects = provided
                .all::<Projects>()
                .into_iter()
                .next()
                .ok_or_else(|| Thrown("project services unavailable".into()))?;
            projects.project_available(&project_id)
        })
        .unwrap_or_else(|| Err(Thrown("project services unavailable".into())))
    }

    fn project_visible_test(&self, project_id: &str) -> Result<Option<VisibleTest>, Thrown> {
        if !self.project_available(project_id)? {
            return Ok(None);
        }
        let project_id = project_id.to_owned();
        Ok(Some(Arc::new(move |path| {
            let project_id = project_id.clone();
            let path = path.to_owned();
            on_core(move |provided| {
                let projects = provided
                    .all::<Projects>()
                    .into_iter()
                    .next()
                    .ok_or_else(|| Thrown("project services unavailable".into()))?;
                projects.project_visible(&project_id, &path)
            })
            .unwrap_or_else(|| Err(Thrown("project services unavailable".into())))
        })))
    }

    fn project_read(
        &self,
        owner: ViewId,
        project_id: &str,
        path: &str,
        offset: f64,
    ) -> Result<ProjectReadResult, Thrown> {
        let project_id = project_id.to_owned();
        let path = path.to_owned();
        let open_files = self.open_files.open_files();
        let step = on_core(move |provided| {
            let projects = provided
                .all::<Projects>()
                .into_iter()
                .next()
                .ok_or_else(|| Thrown("project services unavailable".into()))?;
            projects.prepare_project_read_with_open_files(
                owner,
                &project_id,
                &path,
                offset,
                open_files,
            )
        })
        .unwrap_or_else(|| Err(Thrown("project services unavailable".into())))?;
        use main_core::shell::projects::project_tools::{ProjectReadStep, finish_project_read};
        Ok(match step {
            ProjectReadStep::Done(result) => result,
            ProjectReadStep::Read { path, offset } => {
                finish_project_read(self.project_files.read_page(&path, offset))
            }
        })
    }

    fn new_document_placement(&self, owner: ViewId) -> Result<Option<Placement>, String> {
        on_core(move |provided| {
            let projects = provided
                .all::<Projects>()
                .into_iter()
                .next()
                .ok_or_else(|| "project services unavailable".to_string())?;
            let placer = projects.new_documents();
            let result = placer
                .borrow_mut()
                .placement_for(i64::from(owner.0))
                .map_err(|error| error.message)?;
            Ok(result.map(|placement| Placement {
                project_id: placement.project_id,
                root: placement.root,
                dir: placement.dir,
            }))
        })
        .unwrap_or_else(|| Err("project services unavailable".into()))
    }

    fn write_project_document(
        &self,
        placement: &Placement,
        file_name: &str,
        bytes: Vec<u8>,
    ) -> Result<String, String> {
        let placement = NewDocumentPlacement {
            project_id: placement.project_id.clone(),
            root: placement.root.clone(),
            dir: placement.dir.clone(),
        };
        let file_name = file_name.to_owned();
        on_core(move |provided| {
            let projects = provided
                .all::<Projects>()
                .into_iter()
                .next()
                .ok_or_else(|| "project services unavailable".to_string())?;
            let placer = projects.new_documents();
            let result = placer
                .borrow_mut()
                .write(&placement, &file_name, &bytes)
                .map_err(|error| error.message());
            result
        })
        .unwrap_or_else(|| Err("project services unavailable".into()))
    }

    /// `defaultSaveDir()` of docs-main
    fn default_save_dir(&self) -> Result<String, String> {
        let mut state = self
            .save_dir
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        configured_default_save_dir(&Paths(&self.paths), &mut state, now_ms()).map_err(|e| e.0)
    }

    fn markdown_to_docx(&self, markdown: &str, out: &str) -> Result<(), String> {
        let resources = self
            .resources
            .as_ref()
            .ok_or_else(|| "CLI resources unavailable".to_string())?;
        let runner = BundledCli::new(resources.clone());
        create_file_via_cli(
            &runner,
            &CreateFileOptions {
                file_type: "docx",
                input_name: "content.md",
                input_content: markdown,
                out,
                overwrite: false,
            },
        )
        .map(|_| ())
        .map_err(String::from)
    }

    fn record_recent_file(&self, path: &str) {
        let recent_path = self.paths.path(PathName::UserData).join("recent.json");
        let recent_path = recent_path.to_string_lossy().into_owned();
        let io_path = recent_path.clone();
        let path = path.to_owned();
        let changed = main_core::shell::recent_files::metadata::run_sync(&recent_path, move || {
            let mut recent = match std::fs::read_to_string(&io_path)
                .ok()
                .and_then(|text| json_parse(&text).ok())
            {
                Some(JsonValue::Array(items)) => items
                    .into_iter()
                    .filter_map(|item| match item {
                        JsonValue::String(path) => Some(path),
                        _ => None,
                    })
                    .collect::<Vec<_>>(),
                _ => Vec::new(),
            };
            if recent.first().is_some_and(|first| first == &path) {
                return Ok(false);
            }
            recent.retain(|item| item != &path);
            recent.insert(0, path);
            recent.truncate(100);
            let value = JsonValue::Array(recent.into_iter().map(JsonValue::String).collect());
            write_json_atomic(&io_path, &value)?;
            Ok(true)
        })
        .unwrap_or(true);
        if changed {
            self.home.recents_changed();
        }
    }
}
