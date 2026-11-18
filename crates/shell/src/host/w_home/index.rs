use std::cell::RefCell;
use std::collections::HashSet;
use std::sync::{Arc, Condvar, Mutex};

use jscompat::JsonValue;
use main_core::packages::electron_utils::default_save_dir::Thrown;
use main_core::packages::electron_utils::unsaved_files::{OpenFilesHolder, OpenFilesSourceFailed};
use main_core::shell::file_index::rerank::{RerankStart, SearchReranker, normalize_file_search_settings};
use main_core::shell::file_index::store::SearchOptions;
use main_core::shell::generate::materials::{FileParser, ParsedFile};
use main_core::shell::knowledge_base::{KbReadPage, KbTextCache, read_knowledge_file_page};
use main_core::shell::projects::project_index::RerankIgnoreFilter;
use main_core::shell::projects::project_tools::{IndexHit, IndexSearchOptions, IndexSearchResult};
use main_core::shell::renderer_disposal::Failure;
use shell_contract::runtime::{AppPaths, PathName};
use shell_contract::{Hosts, IpcValue};
use shell_ipc_w::editor_files::{FileParseHost, OpenFilesHost, ParsedKind};
use shell_ipc_w::home_files::{FileSearch, FileSearchHost, IndexProgress, SearchHit, SearchResult};
use shell_ipc_w::projects::{ProjectFiles, ProjectFilesHost, Projects, ProjectsHostRef};
use shell_ipc_w::tabs::TabsState;

use crate::paths::ShellPaths;
use super::decision::DecisionTransport;
use super::index_scheduler::{IndexScheduler, IndexSources};
use super::index_sources;

struct Parser(FileParseHost);
impl FileParser for Parser {
    fn parse_file_to_text(&mut self, path: &str) -> Result<ParsedFile, Failure> {
        let value = self.0.parse_file_to_text(std::path::Path::new(path)).map_err(Failure)?;
        Ok(ParsedFile {
            ok: value.ok,
            error: value.error,
            kind: Some(match value.kind {
                ParsedKind::Text => "text", ParsedKind::Image => "image", ParsedKind::Unsupported => "unsupported",
            }.to_owned()),
            text: value.text,
        })
    }
}
/// The original shell catches only `new FileIndexStore(...)` failures here
/// (`apps/shell/src/main/index.ts`, `ensureFileIndexer`), and retries on the
/// next request. Later scheduler and query errors are not unavailable.
fn open_index_store(path: &str) -> Option<main_core::shell::file_index::store::FileIndexStore> {
    constructor_result(main_core::shell::file_index::store::FileIndexStore::new(path))
}

fn constructor_result<T>(result: Result<T, main_core::shell::file_index::store::StoreError>) -> Option<T> {
    match result {
        Ok(store) => Some(store),
        Err(error) => {
            eprintln!("[file-index] unavailable: {error}");
            None
        }
    }
}


enum IndexLifecycle {
    Running(Option<Arc<IndexScheduler>>),
    Opening,
    StoppingOpening,
    Stopping(Option<Arc<IndexScheduler>>),
    Stopped,
}

struct IndexHost {
    scheduler: Mutex<IndexLifecycle>,
    scheduler_changed: Condvar,
    sources: Arc<dyn IndexSources>,
    parser: FileParseHost,
    open_files: OpenFilesHost,
    cache: Mutex<KbTextCache>,
    reranker: Mutex<SearchReranker>,
}

