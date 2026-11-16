//! Loopback HTTP and timer effects for the sans-I/O app MCP server.
//!
//! Events are delivered to the owner, which serializes them on the core
//! thread. In particular this layer never owns or duplicates MCP sessions.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::body::{Body, Frame, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use main_core::packages::mcp_sdk::server::{Command, ListenerId, ResponseId, SocketId, TimerId};
use main_core::packages::mcp_sdk::streamable_http_server::HttpRequest;
use tokio::sync::{mpsc, oneshot};
use tokio::task::AbortHandle;

const MAX_BODY: usize = 32 * 1024 * 1024;
static NEXT_SOCKET: AtomicU64 = AtomicU64::new(1);
static NEXT_RESPONSE: AtomicU64 = AtomicU64::new(1);
static NEXT_TIMER_GENERATION: AtomicU64 = AtomicU64::new(1);

pub(super) type Sink = Arc<dyn Fn(Event) + Send + Sync>;
type Head = (u16, Vec<(String, String)>);

pub(super) enum Event {
    Listening(ListenerId, u16),
    ListenError(ListenerId, Option<&'static str>, String),
    ListenerClosed(ListenerId),
    Connected(SocketId),
    SocketClosed(SocketId),
    Request(ResponseId, HttpRequest, Vec<u8>),
    ResponseClosed(ResponseId),
    Timer(TimerId, u64, bool),
}

struct Pending {
    head: Option<oneshot::Sender<Head>>,
    body: mpsc::UnboundedSender<Result<Frame<Bytes>, std::io::Error>>,
    socket: SocketId,
}

#[derive(Default)]
struct State {
    listeners: HashMap<ListenerId, oneshot::Sender<()>>,
    sockets: HashMap<SocketId, AbortHandle>,
    responses: HashMap<ResponseId, Pending>,
    timers: HashMap<TimerId, (u64, oneshot::Sender<()>)>,
    closing: bool,
    tasks: usize,
    task_failure: Option<String>,
}

struct Inner {
    state: Mutex<State>,
    stopped: Condvar,
    sink: Sink,
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
    fn emit(&self, event: Event) {
        (self.sink)(event);
    }

    fn track(self: &Arc<Self>) -> Option<TaskGuard> {
        let mut state = self.lock();
        if state.closing {
            return None;
        }
        state.tasks += 1;
        Some(TaskGuard(self.clone()))
    }
}

struct TaskGuard(Arc<Inner>);
impl Drop for TaskGuard {
    fn drop(&mut self) {
        let mut state = self.0.lock();
        if std::thread::panicking() {
            state
                .task_failure
                .get_or_insert_with(|| "MCP transport task panicked".into());
        }
        state.tasks -= 1;
        if state.tasks == 0 {
            self.0.stopped.notify_all();
        }
    }
}

/// Must live for as long as the service; dropping it closes every listener,
/// connection, response and timer.
pub(super) struct Transport(Arc<Inner>);

impl Transport {
    pub(super) fn new(sink: Sink) -> Self {
        Self(Arc::new(Inner {
            state: Mutex::new(State::default()),
            stopped: Condvar::new(),
            sink,
        }))
    }

    pub(super) fn shutdown_and_wait(&self) -> Result<(), String> {
        let mut state = self.0.lock();
        state.closing = true;
        state.listeners.clear();
        for (_, task) in state.sockets.drain() {
            task.abort();
        }
        state.timers.clear();
        state.responses.clear();
        while state.tasks != 0 {
            state = self
                .0
                .stopped
                .wait(state)
                .unwrap_or_else(|error| error.into_inner());
        }
        match &state.task_failure {
            Some(error) => Err(error.clone()),
            None => Ok(()),
        }
    }

