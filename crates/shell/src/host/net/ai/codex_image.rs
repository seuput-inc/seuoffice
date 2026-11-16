//! Codex's built-in image generator, using the shared subscription runtime.
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{OnceLock, mpsc};
use std::time::Duration;

use crate::host::w::post_core;
use main_core::packages::ai_provider::subscription_locate::{
    SubscriptionLocateOptions, SubscriptionProgram, resolve_subscription_program,
};
use main_core::packages::ai_provider::subscription_process::{
    SharedChildRegistry, SubscriptionCommandResult, SubscriptionSpawnOptions, kill_process_tree_rt,
    spawn_subscription_program,
};
use main_core::packages::ai_provider::task::{ChildEvent, JsError, Rt, Signal, Stdio};
use shell_contract::runtime::CancelToken;
use shell_ipc_a::app_ai_image::CodexImage;
use shell_ipc_a::runtime::Driver;
use shell_ipc_a::subscription::SubscriptionShared;

const MAX_IMAGE: u64 = 25 * 1024 * 1024;
const MAX_OUTPUT: usize = 256 * 1024;
static USER_DATA: OnceLock<PathBuf> = OnceLock::new();
static NEXT: AtomicU64 = AtomicU64::new(1);
thread_local! { static RUNS: RefCell<HashMap<u64, (Driver, Signal)>> = RefCell::new(HashMap::new()); }

pub(crate) struct CodexImageRequest {
    pub prompt: String,
    pub aspect_ratio: Option<String>,
    pub image_size: Option<String>,
    pub transparent: bool,
}

pub(crate) fn configure(user_data: PathBuf) {
    let _ = USER_DATA.set(user_data);
}

fn configured_path() -> Option<String> {
    let bytes = std::fs::read(USER_DATA.get()?.join("ai-settings.json")).ok()?;
    let settings: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let path = settings
        .get("providers")?
        .get("codex")?
        .get("cliPath")?
        .as_str()?
        .trim();
    (!path.is_empty()).then(|| path.to_owned())
}

struct WorkDir(PathBuf);
impl WorkDir {
    fn create(root: &Path) -> Result<Self, JsError> {
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).map_err(|e| JsError::error(e.to_string()))?;
        let name = random
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        let path = root.join(format!("ieum-codex-image-{name}"));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(&path)
            .map_err(|e| JsError::error(e.to_string()))?;
        Ok(Self(path))
    }
}
impl Drop for WorkDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn args(request: &CodexImageRequest, work: &Path) -> Vec<String> {
    let size = match request.aspect_ratio.as_deref() {
        Some("16:9" | "3:2" | "4:3") => "1536x1024",
        Some("9:16" | "2:3" | "3:4") => "1024x1536",
        _ => "1024x1024",
    };
    let quality = match request.image_size.as_deref() {
        Some("0.5k") => "low",
        Some("1k") => "medium",
        Some("2k" | "3k" | "4k") => "high",
        _ => "auto",
    };
    // The original CLI route has no transparency control.
    let _ = request.transparent;
    let out = work.join("image.png").to_string_lossy().replace('\\', "/");
    let prompt = format!(
        "Perform the following tasks:\n1. Use the built-in image_gen tool to generate exactly one image.\n2. The prompt is between the markers below:\n<<<PROMPT\n{}\nPROMPT>>>\n3. Size: {size}\n4. Quality: {quality}\n5. Save the generated image as the absolute path \"{out}\". If the tool saved it somewhere else, copy it there.\n6. Print the saved file path and its size.\nDo not draw the image with code or SVG, and do not use any tool other than image_gen and the file copy.",
        request.prompt
    );
    let work = work.to_string_lossy().into_owned();
    [
        "exec".into(),
        prompt,
        "-C".into(),
        work,
        "-s".into(),
        "workspace-write".into(),
        "--skip-git-repo-check".into(),
        "--ephemeral".into(),
        "-c".into(),
        "features.image_generation=true".into(),
        "-c".into(),
        "features.plugins=false".into(),
        "-c".into(),
        "features.apps=false".into(),
        "-c".into(),
        "features.memories=false".into(),
        "-c".into(),
        "web_search=\"disabled\"".into(),
        "-c".into(),
        "history.persistence=\"none\"".into(),
        "-c".into(),
        "model_reasoning_effort=\"medium\"".into(),
    ]
    .into()
}

#[derive(Default)]
struct Run {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    error: Option<JsError>,
}