impl IndexHost {
    fn ensure(&self) -> Result<Option<Arc<IndexScheduler>>, String> {
        let mut lifecycle = self.scheduler.lock().map_err(|error| error.to_string())?;
        loop {
            match &*lifecycle {
                IndexLifecycle::Running(Some(index)) => return Ok(Some(index.clone())),
                IndexLifecycle::Running(None) => { *lifecycle = IndexLifecycle::Opening; break; }
                IndexLifecycle::Opening => lifecycle = self.scheduler_changed.wait(lifecycle).map_err(|error| error.to_string())?,
                _ => return Err("file index is shutting down".into()),
            }
        }
        drop(lifecycle);
        let opened = (|| {
            let db = ShellPaths::new().path(PathName::UserData).join("file-index.db");
            let path = db.to_str().ok_or("index path is not UTF-8")?;
            let Some(store) = open_index_store(path) else { return Ok(None); };
            Ok(Some(Arc::new(IndexScheduler::new(store, self.sources.clone())?)))
        })();
        let mut lifecycle = self.scheduler.lock().map_err(|error| error.to_string())?;
        let stopping = matches!(*lifecycle, IndexLifecycle::StoppingOpening);
        let index = opened.as_ref().ok().and_then(|index: &Option<Arc<IndexScheduler>>| index.clone());
        *lifecycle = if stopping { IndexLifecycle::Stopping(index) } else { IndexLifecycle::Running(index) };
        self.scheduler_changed.notify_all();
        if stopping { return Err("file index is shutting down".into()); }
        opened
    }

    fn require_index(&self) -> Result<Arc<IndexScheduler>, String> {
        self.ensure()?.ok_or_else(|| "file index unavailable".into())
    }

    fn existing(&self) -> Option<Arc<IndexScheduler>> {
        match &*self.scheduler.lock().expect("index scheduler lock poisoned") {
            IndexLifecycle::Running(index) => index.clone(),
            IndexLifecycle::Opening | IndexLifecycle::StoppingOpening | IndexLifecycle::Stopping(_) | IndexLifecycle::Stopped => None,
        }
    }
}

fn to_ipc(value: &serde_json::Value) -> IpcValue {
    shell_ipc_d1::value::from_json(&jscompat::json_value_from_serde(value))
}

impl FileSearch for IndexHost {
    fn available(&self) -> Result<bool, String> {
        self.ensure().map(|index| index.is_some())
    }
    fn refresh_if_stale(&self, max_age_ms: f64) {
        if let Some(index) = self.existing() { index.refresh_if_stale(max_age_ms); }
    }
    fn refresh(&self) {
        if let Some(index) = self.existing() { index.refresh(); }
    }
    fn search(&self, query: &str, exts: Option<&[String]>, offset: f64, limit: f64) -> Result<SearchResult, String> {
        let index = self.require_index()?;
        let store = index.store.lock().map_err(|error| error.to_string())?;
        let result = store.search(query, &SearchOptions {
            exts: exts.map(<[String]>::to_vec), offset: Some(offset), limit: Some(limit), ..Default::default()
        }).map_err(|error| error.0)?;
        Ok(SearchResult {
            total: result.total as f64,
            hits: result.hits.into_iter().map(|hit| {
                let value = to_ipc(&serde_json::to_value(hit).expect("index hit serializes"));
                match value { IpcValue::Object(fields) => SearchHit(fields), _ => unreachable!() }
            }).collect(),
        })
    }
    fn progress(&self) -> Result<IndexProgress, String> {
        let progress = self.require_index()?.progress();
        Ok(IndexProgress { indexed: progress.indexed as f64, pending: progress.pending as f64, scanning: progress.scanning })
    }
    fn rerank(&self, query: &str, paths: &[String]) -> Result<Option<IpcValue>, String> {
        let index = self.require_index()?;
        let open = self.open_files.open_files();
        let mut holder = OpenFilesHolder::new();
        holder.set_open_files_source(Some(Box::new(move || open.clone())));
        let path = ShellPaths::new().settings_file();
        let settings = main_core::shell::app_settings::read_app_settings(&path.to_string_lossy());
        let raw = settings.get("fileSearch").cloned().unwrap_or(JsonValue::Null);
        let codec = super::super::secrets::CODEC.get().map(|codec| codec.as_ref() as &dyn main_core::packages::ai_provider::settings_secrets::AiSecretCodec);
        let settings = normalize_file_search_settings(&main_core::shell::ai_secrets::open_file_search_keys(codec, &raw));
        let indexed = index.store.lock().map_err(|error| error.to_string())?.paths_indexed_as_on_disk(paths).map_err(|error| error.0)?;
        let candidates = indexed.clone();
        let allowed = super::super::w::on_core(move |provided| {
            let projects = provided.all::<Projects>().into_iter().next().ok_or("project services unavailable")?;
            RerankIgnoreFilter::new(projects.guide(), None).sendable(&candidates).map_err(|error| error.0)
        }).ok_or("project services unavailable")??;
        let mut sendable = move |saved: &[String]| -> Result<Vec<String>, String> {
            Ok(saved.iter().filter(|path| allowed.contains(path)).cloned().collect())
        };
        let Ok(mut transport) = DecisionTransport::new() else { return Ok(None); };
        let mut reranker = self.reranker.lock().map_err(|error| error.to_string())?;
        let started = {
            let store = index.store.lock().map_err(|error| error.to_string())?;
            reranker.rerank(&store, &holder, Some(&mut sendable), &mut transport, query, &indexed, &settings).map_err(|error| error.0)?
        };
        Ok(match started {
            RerankStart::Ready(result) => result,
            RerankStart::Judge(job) => reranker.judge(job, &mut transport),
            RerankStart::Join(_) => unreachable!("reranker lock serializes requests"),
        }.map(|result| to_ipc(&result.to_json())))
    }
}