    /// Carries out transport commands. The remaining commands belong to the
    /// runtime owner, notably `ToolCall` and operation settlements.
    pub(super) fn execute(&self, command: Command) -> Result<(), Command> {
        match command {
            Command::Listen { listener, port } => self.listen(listener, port),
            Command::CloseListener { listener } => {
                if let Some(stop) = self.0.lock().listeners.remove(&listener) {
                    let _ = stop.send(());
                } else {
                    self.0.emit(Event::ListenerClosed(listener));
                }
            }
            Command::DestroySocket { socket } => {
                if let Some(task) = self.0.lock().sockets.remove(&socket) {
                    task.abort();
                }
            }
            Command::Respond {
                res,
                status,
                headers,
            } => {
                if let Some(pending) = self.0.lock().responses.get_mut(&res)
                    && let Some(head) = pending.head.take()
                {
                    let _ = head.send((status, headers));
                }
            }
            Command::Write { res, chunk } => {
                if let Some(pending) = self.0.lock().responses.get(&res) {
                    let _ = pending.body.send(Ok(Frame::data(Bytes::from(chunk))));
                }
            }
            Command::End { res, chunk } => {
                if let Some(pending) = self.0.lock().responses.remove(&res) {
                    if let Some(head) = pending.head {
                        let _ = head.send((200, Vec::new()));
                    }
                    if let Some(chunk) = chunk {
                        let _ = pending.body.send(Ok(Frame::data(Bytes::from(chunk))));
                    }
                }
            }
            Command::Destroy { res } => {
                let socket = self.0.lock().responses.remove(&res).map(|p| p.socket);
                if let Some(socket) = socket
                    && let Some(task) = self.0.lock().sockets.remove(&socket)
                {
                    task.abort();
                }
            }
            Command::SetInterval { timer, ms } => self.timer(timer, ms, true),
            Command::SetTimeout { timer, ms } => self.timer(timer, ms, false),
            Command::ClearTimer { timer } => {
                self.0.lock().timers.remove(&timer);
            }
            other => return Err(other),
        }
        Ok(())
    }

    /// A timer ID is local to one MCP service and can be reused after a
    /// restart. Drop queued ticks from an earlier service or a cleared timer.
    pub(super) fn accept_timer(&self, id: TimerId, generation: u64, repeating: bool) -> bool {
        let mut state = self.0.lock();
        let current = state
            .timers
            .get(&id)
            .is_some_and(|(live, _)| *live == generation);
        if current && !repeating {
            state.timers.remove(&id);
        }
        current
    }

    fn listen(&self, id: ListenerId, port: f64) {
        let Some(tracked) = self.0.track() else {
            return;
        };
        let (stop, mut stopped) = oneshot::channel();
        self.0.lock().listeners.insert(id, stop);
        let inner = self.0.clone();
        tauri::async_runtime::spawn(async move {
            let _tracked = tracked;
            let listener =
                match tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port as u16))
                    .await
                {
                    Ok(listener) => listener,
                    Err(error) => {
                        inner.lock().listeners.remove(&id);
                        let code = match error.kind() {
                            std::io::ErrorKind::AddrInUse => Some("EADDRINUSE"),
                            std::io::ErrorKind::PermissionDenied => Some("EACCES"),
                            _ => None,
                        };
                        inner.emit(Event::ListenError(id, code, format!("Error: {error}")));
                        return;
                    }
                };
            let actual = listener
                .local_addr()
                .map(|a| a.port())
                .unwrap_or(port as u16);
            inner.emit(Event::Listening(id, actual));
            loop {
                let accepted = tokio::select! {
                    _ = &mut stopped => break,
                    result = listener.accept() => result,
                };
                let Ok((stream, _)) = accepted else { continue };
                let Some(tracked) = inner.track() else { break };
                let socket = SocketId(NEXT_SOCKET.fetch_add(1, Ordering::Relaxed));
                let owner = inner.clone();
                let guard = SocketTaskGuard {
                    _closed: SocketGuard {
                        inner: owner.clone(),
                        socket,
                    },
                    _tracked: tracked,
                };
                let (ready, start) = oneshot::channel::<()>();
                let task = tokio::spawn(async move {
                    let _guard = guard;
                    if start.await.is_err() {
                        return;
                    }
                    let service = service_fn(move |request| handle(owner.clone(), socket, request));
                    let _ = http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
                let mut state = inner.lock();
                if state.closing {
                    drop(state);
                    task.abort();
                    break;
                }
                state.sockets.insert(socket, task.abort_handle());
                drop(state);
                inner.emit(Event::Connected(socket));
                let _ = ready.send(());
            }
            drop(listener);
            inner.lock().listeners.remove(&id);
            inner.emit(Event::ListenerClosed(id));
        });
    }