// The subscription runner keeps a whole arriving chunk when it starts below
// the shared limit, even if that chunk crosses it; later chunks are dropped.
fn collect(state: &mut Run, chunk: &[u8], stdout: bool) {
    if state.stdout.len() + state.stderr.len() >= MAX_OUTPUT {
        return;
    }
    if stdout {
        state.stdout.extend_from_slice(chunk);
    } else {
        state.stderr.extend_from_slice(chunk);
    }
}

// Unlike the short-command runner, wait for close after terminating the process:
// the temporary workspace must not be removed while its child still runs.
async fn run(
    rt: &Rt,
    shared: &SharedChildRegistry,
    program: &SubscriptionProgram,
    argv: &[String],
    cwd: String,
    signal: &Signal,
) -> Result<SubscriptionCommandResult, JsError> {
    if signal.aborted() {
        return Err(JsError::abort());
    }
    let child = spawn_subscription_program(
        rt,
        shared,
        program,
        argv,
        &SubscriptionSpawnOptions {
            cwd: Some(cwd),
            stdio: Some([Stdio::Ignore, Stdio::Pipe, Stdio::Pipe]),
            ..Default::default()
        },
    )
    .await?;
    let state = Rc::new(RefCell::new(Run::default()));
    let (settle, wait) = rt.deferred::<Result<SubscriptionCommandResult, JsError>>();
    let pid = child.pid();
    let abort_state = state.clone();
    let abort_rt = rt.clone();
    let listener = signal.on_abort(move |_| {
        abort_state.borrow_mut().error =
            Some(JsError::error("The command was cancelled").named("AbortError"));
        kill_process_tree_rt(&abort_rt, pid, true);
    });
    let timer_state = state.clone();
    let timer_rt = rt.clone();
    let timer = rt.set_timeout(180_000.0, move || {
        let mut state = timer_state.borrow_mut();
        if state.error.is_none() {
            state.error = Some(JsError::error(
                "The command did not finish within 180000 ms",
            ));
        }
        kill_process_tree_rt(&timer_rt, pid, true);
    });
    let event_state = state.clone();
    child.on(move |event| {
        let mut state = event_state.borrow_mut();
        match event {
            ChildEvent::Stdout(chunk) | ChildEvent::Stderr(chunk) => {
                collect(&mut state, chunk, matches!(event, ChildEvent::Stdout(_)));
            }
            ChildEvent::Error(error) => {
                if state.error.is_none() {
                    state.error = Some(error.clone());
                }
            }
            ChildEvent::Close { code, .. } => {
                let result = match state.error.take() {
                    Some(error) => Err(error),
                    None => Ok(SubscriptionCommandResult {
                        code: *code,
                        stdout: String::from_utf8_lossy(&state.stdout).into_owned(),
                        stderr: String::from_utf8_lossy(&state.stderr).into_owned(),
                    }),
                };
                settle.settle(result);
            }
            ChildEvent::Exit { .. } => {}
        }
    });
    if signal.aborted() {
        kill_process_tree_rt(rt, pid, true);
        state.borrow_mut().error =
            Some(JsError::error("The command was cancelled").named("AbortError"));
    }
    let result = wait.await;
    signal.off(listener);
    rt.clear_timeout(Some(timer));
    result
}

fn image(work: &Path) -> Result<Option<CodexImage>, JsError> {
    let metadata = |path: &Path| {
        std::fs::symlink_metadata(path)
            .ok()
            .filter(|m| m.file_type().is_file() && m.len() > 0)
    };
    let wanted = work.join("image.png");
    let chosen = if metadata(&wanted).is_some() {
        Some(wanted)
    } else {
        let mut candidates = Vec::new();
        for entry in std::fs::read_dir(work).map_err(|e| JsError::error(e.to_string()))? {
            let entry = entry.map_err(|e| JsError::error(e.to_string()))?;
            let path = entry.path();
            let ext = path
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            if !matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "webp") {
                continue;
            }
            if let Some(meta) = metadata(&path) {
                candidates.push((meta.modified().ok(), meta.len(), path));
            }
        }
        candidates.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
        candidates.into_iter().next().map(|c| c.2)
    };
    let Some(path) = chosen else {
        return Ok(None);
    };
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x00200000);
    }
    let file = options
        .open(path)
        .map_err(|e| JsError::error(e.to_string()))?;
    let meta = file.metadata().map_err(|e| JsError::error(e.to_string()))?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if meta.file_attributes() & 0x400 != 0 {
            return Err(JsError::error("Codex wrote a reparse-point image file."));
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.nlink() != 1 {
            return Err(JsError::error("Codex wrote a hard-linked image file."));
        }
    }
    if !meta.is_file() {
        return Err(JsError::error("Codex did not write a regular image file."));
    }
    if meta.len() > MAX_IMAGE {
        return Err(JsError::error(format!(
            "Codex wrote an image larger than {MAX_IMAGE} bytes."
        )));
    }
    let mut bytes = Vec::new();
    file.take(MAX_IMAGE + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| JsError::error(e.to_string()))?;
    if bytes.is_empty() {
        return Err(JsError::error("Codex wrote an empty image file."));
    }
    if bytes.len() as u64 > MAX_IMAGE {
        return Err(JsError::error(format!(
            "Codex wrote an image larger than {MAX_IMAGE} bytes."
        )));
    }
    let mime = match image::guess_format(&bytes) {
        Ok(image::ImageFormat::Png) => "image/png",
        Ok(image::ImageFormat::Jpeg) => "image/jpeg",
        Ok(image::ImageFormat::Gif) => "image/gif",
        Ok(image::ImageFormat::WebP) => "image/webp",
        _ => return Err(JsError::error("Codex wrote an invalid image file.")),
    };
    Ok(Some(CodexImage::Bytes {
        bytes,
        mime: mime.into(),
    }))
}

