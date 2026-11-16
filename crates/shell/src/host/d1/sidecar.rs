//! Process transport for the workbook sidecar state machine.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use main_core::sheets::xlsx_sidecar_client::{
    ListenerId, Settled, SidecarHost, Submitted, XlsxSidecarClient,
};
use serde_json::{Map, Value};

type Answer = mpsc::Sender<Settled>;
type ExitListener = Arc<dyn Fn() + Send + Sync>;

pub(super) enum Request {
    Open {
        path: String,
        locale: String,
        short_date: String,
        timeout_ms: Option<f64>,
    },
    Close(String),
    Read {
        command: String,
        input: Map<String, Value>,
    },
    Archive {
        command: String,
        input: Map<String, Value>,
    },
    ConvertWorkbook(Map<String, Value>),
    ConvertToOds(Map<String, Value>),
    ArchiveManifest(String),
}

enum Event {
    Request(Request, Answer),
    Subscribe(ExitListener),
    Start,
    Stdout(u64, String),
    Stderr(u64, String),
    WriteFailed(u64, Option<String>, String),
    StdoutClosed(u64),
    StderrClosed(u64),
    Stop(Option<mpsc::Sender<()>>),
}

struct Process {
    id: u64,
    child: Child,
    stdin: mpsc::Sender<(String, Option<String>)>,
    stdout_closed: bool,
    stderr_closed: bool,
}

struct Transport {
    events: mpsc::Sender<Event>,
    process: Option<Process>,
    timers: HashMap<u64, Instant>,
    next_timer: u64,
    pending: HashMap<String, Answer>,
    listeners: HashMap<ListenerId, ExitListener>,
}

impl SidecarHost for Transport {
    fn spawn(&mut self, binary_path: &str, id: u64) -> Result<(), String> {
        let mut command = Command::new(binary_path);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000);
        }
        let mut child = command.spawn().map_err(|e| e.to_string())?;
        let mut stdin = child.stdin.take().ok_or("sidecar stdin unavailable")?;
        let stdout = child.stdout.take().ok_or("sidecar stdout unavailable")?;
        let stderr = child.stderr.take().ok_or("sidecar stderr unavailable")?;
        let (writer, writes) = mpsc::channel::<(String, Option<String>)>();
        let events = self.events.clone();
        thread::spawn(move || {
            while let Ok((data, request_id)) = writes.recv() {
                if let Err(error) = stdin.write_all(data.as_bytes()) {
                    let _ = events.send(Event::WriteFailed(id, request_id, error.to_string()));
                    break;
                }
            }
        });
        let events = self.events.clone();
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = Vec::new();
            loop {
                line.clear();
                match reader.read_until(b'\n', &mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if line.last() == Some(&b'\n') {
                            line.pop();
                        }
                        if line.last() == Some(&b'\r') {
                            line.pop();
                        }
                        if events
                            .send(Event::Stdout(
                                id,
                                String::from_utf8_lossy(&line).into_owned(),
                            ))
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
            let _ = events.send(Event::StdoutClosed(id));
        });
        let events = self.events.clone();
        thread::spawn(move || {
            let mut reader = BufReader::new(stderr);
            let mut buffer = [0u8; 4096];
            while let Ok(n) = reader.read(&mut buffer) {
                if n == 0 {
                    break;
                }
                if events
                    .send(Event::Stderr(
                        id,
                        String::from_utf8_lossy(&buffer[..n]).into_owned(),
                    ))
                    .is_err()
                {
                    break;
                }
            }
            let _ = events.send(Event::StderrClosed(id));
        });
        self.process = Some(Process {
            id,
            child,
            stdin: writer,
            stdout_closed: false,
            stderr_closed: false,
        });
        Ok(())
    }
    fn pid(&mut self, process: u64) -> Option<f64> {
        self.process
            .as_ref()
            .filter(|p| p.id == process)
            .map(|p| f64::from(p.child.id()))
    }

    fn write_stdin(&mut self, process: u64, data: &str, request_id: Option<&str>) {
        let result = self
            .process
            .as_ref()
            .filter(|p| p.id == process)
            .ok_or_else(|| "sidecar stdin unavailable".to_owned())
            .and_then(|p| {
                p.stdin
                    .send((data.to_owned(), request_id.map(str::to_owned)))
                    .map_err(|e| e.to_string())
            });
        if let Err(error) = result {
            let _ = self.events.send(Event::WriteFailed(
                process,
                request_id.map(str::to_owned),
                error,
            ));
        }
    }

    fn kill(&mut self, process: u64) -> bool {
        if let Some(mut p) = self.process.take() {
            if p.id != process {
                self.process = Some(p);
                return false;
            }
            let killed = p.child.kill().is_ok();
            let _ = p.child.wait();
            killed
        } else {
            false
        }
    }
    fn close_lines(&mut self, _: u64) {}
    fn random_uuid(&mut self) -> String {
        jscompat::random_uuid()
    }
    fn set_timeout(&mut self, ms: f64) -> u64 {
        let id = self.next_timer;
        self.next_timer += 1;
        let duration = if ms.is_finite() && (1.0..=2_147_483_647.0).contains(&ms) {
            Duration::from_millis(ms as u64)
        } else {
            Duration::from_millis(1)
        };
        self.timers.insert(id, Instant::now() + duration);
        id
    }
    fn clear_timeout(&mut self, timer: u64) {
        self.timers.remove(&timer);
    }
    fn settle(&mut self, request_id: &str, outcome: Settled) {
        if let Some(answer) = self.pending.remove(request_id) {
            let _ = answer.send(outcome);
        }
    }
    fn warn_prewarm_failed(&mut self, message: &str) {
        eprintln!("XLSX sidecar prewarm failed: {message}");
    }
    fn exit_notified(&mut self, listener: ListenerId) {
        if let Some(callback) = self.listeners.get(&listener).cloned() {
            thread::spawn(move || callback());
        }
    }
}