    fn timer(&self, id: TimerId, ms: f64, repeating: bool) {
        let Some(tracked) = self.0.track() else {
            return;
        };
        let generation = NEXT_TIMER_GENERATION.fetch_add(1, Ordering::Relaxed);
        let (cancel, mut cancelled) = oneshot::channel();
        self.0.lock().timers.insert(id, (generation, cancel));
        let inner = self.0.clone();
        let delay = Duration::from_millis(if ms.is_finite() {
            ms.max(1.0).min(u32::MAX as f64) as u64
        } else {
            1
        });
        tauri::async_runtime::spawn(async move {
            let _tracked = tracked;
            loop {
                tokio::select! {
                    _ = &mut cancelled => break,
                    _ = tokio::time::sleep(delay) => inner.emit(Event::Timer(id, generation, repeating)),
                }
                if !repeating {
                    break;
                }
            }
        });
    }
}

impl Drop for Transport {
    fn drop(&mut self) {
        let mut state = self.0.lock();
        state.listeners.clear();
        for (_, task) in state.sockets.drain() {
            task.abort();
        }
        state.timers.clear();
        state.responses.clear();
    }
}

struct SocketGuard {
    inner: Arc<Inner>,
    socket: SocketId,
}
impl Drop for SocketGuard {
    fn drop(&mut self) {
        self.inner.lock().sockets.remove(&self.socket);
        self.inner.emit(Event::SocketClosed(self.socket));
    }
}

struct SocketTaskGuard {
    _closed: SocketGuard,
    _tracked: TaskGuard,
}

struct ResponseGuard {
    inner: Arc<Inner>,
    res: ResponseId,
}
impl Drop for ResponseGuard {
    fn drop(&mut self) {
        self.inner.lock().responses.remove(&self.res);
        self.inner.emit(Event::ResponseClosed(self.res));
    }
}

struct StreamBody {
    receiver: mpsc::UnboundedReceiver<Result<Frame<Bytes>, std::io::Error>>,
    _guard: ResponseGuard,
}
impl Body for StreamBody {
    type Data = Bytes;
    type Error = std::io::Error;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        self.receiver.poll_recv(cx)
    }
}

