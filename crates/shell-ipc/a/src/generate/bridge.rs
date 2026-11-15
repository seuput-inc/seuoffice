//! The generate window's `main-core` module runs against a host trait
//! ([`GenerateWindowHost`]) made of Electron objects and the shell entry's
//! deps. [`Bridge`] is that host: the shared hosts (dialogs, the OS shell,
//! the main window, the event sink) and the batch host [`GenerateShell`],
//! one call each, named after the TypeScript call in its doc line.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use jscompat::JsonValue;
use main_core::packages::i18n::Lang;
use main_core::shell::app_routing::DocumentTabKind;
use main_core::shell::generate::convert::{
    ConvertEngines, ConvertResult, NoticeHost, OpOutcome, XmindTopic,
};
use main_core::shell::generate::generate_api::GenerateWindowState;
use main_core::shell::generate::generate_window::{
    GenerateText, GenerateWindowHost, GenerateWindowOptions, OpenDialogResult,
    WindowId,
};
use main_core::shell::generate::materials::{FileParser, FileStat, InspectHost, ParsedFile};
use main_core::shell::generate::open_materials::{MaterialContents, OpenMaterialsDeps};
use main_core::shell::mcp::pptx_outline::{
    MAX_OUTLINE_SLIDES, OutlineSlide, PptxSourceFormat, outline_to_ops, parse_pptx_outline,
};
use main_core::shell::renderer_disposal::Failure;
use main_core::shell::title_bar_overlay::TitleBarOverlay;
use serde_json::Value;
use shell_contract::hosts::{FileDialog, FileFilter};
use shell_contract::{ViewId, WindowId as ShellWindowId};

use super::host::{GenerateAuthority, GenerateShell};
use super::{GenerateWindowEvents, Inner};

pub(super) fn inspect_job(inner: &Arc<Inner>, authority: &GenerateAuthority, job: main_core::shell::generate::materials::MaterialInspectJob)
    -> Result<main_core::shell::generate::materials::MaterialStatus, Failure> {
    inner.check_authority(authority).map_err(Failure)?;
    let result = job.execute(&mut Bridge::new(inner, authority));
    inner.check_authority(authority).map_err(Failure)?;
    result
}

pub(super) fn notice_job(inner: &Arc<Inner>, authority: &GenerateAuthority, job: main_core::shell::generate::convert::MarkdownNoticeJob) -> Result<bool, Failure> {
    inner.check_authority(authority).map_err(Failure)?;
    let result = job.execute(&mut NoticeReads(&*inner.shell, authority));
    inner.check_authority(authority).map_err(Failure)?;
    Ok(result)
}

pub(super) fn convert_job(inner: &Arc<Inner>, authority: &GenerateAuthority, job: main_core::shell::generate::convert::ConversionJob) -> Result<ConvertResult, Failure> {
    inner.check_authority(authority).map_err(Failure)?;
    let result = job.execute(&mut Engines(&*inner.shell, authority));
    inner.check_authority(authority).map_err(Failure)?;
    Ok(result)
}


/// The host of one handler's call into the module
pub(super) struct Bridge<'a> {
    pub(super) inner: &'a Arc<Inner>,
    pub(super) authority: &'a GenerateAuthority,
}

impl<'a> Bridge<'a> {
    pub(super) fn new(inner: &'a Arc<Inner>, authority: &'a GenerateAuthority) -> Self {
        Bridge {
            inner,
            authority,
        }
    }

    fn shell(&self) -> &dyn GenerateShell {
        &*self.inner.shell
    }

    fn check(&self) -> Result<(), Failure> {
        self.authority.check_cancelled().map_err(Failure)?;
        self.shell().check_authority(self.authority).map_err(Failure)
    }

    /// `win.webContents`: the shell window's page is the main window's view
    /// (the shared host knows it), any other window's is the host's
    fn view_of(&self, window: WindowId) -> Result<Option<ViewId>, Failure> {
        self.check()?;
        let main = &self.inner.main_window;
        if main.window() == Some(shell_window(window)) {
            return Ok(main.view());
        }
        self.shell().window_view(self.authority, shell_window(window)).map_err(Failure)
    }
}