fn search_project_store(store: &Mutex<main_core::shell::file_index::store::FileIndexStore>, input: &str, options: &mut IndexSearchOptions<'_>) -> Result<IndexSearchResult, Thrown> {
        // Never call a caller-supplied predicate while holding the store lock:
        // the KB predicate may synchronously ask the core thread, which can
        // itself be waiting to search this store.
        let paths = {
            let store = store.lock().map_err(|error| Thrown(error.to_string()))?;
            let candidates = RefCell::new(Vec::new());
            let collect = |path: &str| { candidates.borrow_mut().push(path.to_owned()); false };
            store.search(input, &SearchOptions {
                exts: options.exts.clone(), offset: Some(0.0), limit: Some(0.0),
                excerpt_chars: None, within: Some(&collect),
            }).map_err(|error| Thrown(error.0))?;
            candidates.into_inner()
        };
        let mut allowed = HashSet::new();
        for path in &paths {
            if (options.within)(path)? { allowed.insert(path.clone()); }
        }
        let result = {
            let store = store.lock().map_err(|error| Thrown(error.to_string()))?;
            let current = RefCell::new(Vec::new());
            let filter = |path: &str| {
                current.borrow_mut().push(path.to_owned());
                allowed.contains(path)
            };
            let result = store.search(input, &SearchOptions {
                exts: options.exts.clone(), offset: Some(options.offset), limit: Some(options.limit),
                excerpt_chars: None, within: Some(&filter),
            }).map_err(|error| Thrown(error.0))?;
            if current.into_inner() != paths { return Err(Thrown("file index changed during search".into())); }
            result
        };
        // Authorization can change while the query runs; refuse the whole
        // page rather than leak a hit or change its ranking/total after paging.
        for path in &paths {
            if (options.within)(path)? != allowed.contains(path) {
                return Err(Thrown("project visibility changed during search".into()));
            }
        }
        Ok(IndexSearchResult {
            total: result.total as u64,
            hits: result.hits.into_iter().map(|hit| IndexHit {
                path: hit.path, name: hit.name, ext: hit.ext, mtime_ms: hit.mtime_ms,
                snippet: hit.snippet.map(|parts| parts.into_iter().map(|part| part.text).collect()),
            }).collect(),
        })
    }