pub(super) fn binary_path(app: &tauri::AppHandle) -> Result<std::path::PathBuf, String> {
    if let Some(path) = std::env::var_os("XLSX_SIDECAR_PATH")
        && !path.is_empty()
    {
        return Ok(path.into());
    }
    let executable = if cfg!(windows) {
        "xlsx-sidecar.exe"
    } else {
        "xlsx-sidecar"
    };
    if cfg!(debug_assertions) {
        Ok(std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../apps/sheets/native/xlsx-engine/target/release")
            .join(executable))
    } else {
        Ok(crate::paths::resources_path(app)
            .ok_or("application resources are unavailable")?
            .join("native")
            .join(executable))
    }
}

pub(super) struct Client {
    events: mpsc::Sender<Event>,
}

impl Client {
    pub fn new(binary_path: String) -> Self {
        let (events, receiver) = mpsc::channel();
        let host = Transport {
            events: events.clone(),
            process: None,
            timers: HashMap::new(),
            next_timer: 0,
            pending: HashMap::new(),
            listeners: HashMap::new(),
        };
        thread::spawn(move || run(XlsxSidecarClient::new(host, &binary_path), receiver));
        Self { events }
    }

    pub fn start(&self) -> Result<(), String> {
        self.events
            .send(Event::Start)
            .map_err(|_| "XLSX sidecar transport stopped.".to_owned())
    }

    pub fn shutdown(&self) {
        let (send, receive) = mpsc::channel();
        if self.events.send(Event::Stop(Some(send))).is_ok() {
            let _ = receive.recv();
        }
    }

    pub fn request(&self, request: Request) -> Settled {
        let (send, receive) = mpsc::channel();
        self.events
            .send(Event::Request(request, send))
            .map_err(|_| "XLSX sidecar transport stopped.".to_owned())?;
        receive
            .recv()
            .unwrap_or_else(|_| Err("XLSX sidecar transport stopped.".to_owned()))
    }