async fn handle(
    inner: Arc<Inner>,
    socket: SocketId,
    request: Request<Incoming>,
) -> Result<Response<StreamBody>, hyper::Error> {
    let res = ResponseId(NEXT_RESPONSE.fetch_add(1, Ordering::Relaxed));
    let mut headers: Vec<(String, String)> = Vec::new();
    for (name, value) in request.headers() {
        let value = String::from_utf8_lossy(value.as_bytes());
        if let Some((_, old)) = headers.iter_mut().find(|(key, _)| key == name.as_str()) {
            old.push_str(", ");
            old.push_str(&value);
        } else {
            headers.push((name.as_str().to_owned(), value.into_owned()));
        }
    }
    let head = HttpRequest {
        method: request.method().as_str().to_owned(),
        url: request
            .uri()
            .path_and_query()
            .map_or("/", |p| p.as_str())
            .to_owned(),
        headers,
    };
    let mut body = request.into_body();
    let mut bytes = Vec::new();
    while let Some(frame) = body.frame().await {
        if let Ok(chunk) = frame?.into_data() {
            let left = (MAX_BODY + 1).saturating_sub(bytes.len());
            bytes.extend_from_slice(&chunk[..chunk.len().min(left)]);
            if bytes.len() > MAX_BODY {
                break;
            }
        }
    }
    let (head_tx, head_rx) = oneshot::channel();
    let (body_tx, body_rx) = mpsc::unbounded_channel();
    inner.lock().responses.insert(
        res,
        Pending {
            head: Some(head_tx),
            body: body_tx,
            socket,
        },
    );
    let guard = ResponseGuard {
        inner: inner.clone(),
        res,
    };
    inner.emit(Event::Request(res, head, bytes));
    let (status, headers) = head_rx.await.unwrap_or((500, Vec::new()));
    let stream = StreamBody {
        receiver: body_rx,
        _guard: guard,
    };
    let mut response = Response::new(stream);
    *response.status_mut() =
        hyper::StatusCode::from_u16(status).unwrap_or(hyper::StatusCode::INTERNAL_SERVER_ERROR);
    for (name, value) in headers {
        if let (Ok(name), Ok(value)) = (
            hyper::header::HeaderName::from_bytes(name.as_bytes()),
            hyper::header::HeaderValue::from_str(&value),
        ) {
            response.headers_mut().append(name, value);
        }
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shutdown_waits_until_an_owned_task_releases() {
        let transport = Arc::new(Transport::new(Arc::new(|_| {})));
        let task = transport.0.track().unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let owned = transport.clone();
        let worker = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            owned.shutdown_and_wait().unwrap();
            done_tx.send(()).unwrap();
        });
        started_rx.recv().unwrap();
        assert!(done_rx.try_recv().is_err());
        drop(task);
        done_rx.recv().unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn failed_owned_task_is_reported_by_shutdown() {
        let transport = Transport::new(Arc::new(|_| {}));
        let task = transport.0.track().unwrap();
        let worker = std::thread::spawn(move || {
            let _task = task;
            panic!("synthetic transport failure");
        });
        assert!(worker.join().is_err());
        assert!(transport.shutdown_and_wait().is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn shutdown_releases_the_listening_port_before_returning() {
        let (tx, mut events) = mpsc::unbounded_channel();
        let transport = Transport::new(Arc::new(move |event| {
            let _ = tx.send(event);
        }));
        assert!(
            transport
                .execute(Command::Listen {
                    listener: ListenerId(9),
                    port: 0.0
                })
                .is_ok()
        );
        let port = match tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .unwrap()
            .unwrap()
        {
            Event::Listening(ListenerId(9), port) => port,
            _ => panic!("listener did not start"),
        };
        tokio::task::spawn_blocking(move || transport.shutdown_and_wait())
            .await
            .unwrap()
            .unwrap();
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
        drop(listener);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn loopback_request_streams_response_and_closes_listener() {
        let (tx, mut events) = mpsc::unbounded_channel();
        let transport = Transport::new(Arc::new(move |event| {
            let _ = tx.send(event);
        }));
        assert!(
            transport
                .execute(Command::Listen {
                    listener: ListenerId(1),
                    port: 0.0
                })
                .is_ok()
        );
        let port = match tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .unwrap()
            .unwrap()
        {
            Event::Listening(ListenerId(1), port) => port,
            _ => panic!("listener did not start"),
        };
        let client = tokio::spawn(async move {
            reqwest::Client::new()
                .post(format!("http://127.0.0.1:{port}/mcp"))
                .header("x-test", "value")
                .body("payload")
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap()
        });
        let response = loop {
            match tokio::time::timeout(Duration::from_secs(5), events.recv())
                .await
                .unwrap()
                .unwrap()
            {
                Event::Request(id, head, body) => {
                    assert_eq!(head.method, "POST");
                    assert_eq!(head.url, "/mcp");
                    assert_eq!(head.header("x-test"), Some("value"));
                    assert_eq!(body, b"payload");
                    break id;
                }
                Event::Connected(_) => {}
                _ => panic!("unexpected transport event"),
            }
        };
        assert!(
            transport
                .execute(Command::Respond {
                    res: response,
                    status: 200,
                    headers: vec![("content-type".into(), "text/plain".into())]
                })
                .is_ok()
        );
        assert!(
            transport
                .execute(Command::Write {
                    res: response,
                    chunk: "one".into()
                })
                .is_ok()
        );
        assert!(
            transport
                .execute(Command::End {
                    res: response,
                    chunk: Some(" two".into())
                })
                .is_ok()
        );
        assert_eq!(client.await.unwrap(), "one two");
        assert!(
            transport
                .execute(Command::CloseListener {
                    listener: ListenerId(1)
                })
                .is_ok()
        );
        loop {
            if matches!(
                tokio::time::timeout(Duration::from_secs(5), events.recv())
                    .await
                    .unwrap()
                    .unwrap(),
                Event::ListenerClosed(ListenerId(1))
            ) {
                break;
            }
        }
    }

    #[test]
    fn stale_tick_cannot_fire_a_reused_timer() {
        let transport = Transport::new(Arc::new(|_| {}));
        let (cancel, _) = oneshot::channel();
        transport.0.lock().timers.insert(TimerId(7), (2, cancel));
        assert!(!transport.accept_timer(TimerId(7), 1, false));
        assert!(transport.accept_timer(TimerId(7), 2, false));
        assert!(!transport.accept_timer(TimerId(7), 2, false));
    }
}
