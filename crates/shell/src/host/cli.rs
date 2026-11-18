//! The bundled Node CLI used by headless document tools.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use jscompat::JsonValue;
use main_core::shell::mcp::cli_runner::{CliRunOptions, CliRunOutcome, CliRunner, MAX_CLI_OUTPUT_BYTES, clamp_timeout_ms, parse_json};

pub(crate) struct BundledCli(PathBuf);

impl BundledCli {
    pub(crate) fn new(resources: PathBuf) -> Self {
        Self(resources)
    }
}

impl CliRunner for BundledCli {
    fn run(&self, args: &[String], options: &CliRunOptions) -> CliRunOutcome {
        let cli = self.0.join("cli");
        let node = cli.join("node").join(if cfg!(windows) { "node.exe" } else { "node" });
        let mut command = Command::new(node);
        command.arg(cli.join("seuoffice.cjs")).args(args).arg("--json")
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        command.env("ELECTRON_RUN_AS_NODE", "1");
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        let sidecar = self.0.join("native").join(if cfg!(windows) { "xlsx-sidecar.exe" } else { "xlsx-sidecar" });
        if sidecar.is_file() {
            command.env("XLSX_SIDECAR_PATH", sidecar);
        }
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => return CliRunOutcome {
                ok: false, code: -1.0, json: None, stdout: String::new(), stderr: error.to_string(),
            },
        };
        if let Some(mut stdin) = child.stdin.take() {
            let input = options.input.clone();
            std::thread::spawn(move || {
                if let Some(input) = input {
                    let _ = stdin.write_all(input.as_bytes());
                }
            });
        }
        let (tx, rx) = mpsc::sync_channel(16);
        for (is_stdout, stream) in [
            (true, child.stdout.take().map(|s| Box::new(s) as Box<dyn Read + Send>)),
            (false, child.stderr.take().map(|s| Box::new(s) as Box<dyn Read + Send>)),
        ] {
            let tx = tx.clone();
            std::thread::spawn(move || {
                if let Some(mut stream) = stream {
                    let mut buffer = [0; 8192];
                    loop {
                        match stream.read(&mut buffer) {
                            Ok(0) => break,
                            Ok(n) => if tx.send((is_stdout, Ok(Some(buffer[..n].to_vec())))).is_err() { return; },
                            Err(error) => {
                                let _ = tx.send((is_stdout, Err(error.to_string())));
                                return;
                            }
                        }
                    }
                }
                let _ = tx.send((is_stdout, Ok(None)));
            });
        }
        drop(tx);
        let deadline = Instant::now() + Duration::from_millis(clamp_timeout_ms(options.timeout_ms) as u64);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut closed = 0;
        let mut status = None;
        let mut failure = None;
        while closed < 2 || status.is_none() {
            if Instant::now() >= deadline {
                failure = Some("seuoffice timed out".to_string());
                break;
            }
            match rx.recv_timeout(Duration::from_millis(25)) {
                Ok((_, Ok(None))) => closed += 1,
                Ok((is_stdout, Ok(Some(chunk)))) => {
                    if stdout.len() + stderr.len() + chunk.len() > MAX_CLI_OUTPUT_BYTES {
                        failure = Some(format!("output truncated past {MAX_CLI_OUTPUT_BYTES} bytes"));
                        break;
                    }
                    if is_stdout { stdout.extend(chunk); } else { stderr.extend(chunk); }
                }
                Ok((_, Err(error))) => {
                    failure = Some(error);
                    break;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    closed = 2;
                    std::thread::sleep(Duration::from_millis(25));
                }
            }
            if status.is_none() {
                match child.try_wait() {
                    Ok(result) => status = result,
                    Err(error) => {
                        failure = Some(error.to_string());
                        break;
                    }
                }
            }
        }
        if failure.is_some() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let stdout = String::from_utf8_lossy(&stdout).into_owned();
        let mut stderr = String::from_utf8_lossy(&stderr).into_owned();
        if let Some(failure) = failure {
            stderr.push('\n');
            stderr.push_str(&failure);
            return CliRunOutcome { ok: false, code: -1.0, json: None, stdout, stderr };
        }
        let code = status.and_then(|status| status.code()).unwrap_or(-1) as f64;
        let json = parse_json(&stdout);
        let ok = code == 0.0 && matches!(json.as_ref().and_then(|value| value.get("status")).as_deref(), Some(JsonValue::String(value)) if value == "ok");
        CliRunOutcome { ok, code, json, stdout, stderr }
    }
}