impl ProjectFiles for IndexHost {
    fn search(&self, input: &str, options: &mut IndexSearchOptions<'_>) -> Result<IndexSearchResult, Thrown> {
        let index = self.require_index().map_err(Thrown)?;
        search_project_store(&index.store, input, options)
    }
    fn paths_indexed_as_on_disk(&self, paths: &[String]) -> Result<Vec<String>, Thrown> {
        let index = self.require_index().map_err(Thrown)?;
        let store = index.store.lock().map_err(|error| Thrown(error.to_string()))?;
        store.paths_indexed_as_on_disk(paths).map_err(|error| Thrown(error.0))
    }
    fn read_page(&self, path: &str, offset: f64) -> Result<KbReadPage, Thrown> {
        let open = self.open_files.open_files();
        let mut holder = OpenFilesHolder::new();
        holder.set_open_files_source(Some(Box::new(move || open.clone())));
        let mut parser = Parser(self.parser.clone());
        let mut cache = self.cache.lock().map_err(|error| Thrown(error.to_string()))?;
        read_knowledge_file_page(&holder, &mut cache, &mut parser, path, offset)
    }
    fn open_files_async(&self, done: Box<dyn FnOnce(Result<Vec<main_core::packages::electron_utils::unsaved_files::OpenFileState>, OpenFilesSourceFailed>)>) {
        let mut done = Some(done);
        let accepted = super::super::w::on_core_local(|provided| {
            let Some(tabs) = provided.all::<TabsState>().into_iter().next() else {
                done.take().expect("callback available")(Err(OpenFilesSourceFailed));
                return;
            };
            let done = done.take().expect("callback available");
            tabs.open_documents(Box::new(move |result| {
                done(result.map(|docs| docs.into_iter().map(|doc| main_core::packages::electron_utils::unsaved_files::OpenFileState {
                    file_path: doc.file_path, dirty: doc.dirty,
                }).collect()).map_err(|_| OpenFilesSourceFailed));
            }));
        });
        if !accepted { done.expect("callback available")(Err(OpenFilesSourceFailed)); }
    }
}

pub(super) fn install(hosts: &mut Hosts) {
    let save_dir = hosts.get::<shell_ipc_w::SharedSaveDirState>().expect("save directory state is unavailable").clone();
    let projects = hosts.get::<ProjectsHostRef>().expect("project host is unavailable").clone();
    let parser = hosts.get::<FileParseHost>().expect("file parser is unavailable").clone();
    let open_files = hosts.get::<OpenFilesHost>().expect("open file source is unavailable").clone();
    let host = Arc::new(IndexHost {
        scheduler: Mutex::new(IndexLifecycle::Running(None)), scheduler_changed: Condvar::new(), sources: index_sources::shared(save_dir, projects),
        parser, open_files, cache: Mutex::new(KbTextCache::default()), reranker: Mutex::new(SearchReranker::new()),
    });
    hosts.insert(host.clone());
    let search: FileSearchHost = host.clone();
    let files: ProjectFilesHost = host;
    hosts.insert(search);
    hosts.insert(files);
}

pub(super) fn shutdown(hosts: &Hosts) -> Result<(), String> {
    let Some(host) = hosts.get::<Arc<IndexHost>>() else { return Ok(()); };
    let index = {
        let mut lifecycle = host.scheduler.lock().map_err(|error| error.to_string())?;
        loop {
            match &*lifecycle {
                IndexLifecycle::Stopped => return Ok(()),
                IndexLifecycle::Opening => {
                    *lifecycle = IndexLifecycle::StoppingOpening;
                }
                IndexLifecycle::StoppingOpening => {}
                IndexLifecycle::Running(index) | IndexLifecycle::Stopping(index) => {
                    let index = index.clone();
                    *lifecycle = IndexLifecycle::Stopping(index.clone());
                    break index;
                }
            }
            lifecycle = host.scheduler_changed.wait(lifecycle).map_err(|error| error.to_string())?;
        }
    };
    if let Some(index) = index { index.shutdown()?; }
    *host.scheduler.lock().map_err(|error| error.to_string())? = IndexLifecycle::Stopped;
    Ok(())
}

#[cfg(test)]
mod project_search_tests {
    use super::*;
    use main_core::shell::file_index::scan::ScannedFile;
    use main_core::shell::file_index::store::{FileIndexStore, IndexStatus};
    use std::sync::mpsc;
    use std::time::Duration;