fn failure(result: &SubscriptionCommandResult) -> (JsError, bool) {
    let text = format!("{}\n{}", result.stderr, result.stdout);
    let lines: Vec<_> = jscompat::trim(&text).split('\n').collect();
    let tail = lines[lines.len().saturating_sub(8)..].join("\n");
    let detail = jscompat::slice_utf16(&tail, 0.0, Some(1200.0));
    let lower = detail.to_lowercase();
    // JavaScript's dot does not cross line terminators.
    let unsupported = lower.contains("not exposed")
        || lower.contains("image tool")
        || lower
            .split(['\n', '\r', '\u{2028}', '\u{2029}'])
            .any(|line| line.contains("model") && line.contains("not supported"));
    let hint = if unsupported || lower.contains("image_gen") || lower.contains("image generation") {
        "Codex did not offer its image generation tool in this session (the account or its default model does not support it). ChatGPT Plus/Team/Enterprise is needed for Codex image generation; connect an image provider key or use image search instead."
    } else {
        "Codex image generation failed."
    };
    let suffix = if detail.is_empty() {
        format!(
            " (exit code {})",
            result.code.map_or("null".into(), |c| c.to_string())
        )
    } else {
        format!(" Codex said: {detail}")
    };
    (JsError::error(format!("{hint}{suffix}")), unsupported)
}