/// the shell's id of a window the module names
fn shell_window(window: WindowId) -> ShellWindowId {
    ShellWindowId(window as u32)
}

/// `Date`-less `stat`: the modification time in milliseconds
fn mtime_ms(meta: &fs::Metadata) -> Option<f64> {
    let since = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    Some(since.as_secs_f64() * 1000.0)
}

/// `stat(path)` as the materials read it
pub(super) fn real_stat(path: &str) -> Result<FileStat, Failure> {
    let meta = fs::metadata(path).map_err(|e| Failure(e.to_string()))?;
    Ok(FileStat {
        is_file: meta.is_file(),
        size: meta.len() as f64,
        mtime_ms: mtime_ms(&meta),
    })
}

/// the dialog options the module built, as the shared host takes them
pub(super) fn file_dialog(options: &Value) -> FileDialog {
    let text = |value: Option<&Value>| value.and_then(Value::as_str).map(str::to_owned);
    let list = |value: Option<&Value>| -> Vec<String> {
        value
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    };
    FileDialog {
        title: text(options.get("title")),
        default_path: text(options.get("defaultPath")).map(PathBuf::from),
        button_label: text(options.get("buttonLabel")),
        filters: options
            .get("filters")
            .and_then(Value::as_array)
            .map(|filters| {
                filters
                    .iter()
                    .map(|f| FileFilter {
                        name: text(f.get("name")).unwrap_or_default(),
                        extensions: list(f.get("extensions")),
                    })
                    .collect()
            })
            .unwrap_or_default(),
        properties: list(options.get("properties")),
    }
}

/// what `webContentsFor(tabId)` answered
pub(super) struct TabHandle {
    id: f64,
    destroyed: bool,
}

impl MaterialContents for TabHandle {
    fn id(&self) -> f64 {
        self.id
    }

    fn is_destroyed(&self) -> bool {
        self.destroyed
    }
}

impl OpenMaterialsDeps for Bridge<'_> {
    type Contents = TabHandle;

    fn web_contents_for(&mut self, tab_id: &str) -> Result<Option<TabHandle>, Failure> {
        self.shell().tab_contents(self.authority, tab_id).map(|value| value.map(|c| TabHandle {
            id: c.view.0 as f64,
            destroyed: c.destroyed,
        })).map_err(Failure)
    }


    fn has_save(&self, kind: DocumentTabKind) -> bool {
        self.shell().has_save(kind)
    }

    fn warn_save_failed(&mut self, error: &Failure) {
        eprintln!("[generate] saving a material failed: {}", error.0);
    }
}

impl InspectHost for Bridge<'_> {
    fn stat(&mut self, path: &str) -> Result<FileStat, Failure> {
        self.check()?;
        let value = real_stat(path)?;
        self.check()?;
        Ok(value)
    }

    fn parse(&mut self, path: &str) -> Result<ParsedFile, Failure> {
        self.shell().parse_file_to_text(self.authority, path).map_err(Failure)
    }

    fn injected_parse(&self) -> bool {
        false
    }
}

/// The conversions' engines: the outline reader and the op builder are
/// `main-core`'s; the rest is the host's
struct Engines<'a>(&'a dyn GenerateShell, &'a GenerateAuthority);

impl Drop for Engines<'_> {
    fn drop(&mut self) {
        self.0.release_conversion_resources(self.1);
    }
}

impl FileParser for Engines<'_> {
    fn parse_file_to_text(&mut self, path: &str) -> Result<ParsedFile, Failure> {
        self.0.parse_file_to_text(self.1, path).map_err(Failure)
    }
}