    pub fn request_native(
        &self,
        request: Request,
        document: &shell_contract::ipc::NativeInvocationDocument,
        cancel: &shell_contract::runtime::CancelToken,
        pending: &shell_contract::runtime::CancelToken,
    ) -> Settled {
        let (send, receive) = mpsc::channel();
        super::workbook::native_snapshot::admit(document, cancel, pending, || {
            self.events.send(Event::Request(request, send))
        })
        .map_err(|error| error.message)?
        .map_err(|_| "XLSX sidecar transport stopped.".to_owned())?;
        receive
            .recv()
            .unwrap_or_else(|_| Err("XLSX sidecar transport stopped.".to_owned()))
    }

    pub fn request_saved(
        &self,
        request: Request,
        document: &shell_contract::ipc::NativeSavedDocument,
        actual: &shell_contract::ipc::NativeRequestOwner,
        actual_cancel: &shell_contract::runtime::CancelToken,
    ) -> Settled {
        let (send, receive) = mpsc::channel();
        super::workbook::native_snapshot::admit_saved(document, actual, actual_cancel, |_| {
            self.events.send(Event::Request(request, send))
        })
        .map_err(|error| error.message)?
        .map_err(|_| "XLSX sidecar transport stopped.".to_owned())?;
        receive
            .recv()
            .unwrap_or_else(|_| Err("XLSX sidecar transport stopped.".to_owned()))
    }

    pub fn on_process_exit(&self, callback: ExitListener) -> Result<(), String> {
        self.events
            .send(Event::Subscribe(callback))
            .map_err(|_| "XLSX sidecar transport stopped.".to_owned())
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.events.send(Event::Stop(None));
    }
}

#[cfg(unix)]
fn exit_signal(status: &std::process::ExitStatus) -> Option<&'static str> {
    use std::os::unix::process::ExitStatusExt;
    match status.signal()? {
        1 => Some("SIGHUP"),
        2 => Some("SIGINT"),
        3 => Some("SIGQUIT"),
        4 => Some("SIGILL"),
        5 => Some("SIGTRAP"),
        6 => Some("SIGABRT"),
        8 => Some("SIGFPE"),
        9 => Some("SIGKILL"),
        11 => Some("SIGSEGV"),
        13 => Some("SIGPIPE"),
        14 => Some("SIGALRM"),
        15 => Some("SIGTERM"),
        _ => None,
    }
}

#[cfg(not(unix))]
fn exit_signal(_: &std::process::ExitStatus) -> Option<&'static str> {
    None
}