pub(crate) fn generate(
    request: &CodexImageRequest,
    cancel: &CancelToken,
) -> Result<CodexImage, JsError> {
    if cancel.is_cancelled() {
        return Err(JsError::abort());
    }
    if USER_DATA.get().is_none() {
        return Err(JsError::error("AI settings are unavailable"));
    }
    let path = configured_path();
    let work = WorkDir::create(&std::env::temp_dir())?;
    let argv = args(request, &work.0);
    let cwd = work.0.to_string_lossy().into_owned();
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = mpsc::sync_channel(1);
    post_core(move |provided, _| {
        let Some(subscription) = provided.all::<SubscriptionShared>().into_iter().next() else {
            let _ = tx.send(Err(JsError::error("AI runtime is unavailable")));
            return;
        };
        let driver = subscription.driver();
        let rt = driver.rt();
        let signal = rt.controller();
        let providers = driver.providers();
        let (locate, shared) = {
            let codex = providers.codex.borrow();
            (codex.locate.clone(), codex.shared.clone())
        };
        RUNS.with(|runs| {
            runs.borrow_mut()
                .insert(id, (driver.clone(), signal.clone()));
        });
        driver.run(
            async move {
                let program = resolve_subscription_program(
                    &rt,
                    &locate,
                    "codex",
                    &SubscriptionLocateOptions {
                        configured_path: path,
                        ..Default::default()
                    },
                )
                .await?;
                run(&rt, &shared, &program, &argv, cwd, &signal).await
            },
            move |result| {
                RUNS.with(|runs| {
                    runs.borrow_mut().remove(&id);
                });
                let _ = tx.send(result);
            },
        );
    });
    let mut aborted = false;
    let result = loop {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(result) => break result?,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(JsError::error("AI runtime is unavailable"));
            }
            Err(mpsc::RecvTimeoutError::Timeout) if cancel.is_cancelled() && !aborted => {
                aborted = true;
                post_core(move |_, _| {
                    if let Some((driver, signal)) =
                        RUNS.with(|runs| runs.borrow().get(&id).cloned())
                    {
                        signal.abort(Some(JsError::abort()));
                        driver.run(async {}, |_| {});
                    }
                });
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    };
    if cancel.is_cancelled() {
        return Err(JsError::abort());
    }
    if let Some(image) = image(&work.0)? {
        return Ok(image);
    }
    let (error, unsupported) = failure(&result);
    if unsupported {
        let _ = crate::host::w::on_core(|provided| {
            if let Some(shared) = provided.all::<SubscriptionShared>().into_iter().next() {
                shared
                    .image
                    .borrow_mut()
                    .set_codex_image_generation_available(false);
            }
        });
    }
    Err(error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_arguments_preserve_requested_size_and_sandbox() {
        let request = CodexImageRequest {
            prompt: "draw a tree".into(),
            aspect_ratio: Some("16:9".into()),
            image_size: Some("2k".into()),
            transparent: false,
        };
        let argv = args(&request, Path::new("/work"));
        assert!(argv[1].contains("3. Size: 1536x1024\n4. Quality: high"));
        assert!(argv[1].contains("<<<PROMPT\ndraw a tree\nPROMPT>>>"));
        assert!(argv.contains(&"workspace-write".to_owned()));
        assert!(argv.contains(&"features.image_generation=true".to_owned()));
    }

    #[test]
    fn missing_tool_disables_route_but_generic_failure_does_not() {
        let result = SubscriptionCommandResult {
            code: Some(1.0),
            stdout: String::new(),
            stderr: "image tool not exposed".into(),
        };
        let (error, disabled) = failure(&result);
        assert!(disabled);
        assert!(error.message.contains("ChatGPT Plus/Team/Enterprise"));
        let (error, disabled) = failure(&SubscriptionCommandResult {
            stderr: "network failure".into(),
            ..result
        });
        assert!(!disabled);
        assert!(error.message.contains("Codex said: network failure"));
    }

    #[test]
    fn unsupported_model_orders_and_hint_only_cases_match_cli() {
        for text in ["model is not supported", "not supported by this model"] {
            let result = SubscriptionCommandResult {
                code: Some(1.0),
                stdout: String::new(),
                stderr: text.into(),
            };
            let (error, disabled) = failure(&result);
            assert!(disabled);
            assert!(error.message.contains("ChatGPT Plus/Team/Enterprise"));
        }
        let result = SubscriptionCommandResult {
            code: Some(1.0),
            stdout: String::new(),
            stderr: "image_gen unavailable".into(),
        };
        let (error, disabled) = failure(&result);
        assert!(!disabled);
        assert!(error.message.contains("ChatGPT Plus/Team/Enterprise"));
    }

    #[test]
    fn output_limit_keeps_crossing_chunk_and_failure_uses_its_tail() {
        let mut state = Run::default();
        collect(&mut state, &vec![b'x'; MAX_OUTPUT - 1], false);
        collect(&mut state, b"\nnot supported by this model", false);
        collect(&mut state, b"\nthis chunk is dropped", false);
        let result = SubscriptionCommandResult {
            code: Some(1.0),
            stdout: String::new(),
            stderr: String::from_utf8(state.stderr).unwrap(),
        };
        let (error, disabled) = failure(&result);
        // The final eight lines are then sliced from the front, just as in JS.
        assert!(!disabled);
        assert!(!error.message.contains("this chunk is dropped"));

        let mut state = Run::default();
        collect(&mut state, &vec![b'x'; MAX_OUTPUT - 1], false);
        collect(
            &mut state,
            b"\n1\n2\n3\n4\n5\n6\n7\nnot supported by this model",
            false,
        );
        let result = SubscriptionCommandResult {
            code: Some(1.0),
            stdout: String::new(),
            stderr: String::from_utf8(state.stderr).unwrap(),
        };
        assert!(failure(&result).1);
    }

    #[test]
    fn failure_slice_counts_utf16_and_handles_a_split_surrogate() {
        let result = SubscriptionCommandResult {
            code: Some(1.0),
            stdout: String::new(),
            stderr: format!("{}😀after", "a".repeat(1199)),
        };
        let (error, _) = failure(&result);
        let detail = error.message.split_once(" Codex said: ").unwrap().1;
        assert_eq!(detail, format!("{}�", "a".repeat(1199)));
        assert!(!detail.contains("after"));
    }
}
