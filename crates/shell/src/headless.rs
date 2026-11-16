use std::fs::File;
use std::io::Write;
use std::sync::{Arc, LazyLock, Mutex, mpsc};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use main_core::packages::electron_utils::headless_export::{
    HeadlessArgvParse, HeadlessExportOutcome, format_headless_envelope, headless_exit_code,
    parse_headless_export_argv,
};
use main_core::packages::electron_utils::headless_mode::HeadlessMode;
use main_core::shell::headless_export::{
    HeadlessExportProduct, HeadlessFs, HeadlessRun, HeadlessStep, HiddenContent,
    begin_headless_export, finish_headless_export,
};
use main_core::shell::mcp::cli_runner::{CliRunOptions, CliRunner};
use shell_contract::runtime::Services;
use tauri::AppHandle;

use crate::host::HostContext;

static PARSED: LazyLock<HeadlessArgvParse> =
    LazyLock::new(|| parse_headless_export_argv(&std::env::args().collect::<Vec<_>>()));
static MODE: LazyLock<HeadlessMode> = LazyLock::new(|| {
    let mode = HeadlessMode::new();
    mode.set_headless_mode(!matches!(&*PARSED, HeadlessArgvParse::None));
    mode
});

pub(crate) fn is_headless() -> bool {
    MODE.is_headless_mode()
}

pub(crate) type Output = Arc<Mutex<Option<File>>>;

fn json_mode() -> bool {
    match &*PARSED {
        HeadlessArgvParse::Ok(request) => request.json,
        HeadlessArgvParse::Error { json, .. } => *json,
        HeadlessArgvParse::None => false,
    }
}

pub(crate) fn startup_failed(app: AppHandle, services: &Services, output: Output, message: String) {
    crate::host::quit::startup_failed();
    let completion = Completion {
        app,
        output,
        json: json_mode(),
    };
    services
        .workers
        .spawn(Box::new(move || completion.finish(failed(message))));
}

struct Disk;
impl HeadlessFs for Disk {
    fn exists(&mut self, path: &str) -> bool {
        std::path::Path::new(path).exists()
    }
    fn is_file(&mut self, path: &str) -> bool {
        std::path::Path::new(path).is_file()
    }
    fn realpath(&mut self, path: &str) -> String {
        std::fs::canonicalize(path).map_or_else(
            |_| path.to_owned(),
            |path| path.to_string_lossy().into_owned(),
        )
    }
    fn has_mtime(&self) -> bool {
        true
    }
    fn mtime_ms(&mut self, path: &str) -> f64 {
        std::fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map_or(f64::NAN, |time| time.as_secs_f64() * 1000.0)
    }
    fn has_read(&self) -> bool {
        true
    }
    fn read(&mut self, path: &str) -> Result<Vec<u8>, String> {
        std::fs::read(path).map_err(|error| error.to_string())
    }
}
struct Hidden;
impl HiddenContent for Hidden {
    fn file_holds_hidden_content(&mut self, bytes: &[u8], ext: &str) -> Result<bool, String> {
        file_parse::hidden_content::file_holds_hidden_content(bytes, ext)
    }
}

struct Completion {
    app: AppHandle,
    output: Output,
    json: bool,
}
impl Completion {
    fn finish(&self, outcome: HeadlessExportOutcome) {
        if let Some(code) = write_terminal(&self.output, outcome, self.json) {
            self.app.exit(code);
        }
    }
}

pub(crate) fn build_failed(output: &Output, message: String) -> i32 {
    write_terminal(output, failed(message), json_mode()).unwrap_or(3)
}