impl ConvertEngines for Engines<'_> {
    type Slides = Vec<OutlineSlide>;
    type Op = JsonValue;
    type Deck = u64;

    fn not_topic_marks(&self) -> [String; 2] {
        self.0.not_topic_marks()
    }

    fn first_sheet_root(&mut self, outline: &str, untitled: &str) -> Result<XmindTopic, Failure> {
        self.0.first_sheet_root(self.1, outline, untitled).map_err(Failure)
    }

    fn create_xmind_document_from_outline(
        &mut self,
        text: &str,
        untitled: &str,
    ) -> Result<(), Failure> {
        self.0
            .create_xmind_document_from_outline(self.1, text, untitled)
            .map_err(Failure)
    }

    fn parse_markdown_outline(&mut self, markdown: &str) -> Result<Self::Slides, Failure> {
        parse_pptx_outline(PptxSourceFormat::Markdown, markdown).map_err(Failure)
    }

    fn slide_count(&self, slides: &Self::Slides) -> usize {
        slides.len()
    }

    fn max_outline_slides(&self) -> usize {
        MAX_OUTLINE_SLIDES
    }

    fn outline_to_ops(&mut self, slides: &Self::Slides) -> Result<Vec<JsonValue>, Failure> {
        outline_to_ops(slides).map_err(Failure)
    }

    fn open_blank_deck(&mut self) -> Result<u64, Failure> {
        self.0.open_blank_deck(self.1).map_err(Failure)
    }

    fn apply_op(&mut self, deck: &mut u64, op: &JsonValue) -> Result<OpOutcome, Failure> {
        self.0.apply_deck_op(self.1, *deck, op).map_err(Failure)
    }

    fn op_name(&self, op: &JsonValue) -> String {
        match op.get("op").as_deref() {
            Some(JsonValue::String(name)) => name.clone(),
            _ => "undefined".to_owned(),
        }
    }

    fn save_deck(&mut self, deck: u64) -> Result<Vec<u8>, Failure> {
        self.0.save_deck(self.1, deck).map_err(Failure)
    }
}

/// the reads of the default `markdownRedactionNotice(path)`
struct NoticeReads<'a>(&'a dyn GenerateShell, &'a GenerateAuthority);

impl FileParser for NoticeReads<'_> {
    fn parse_file_to_text(&mut self, path: &str) -> Result<ParsedFile, Failure> {
        self.0.parse_file_to_text(self.1, path).map_err(Failure)
    }
}

impl NoticeHost for NoticeReads<'_> {
    fn stat(&mut self, path: &str) -> Result<FileStat, Failure> {
        self.1.check_cancelled().map_err(Failure)?;
        self.0.check_authority(self.1).map_err(Failure)?;
        let stat = real_stat(path)?;
        self.1.check_cancelled().map_err(Failure)?;
        self.0.check_authority(self.1).map_err(Failure)?;
        Ok(stat)
    }

    fn read_bytes(&mut self, path: &str) -> Result<Vec<u8>, Failure> {
        self.1.check_cancelled().map_err(Failure)?;
        self.0.check_authority(self.1).map_err(Failure)?;
        let bytes = fs::read(path).map_err(|e| Failure(e.to_string()))?;
        self.1.check_cancelled().map_err(Failure)?;
        self.0.check_authority(self.1).map_err(Failure)?;
        Ok(bytes)
    }

    fn injected_reader(&self) -> bool {
        false
    }
}