    fn store() -> Arc<Mutex<FileIndexStore>> {
        let store = FileIndexStore::new(":memory:").unwrap();
        for path in ["/project/apple.txt", "/other/apple.txt"] {
            store.upsert(&ScannedFile { path: path.into(), mtime_ms: 1.0, size_bytes: 10.0 }, Some("apple"), IndexStatus::Ok).unwrap();
        }
        Arc::new(Mutex::new(store))
    }

    #[test]
    fn predicate_waits_without_holding_the_real_index_lock() {
        let store = store();
        let worker_store = store.clone();
        let (request, requests) = mpsc::sync_channel(1);
        let (answer, answers) = mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || {
            let mut first = true;
            let mut within = |_: &str| {
                if first {
                    first = false;
                    request.send(()).unwrap();
                    answers.recv_timeout(Duration::from_secs(3)).map_err(|error| Thrown(error.to_string()))?;
                }
                Ok(true)
            };
            search_project_store(&worker_store, "apple", &mut IndexSearchOptions {
                offset: 0.0, limit: 10.0, exts: None, within: &mut within,
            })
        });
        requests.recv_timeout(Duration::from_secs(3)).unwrap();
        // This represents the core taking the index lock before servicing the
        // worker's posted visibility request. The old implementation held it.
        let acquired = store.try_lock().is_ok();
        answer.send(()).unwrap();
        let result = worker.join().unwrap().unwrap();
        assert!(acquired, "worker waited for core while holding index lock");
        assert_eq!(result.total, 2);
    }

    #[test]
    fn visibility_is_applied_before_page_and_revalidated() {
        let store = store();
        let mut within = |path: &str| Ok(path.starts_with("/project/"));
        let page = search_project_store(&store, "apple", &mut IndexSearchOptions {
            offset: 0.0, limit: 1.0, exts: None, within: &mut within,
        }).unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.hits[0].path, "/project/apple.txt");

        let mut calls = 0;
        let mut changed = |_: &str| { calls += 1; Ok(calls <= 2) };
        assert!(search_project_store(&store, "apple", &mut IndexSearchOptions {
            offset: 0.0, limit: 1.0, exts: None, within: &mut changed,
        }).is_err());

        let mut failed = |_: &str| Err(Thrown("visibility failed".into()));
        let error = search_project_store(&store, "apple", &mut IndexSearchOptions {
            offset: 0.0, limit: 1.0, exts: None, within: &mut failed,
        }).err().unwrap();
        assert_eq!(error.0, "visibility failed");
    }

    #[test]
    fn bounded_wait_for_model_excludes_the_old_cycle() {
        // One worker and one core request, with a single exclusive index lock.
        // Worker phases: snapshot, wait for core, query, done. Core phases:
        // acquire store, release store, answer visibility. In the old variant
        // the worker keeps the lock from snapshot through the core wait.
        fn reaches_cycle(old: bool) -> bool {
            let mut pending = vec![(0u8, 0u8, 0u8)];
            let mut seen = HashSet::new();
            while let Some((worker, core, owner)) = pending.pop() {
                if !seen.insert((worker, core, owner)) { continue; }
                if worker == 1 && core == 0 && owner == 1 { return true; }
                if worker == 0 && owner == 0 {
                    pending.push((1, core, if old { 1 } else { 0 }));
                }
                if worker == 1 && core == 3 {
                    pending.push((2, core, 0));
                }
                if worker == 2 && owner == 0 { pending.push((3, core, 0)); }
                if core == 0 && owner == 0 { pending.push((worker, 1, 2)); }
                if core == 1 && owner == 2 { pending.push((worker, 2, 0)); }
                if core == 2 && worker == 1 { pending.push((worker, 3, owner)); }
            }
            false
        }
        assert!(reaches_cycle(true));
        assert!(!reaches_cycle(false));
    }

    #[test]
    fn only_constructor_errors_use_the_original_unavailable_policy() {
        use main_core::shell::file_index::store::StoreError;
        // `ensureFileIndexer`, apps/shell/src/main/index.ts:1716–1723.
        assert!(constructor_result::<()>(Err(StoreError("cannot open".into()))).is_none());
        assert_eq!(constructor_result(Ok::<_, StoreError>(7)), Some(7));
    }
}