fn write_terminal(output: &Output, outcome: HeadlessExportOutcome, json: bool) -> Option<i32> {
    let mut output = output
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take()?;
    let code = headless_exit_code(&outcome) as i32;
    let line = format!("{}\n", format_headless_envelope(&outcome, json));
    let (tx, rx) = mpsc::sync_channel(1);
    let writer = std::thread::Builder::new()
        .name("headless-stdout".into())
        .spawn(move || {
            let result = output
                .write_all(line.as_bytes())
                .and_then(|()| output.flush());
            let _ = tx.send(result);
        });
    if let Err(error) = writer {
        eprintln!("headless stdout could not start: {error}");
        return Some(3);
    }
    Some(match rx.recv_timeout(Duration::from_secs(2)) {
        Ok(Ok(())) | Err(mpsc::RecvTimeoutError::Timeout) => code,
        Ok(Err(error)) => {
            eprintln!("headless stdout failed: {error}");
            3
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => 3,
    })
}
fn failed(message: String) -> HeadlessExportOutcome {
    HeadlessExportOutcome::Failed { code: 3.0, message }
}

pub(crate) fn start(
    cx: HostContext,
    services: Services,
    output: Output,
    shutdown: shell_contract::runtime::CancelToken,
) {
    let parsed = (*PARSED).clone();
    let completion = Arc::new(Completion {
        app: cx.app.clone(),
        output,
        json: json_mode(),
    });
    let workers = services.workers.clone();
    workers.spawn(Box::new(move || {
        let request = match parsed {
            HeadlessArgvParse::Ok(request) => request,
            HeadlessArgvParse::Error { message, .. } => {
                completion.finish(HeadlessExportOutcome::Failed { code: 1.0, message });
                return;
            }
            HeadlessArgvParse::None => {
                completion.finish(failed("headless entry has no invocation".into()));
                return;
            }
        };
        let mut now = || {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0.0, |time| time.as_secs_f64() * 1000.0)
        };
        let pending = match begin_headless_export(&request, &mut Disk, &mut Hidden, true, &mut now)
        {
            Ok(HeadlessStep::Done(outcome)) => {
                completion.finish(outcome);
                return;
            }
            Ok(HeadlessStep::Export(pending)) => pending,
            Err(error) => {
                completion.finish(failed(error));
                return;
            }
        };
        if let HeadlessRun::Placeholders {
            input,
            out_path,
            format,
        } = &pending.run
        {
            let result = placeholders(&cx.app, input, out_path, format);
            completion.finish(finish_headless_export(pending, result, &mut Disk));
            return;
        }
        let HeadlessRun::Exporter {
            module,
            input,
            out_path,
            format,
        } = pending.run.clone()
        else {
            unreachable!()
        };
        let surface = match module {
            "docs" => shell_contract::Surface::Docs,
            "sheets" => shell_contract::Surface::Sheets,
            "slides" => shell_contract::Surface::Slides,
            "markdown" => shell_contract::Surface::Markdown,
            "html" => shell_contract::Surface::Html,
            "hwp" => shell_contract::Surface::Hwp,
            "diagram" => shell_contract::Surface::Diagram,
            _ => {
                completion.finish(failed(format!(
                    "unknown validated headless module: {module}"
                )));
                return;
            }
        };
        let issuer = shell_contract::ipc::NativeInvocationIssuer::new(shutdown);
        let operation = issuer.issue(
            shell_contract::ipc::NativeInvocationSelection::HeadlessExport {
                surface,
                input: input.clone().into(),
                output: out_path.clone().into(),
                format: format.into(),
                page: request.page,
                ops: request.ops.clone(),
                batch: request.batch.map(str::to_owned),
            },
            shell_contract::runtime::CancelToken::default(),
        );
        let unavailable = completion.clone();
        let unavailable_operation = operation.clone();
        crate::host::w::post_core_checked(
            move |_, _| {
                let after = completion.clone();
                let workers = services.workers.clone();
                let finished_operation = operation.clone();
                let done = Box::new(move |result| {
                    finished_operation.finish();
                    workers.spawn(Box::new(move || {
                        after.finish(finish_headless_export(pending, result, &mut Disk))
                    }));
                });
                if let Err(error) = export(
                    &cx, module, &input, &out_path, format, &request, &operation, done,
                ) {
                    operation.finish();
                    services
                        .workers
                        .spawn(Box::new(move || completion.finish(failed(error))));
                }
            },
            move || {
                unavailable_operation.finish();
                unavailable.finish(failed("the headless core dispatcher is unavailable".into()));
            },
        );
    }));
}

fn placeholders(
    app: &AppHandle,
    input: &str,
    out: &str,
    format: &str,
) -> Result<Option<HeadlessExportProduct>, String> {
    let resources =
        crate::paths::resources_path(app).ok_or("application resources are unavailable")?;
    let result = crate::host::cli::BundledCli::new(resources).run(
        &[
            "convert".into(),
            input.into(),
            "--to".into(),
            format.into(),
            "--out".into(),
            out.into(),
            "--force".into(),
        ],
        &CliRunOptions {
            input: None,
            timeout_ms: Some(300_000.0),
        },
    );
    if result.ok {
        return Ok(None);
    }
    let message = result
        .json
        .as_ref()
        .and_then(|json| json.get("message"))
        .and_then(|value| value.as_str().map(str::to_owned))
        .filter(|message| !message.is_empty())
        .unwrap_or_else(|| result.stderr.trim().to_owned());
    Err(if message.is_empty() {
        format!("the conversion to {format} failed")
    } else {
        message
    })
}

type ExportDone = Box<dyn FnOnce(Result<Option<HeadlessExportProduct>, String>) + Send>;
fn export(
    cx: &HostContext,
    module: &str,
    input: &str,
    out: &str,
    format: &str,
    request: &main_core::packages::electron_utils::headless_export::HeadlessExportRequest,
    operation: &shell_contract::ipc::NativeInvocationOperation,
    done: ExportDone,
) -> Result<(), String> {
    match module {
        "docs" => crate::host::d2::docs_headless::export_headless(
            cx, input, out, format, request, operation, done,
        ),
        "sheets" => crate::host::d1::sheets_headless::export_headless(
            cx, input, out, format, request, operation, done,
        ),
        "slides" => crate::host::d1::slides_shell::export_headless(
            cx, input, out, format, request, operation, done,
        ),
        "markdown" => crate::host::d2::markdown_headless::export_headless(
            cx, input, out, format, request, operation, done,
        ),
        "html" => crate::host::d2::html_headless::export_headless(
            cx, input, out, format, request, operation, done,
        ),
        "hwp" => crate::host::d2::hwp_headless::export_headless(
            cx, input, out, format, request, operation, done,
        ),
        "diagram" => crate::host::d1::diagram_headless::export_headless(
            cx, input, out, format, request, operation, done,
        ),
        _ => Err(format!("unknown validated headless module: {module}")),
    }
}
