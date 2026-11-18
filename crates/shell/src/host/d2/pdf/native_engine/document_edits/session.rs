//! Request-bound transport for a single isolated pdf-lib document.
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, mpsc};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use shell_contract::runtime::{CancelSubscription, CancelToken};

static NEXT: AtomicU64 = AtomicU64::new(1);
static SESSIONS: LazyLock<Mutex<HashMap<u64,Arc<Transport>>>> = LazyLock::new(||Mutex::new(HashMap::new()));
struct Transport { commands: Mutex<mpsc::Receiver<Value>>, results: mpsc::Sender<Result<Value,String>>, request_id: Mutex<Option<u64>>, cancel: CancelToken }
pub(in super::super) struct Session {id:u64, commands:mpsc::Sender<Value>, results:mpsc::Receiver<Result<Value,String>>, cancel:CancelToken, _subscription:CancelSubscription}
impl Session {
    pub(in super::super) fn open(bytes: &[u8], cancel: CancelToken) -> Result<Self,String> {
        let internal=CancelToken::default(); let forwarded=internal.clone(); let subscription=cancel.on_cancel(move ||forwarded.cancel()); let cancel=internal;
        let id=NEXT.fetch_update(Ordering::Relaxed,Ordering::Relaxed,|n|n.checked_add(1)).map_err(|_|"PDF document session identities exhausted")?;
        if id>9_007_199_254_740_991 {return Err("PDF document session identities exhausted".into())}
        let (commands,receiver)=mpsc::channel(); let (sender,results)=mpsc::channel();
        let transport=Arc::new(Transport{commands:Mutex::new(receiver),results:sender.clone(),request_id:Mutex::new(None),cancel:cancel.clone()});
        SESSIONS.lock().map_err(|_|"PDF document session registry poisoned")?.insert(id,transport);
        let worker_cancel=cancel.clone(); std::thread::spawn(move || {let result=crate::engine_host::call_without_timeout_cancellable("pdf.documentEditSession",&json!({"session":id}),&worker_cancel); if let Err(error)=result {let _=sender.send(Err(error.message));} if let Ok(mut sessions)=SESSIONS.lock(){sessions.remove(&id);}});
        let mut session=Self{id,commands,results,cancel,_subscription:subscription}; session.exchange(json!({"op":"open","bytesBase64":STANDARD.encode(bytes)}))?; Ok(session)
    }
    pub(in super::super) fn exchange(&mut self, command: Value) -> Result<Value,String> {
        if self.cancel.is_cancelled(){return Err("PDF document session cancelled".into())}
        self.commands.send(command).map_err(|_|"PDF document session closed")?;
        loop {match self.results.recv_timeout(Duration::from_millis(100)){Ok(value)=>return value,Err(mpsc::RecvTimeoutError::Timeout) if !self.cancel.is_cancelled()=>continue,Err(mpsc::RecvTimeoutError::Timeout)=>return Err("PDF document session cancelled".into()),Err(mpsc::RecvTimeoutError::Disconnected)=>return Err("PDF document session closed".into())}}
    }
}
impl Drop for Session {fn drop(&mut self) {self.cancel.cancel(); if let Ok(mut sessions)=SESSIONS.lock(){sessions.remove(&self.id);}}}
pub(crate) fn dispatch(request_id:u64, operation:&str, input:&Value, original:&Value, cancel:&CancelToken) -> Result<Value,String> {
    if operation!="pdfDocument.next" {return Err("invalid PDF document callback".into())}
    let id=original.get("session").and_then(Value::as_u64).filter(|n|*n>0).ok_or("invalid PDF document session")?;
    let transport=SESSIONS.lock().map_err(|_|"PDF document session registry poisoned")?.get(&id).cloned().ok_or("PDF document session closed")?;
    {let mut owner=transport.request_id.lock().map_err(|_|"PDF document session owner poisoned")?; match *owner {Some(owner) if owner!=request_id=>return Err("PDF document session belongs to another request".into()),None=>*owner=Some(request_id),_=>{}}}
    if let Some(response)=input.get("response") {let result=if let Some(error)=response.get("error") {Err(error.get("message").and_then(Value::as_str).unwrap_or("PDF document operation failed").to_owned())} else {Ok(response.get("ok").cloned().ok_or("invalid PDF document response")?)}; transport.results.send(result).map_err(|_|"PDF document client closed")?;}
    let receiver=transport.commands.lock().map_err(|_|"PDF document session queue poisoned")?;
    loop {if cancel.is_cancelled() || transport.cancel.is_cancelled(){return Err("PDF document session cancelled".into())} match receiver.recv_timeout(Duration::from_millis(100)){Ok(command)=>return Ok(command),Err(mpsc::RecvTimeoutError::Timeout)=>{},Err(mpsc::RecvTimeoutError::Disconnected)=>return Err("PDF document client closed".into())}}
}
pub(crate) fn close_request(request_id:u64) {if let Ok(sessions)=SESSIONS.lock(){for session in sessions.values(){if session.request_id.lock().ok().is_some_and(|owner|*owner==Some(request_id)){session.cancel.cancel();let _=session.results.send(Err("PDF document session closed".into()));}}}}