impl GenerateWindowHost for Bridge<'_> {
    fn parent_window(&mut self) -> Result<Option<WindowId>, Failure> {
        self.check()?;
        Ok(self.inner.main_window.window().map(|w| i64::from(w.0)))
    }

    fn lang(&self) -> Lang {
        self.shell().lang()
    }

    fn theme(&self) -> String {
        self.shell().theme()
    }

    fn can_open_converted(&self) -> bool {
        self.shell().can_open_converted()
    }


    fn starred_group_paths(&mut self, group: &str) -> Vec<String> {
        self.shell().starred_group_paths(group)
    }


    fn new_id(&mut self) -> String {
        self.shell().new_id()
    }

    fn now(&mut self) -> f64 {
        self.shell().now_ms()
    }

    fn max_convert_slides(&self) -> usize {
        MAX_OUTLINE_SLIDES
    }

    fn generate_text(&self, lang: Lang, key: GenerateText) -> String {
        self.shell().generate_text(lang, key)
    }

    fn should_use_dark_colors(&self) -> Result<bool, Failure> {
        self.shell().should_use_dark_colors(self.authority).map_err(Failure)
    }

    fn is_darwin(&self) -> bool {
        crate::secrets::PLATFORM == "darwin"
    }

    fn renderer_url(&self) -> Option<String> {
        self.shell().renderer_url()
    }

    fn create_window(&mut self, options: &GenerateWindowOptions) -> Result<WindowId, Failure> {
        let events = GenerateWindowEvents::new(self.inner.clone());
        self.shell().create_window(self.authority, options, events)
            .map(|window| i64::from(window.0)).map_err(Failure)
    }

    fn window_is_destroyed(&self, window: WindowId) -> Result<bool, Failure> {
        self.shell().window_is_destroyed(self.authority, shell_window(window)).map_err(Failure)
    }

    fn window_is_minimized(&self, window: WindowId) -> Result<bool, Failure> {
        self.shell().window_is_minimized(self.authority, shell_window(window)).map_err(Failure)
    }

    fn window_restore(&mut self, window: WindowId) -> Result<(), Failure> {
        self.shell().window_restore(self.authority, shell_window(window)).map_err(Failure)
    }

    fn window_show(&mut self, window: WindowId) -> Result<(), Failure> {
        self.shell().window_show(self.authority, shell_window(window)).map_err(Failure)
    }

    fn window_focus(&mut self, window: WindowId) -> Result<(), Failure> {
        self.shell().window_focus(self.authority, shell_window(window)).map_err(Failure)
    }

    fn window_close(&mut self, window: WindowId) -> Result<(), Failure> {
        self.shell().window_close(self.authority, shell_window(window)).map_err(Failure)
    }

    fn window_remove_menu(&mut self, window: WindowId) -> Result<(), Failure> {
        self.shell().window_remove_menu(self.authority, shell_window(window)).map_err(Failure)
    }

    fn window_web_contents_id(&self, window: WindowId) -> Result<f64, Failure> {
        self.view_of(window)?.map(|ViewId(id)| f64::from(id))
            .ok_or_else(|| Failure("the generate window has no live contents".into()))
    }

    fn window_send_state(&mut self, window: WindowId, state: &GenerateWindowState) -> Result<(), Failure> {
        let view = self.view_of(window)?
            .ok_or_else(|| Failure("the generate window has no live contents".into()))?;
        self.shell().send_state(self.authority, self.inner.services.events.clone(), view, state).map_err(Failure)
    }

    fn window_set_title_bar_overlay(
        &mut self,
        window: WindowId,
        overlay: &TitleBarOverlay,
    ) -> Result<(), Failure> {
        self.shell()
            .window_set_title_bar_overlay(self.authority, shell_window(window), overlay)
            .map_err(Failure)
    }

    fn watch_theme(&mut self, window: WindowId) -> Result<(), Failure> {
        self.shell().watch_theme(self.authority, shell_window(window)).map_err(Failure)
    }

    fn unwatch_theme(&mut self, window: WindowId) -> Result<(), Failure> {
        self.shell().unwatch_theme(self.authority, shell_window(window)).map_err(Failure)
    }

    fn window_load_url(&mut self, window: WindowId, url: &str) -> Result<(), Failure> {
        self.shell().window_load_url(self.authority, shell_window(window), url).map_err(Failure)
    }

    fn window_load_generate_page(&mut self, window: WindowId) -> Result<(), Failure> {
        self.shell().window_load_generate_page(self.authority, shell_window(window)).map_err(Failure)
    }

    fn open_external(&mut self, url: &str) -> Result<(), Failure> {
        self.shell().open_external(self.authority, self.inner.os_shell.clone(), url).map_err(Failure)
    }
}

/// `{ canceled, filePaths }` of what the shared host answered
pub(super) fn dialog_result(picked: Option<Vec<PathBuf>>) -> OpenDialogResult {
    match picked {
        None => OpenDialogResult {
            canceled: true,
            file_paths: Vec::new(),
        },
        Some(paths) => OpenDialogResult {
            canceled: false,
            file_paths: paths
                .into_iter()
                .map(|p| Value::String(p.to_string_lossy().into_owned()))
                .collect(),
        },
    }
}