fn run(mut client: XlsxSidecarClient<Transport>, events: mpsc::Receiver<Event>) {
    loop {
        let now = Instant::now();
        let due: Vec<_> = client
            .host()
            .timers
            .iter()
            .filter_map(|(&id, &at)| (at <= now).then_some(id))
            .collect();
        for timer in due {
            client.host_mut().timers.remove(&timer);
            client.on_timeout(timer);
        }
        let exited = client
            .host_mut()
            .process
            .as_mut()
            .filter(|p| p.stdout_closed && p.stderr_closed)
            .and_then(|p| match p.child.try_wait() {
                Ok(Some(status)) => Some((p.id, Ok(status))),
                Ok(None) => None,
                Err(error) => Some((p.id, Err(error.to_string()))),
            });
        if let Some((id, status)) = exited {
            if let Some(mut process) = client.host_mut().process.take()
                && status.is_err()
            {
                let _ = process.child.kill();
                let _ = process.child.wait();
            }
            match status {
                Ok(status) => {
                    client.on_exit(id, status.code().map(f64::from), exit_signal(&status))
                }
                Err(error) => client.on_error(id, &error),
            }
        }
        let delay = client
            .host()
            .timers
            .values()
            .min()
            .map(|at| at.saturating_duration_since(Instant::now()))
            .unwrap_or(Duration::from_millis(50))
            .min(Duration::from_millis(50));
        match events.recv_timeout(delay) {
            Ok(Event::Request(request, answer)) => {
                let submitted = match request {
                    Request::Open {
                        path,
                        locale,
                        short_date,
                        timeout_ms,
                    } => client.open(&path, Some(&locale), Some(&short_date), timeout_ms),
                    Request::Close(id) => client.close(&id),
                    Request::Read { command, input } => client.read(&command, &input),
                    Request::Archive { command, input } => client.archive_command(&command, &input),
                    Request::ConvertWorkbook(input) => client.convert_workbook(&input),
                    Request::ConvertToOds(input) => client.convert_to_ods(&input),
                    Request::ArchiveManifest(path) => client.archive_manifest(&path),
                };
                match submitted {
                    Submitted::Pending(id) => {
                        client.host_mut().pending.insert(id, answer);
                    }
                    Submitted::Rejected(error) => {
                        let _ = answer.send(Err(error));
                    }
                }
            }
            Ok(Event::Subscribe(callback)) => {
                let id = client.on_process_exit();
                client.host_mut().listeners.insert(id, callback);
            }
            Ok(Event::Start) => client.start(),
            Ok(Event::Stdout(id, line))
                if client.host().process.as_ref().is_some_and(|p| p.id == id) =>
            {
                if let Err(error) = client.on_line(&line) {
                    if let Some(mut process) = client.host_mut().process.take() {
                        let _ = process.child.kill();
                        let _ = process.child.wait();
                    }
                    client.on_error(id, &error);
                }
            }
            Ok(Event::Stderr(id, text))
                if client.host().process.as_ref().is_some_and(|p| p.id == id) =>
            {
                client.on_stderr(&text)
            }
            Ok(Event::StdoutClosed(id)) => {
                if let Some(process) = client.host_mut().process.as_mut().filter(|p| p.id == id) {
                    process.stdout_closed = true;
                }
            }
            Ok(Event::StderrClosed(id)) => {
                if let Some(process) = client.host_mut().process.as_mut().filter(|p| p.id == id) {
                    process.stderr_closed = true;
                }
            }
            Ok(Event::WriteFailed(id, request, error))
                if client.host().process.as_ref().is_some_and(|p| p.id == id) =>
            {
                if let Some(request) = request {
                    client.on_write_done(&request, Some(&error));
                }
                if let Some(mut process) = client.host_mut().process.take() {
                    let _ = process.child.kill();
                    let _ = process.child.wait();
                }
                client.on_stdin_error(id);
            }
            Ok(Event::Stop(done)) => {
                client.stop();
                if let Some(done) = done {
                    let _ = done.send(());
                }
                break;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                client.stop();
                break;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    #[test]
    fn real_sidecar_opens_reads_and_closes_a_workbook() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let binary = std::env::var_os("XLSX_SIDECAR_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                root.join("apps/sheets/native/xlsx-engine/target/release")
                    .join(if cfg!(windows) {
                        "xlsx-sidecar.exe"
                    } else {
                        "xlsx-sidecar"
                    })
            });
        let workbook = root.join("apps/shell/build/shell-new/blank.xlsx");
        let client = Client::new(binary.to_string_lossy().into_owned());
        let opened = client
            .request(Request::Open {
                path: workbook.to_string_lossy().into_owned(),
                locale: "en".into(),
                short_date: "m/d/yy".into(),
                timeout_ms: None,
            })
            .expect("open workbook")
            .expect("open result");
        let session = opened["sessionId"].as_str().expect("session id");
        let sheet = opened["sheets"][0]["id"].as_str().expect("sheet id");
        let input = json!({
            "sessionId": session, "sheetId": sheet,
            "range": { "startRow": 0, "endRow": 0, "startColumn": 0, "endColumn": 0 }
        })
        .as_object()
        .expect("request object")
        .clone();
        let cells = client
            .request(Request::Read {
                command: "read_range".into(),
                input: input.clone(),
            })
            .expect("read blank sheet")
            .expect("read result");
        assert!(cells["cells"].as_array().expect("cells").is_empty());
        client
            .request(Request::Close(session.to_owned()))
            .expect("close workbook");
        assert!(
            client
                .request(Request::Read {
                    command: "read_range".into(),
                    input
                })
                .is_err()
        );
        client.shutdown();
    }
}
