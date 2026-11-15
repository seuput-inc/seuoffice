// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use std::{collections::HashMap, sync::{Arc, LazyLock, Mutex, Weak, atomic::{AtomicBool, Ordering}}};
use cef::*;

type Completion = Box<dyn FnOnce(Result<(), String>) + Send>;
struct Listener { active: AtomicBool, completion: Mutex<Option<Completion>>, inline: bool }
#[derive(Default)]
struct Exit { result: Option<Result<(), String>>, listeners: Vec<Arc<Listener>> }
struct Process {
  identity: NativeRendererProcessIdentity,
  _native: cef::NativeRendererProcess,
  exit: Mutex<Exit>,
}
static PROCESSES: LazyLock<Mutex<HashMap<NativeRendererProcessIdentity, Arc<Process>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// One native RenderProcessHost process incarnation, shared by its documents.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct NativeRendererProcessIdentity { child_id: i32, pid: u32, observer: u64 }
impl NativeRendererProcessIdentity {
  pub fn child_id(&self) -> i32 { self.child_id }
  pub fn pid(&self) -> u32 { self.pid }
  #[cfg(test)]
  pub(crate) fn test_identity(child_id: i32, pid: u32, observer: u64) -> Self {
    Self { child_id, pid, observer }
  }
}
#[derive(Clone)]
pub struct NativeRendererProcess { process: Arc<Process> }
impl std::fmt::Debug for NativeRendererProcess {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("NativeRendererProcess").field("identity", &self.process.identity).finish_non_exhaustive()
  }
}
pub struct NativeRendererProcessSubscription { listener: Arc<Listener> }
impl Drop for NativeRendererProcessSubscription {
  fn drop(&mut self) { self.listener.active.store(false, Ordering::Release); }
}
fn deliver(listener: Arc<Listener>, result: Result<(), String>) {
  if listener.inline {
    if !listener.active.swap(false, Ordering::AcqRel) { return; }
    let completion = listener.completion.lock().unwrap_or_else(|error| error.into_inner()).take();
    if let Some(completion) = completion { completion(result); }
    return;
  }
  crate::native_evaluation::defer_completion(Box::new(move || {
    if !listener.active.swap(false, Ordering::AcqRel) { return; }
    let completion = listener.completion.lock().unwrap_or_else(|error| error.into_inner()).take();
    if let Some(completion) = completion { completion(result); }
  }));
}
fn same_process_incarnation(captured: Option<&NativeRendererProcessIdentity>, actual: &NativeRendererProcessIdentity) -> bool {
  captured == Some(actual)
}

impl NativeRendererProcess {
  pub fn identity(&self) -> NativeRendererProcessIdentity { self.process.identity.clone() }
  pub fn when_exited(&self, completion: impl FnOnce(Result<(), String>) + Send + 'static) -> Result<NativeRendererProcessSubscription, String> {
    self.subscribe(false, completion)
  }

  /// Internal native-loss observer. The callback only mutates the short
  /// authority gate and defers every external effect.
  pub(crate) fn when_lost(&self, completion: impl FnOnce(Result<(), String>) + Send + 'static) -> Result<NativeRendererProcessSubscription, String> {
    self.subscribe(true, completion)
  }

  fn subscribe(&self, inline: bool, completion: impl FnOnce(Result<(), String>) + Send + 'static) -> Result<NativeRendererProcessSubscription, String> {
    let listener = Arc::new(Listener { active: AtomicBool::new(true), completion: Mutex::new(Some(Box::new(completion))), inline });
    let result = {
      let mut exit = self.process.exit.lock().map_err(|_| "Native process exit state was poisoned.")?;
      if exit.result.is_none() {
        exit.listeners.retain(|listener| listener.active.load(Ordering::Acquire));
        exit.listeners.push(listener.clone());
      }
      exit.result.clone()
    };
    if let Some(result) = result { deliver(listener.clone(), result); }
    Ok(NativeRendererProcessSubscription { listener })
  }

  /// Queries every actual native owner on CEF UI immediately before the
  /// original-context Page.crash admission. No Rust context registry is locked.
  pub fn permits_termination_for(&self, documents: &[crate::NativeContextToken]) -> Result<bool, String> {
    if currently_on(sys::cef_thread_id_t::TID_UI.into()) == 0 {
      return Err("Native renderer ownership must be inspected on CEF UI.".into());
    }
    if documents.is_empty() || documents.len() > 256 { return Ok(false); }
    let mut retired = CefStringList::new();
    for document in documents {
      let Some(identity) = document.native_identity.as_ref() else { return Ok(false) };
      if !same_process_incarnation(document.process_identity.get(), &self.process.identity)
        || identity.renderer_child_id != self.process.identity.child_id || identity.world_id != 0
        || identity.registration == 0 { return Ok(false); }
      if !retired.append(&format!("{}:{}", document.browser_id(), identity.document_token)) {
        return Err("Native renderer ownership list could not be allocated.".into());
      }
    }
    Ok(self.process._native.permits_termination(Some(&retired)) != 0)
  }

  pub(crate) fn from_native(native: &cef::NativeRendererProcess) -> Option<Self> {
    let identity = NativeRendererProcessIdentity {
      child_id: native.renderer_child_id(), pid: native.process_id(), observer: native.observer_identity(),
    };
    if identity.child_id <= 0 || identity.pid == 0 || identity.pid == std::process::id() || identity.observer == 0 { return None; }
    let process = {
      let mut processes = PROCESSES.lock().ok()?;
      if let Some(process) = processes.get(&identity) { return Some(Self { process: process.clone() }); }
      let process = Arc::new(Process { identity: identity.clone(), _native: native.clone(), exit: Mutex::default() });
      processes.insert(identity.clone(), process.clone());
      process
    };
    let mut callback = ProcessExitCallback::new(Arc::downgrade(&process));
    if native.observe_exit(Some(&mut callback)) == 0 {
      let removed = PROCESSES.lock().ok().and_then(|mut processes| processes.remove(&identity));
      drop(removed);
      return None;
    }
    Some(Self { process })
  }
}
wrap_native_renderer_process_callback! {
  struct ProcessExitCallback { process: Weak<Process> }
  impl NativeRendererProcessCallback {
    fn on_exited(&self, actual_exit: i32) {
      let Some(process) = self.process.upgrade() else { return };
      let result = if actual_exit != 0 { Ok(()) } else { Err("Native renderer host was destroyed without an observed process exit.".into()) };
      let listeners = {
        let mut exit = process.exit.lock().unwrap_or_else(|error| error.into_inner());
        if exit.result.is_some() { return; }
        exit.result = Some(result.clone());
        std::mem::take(&mut exit.listeners)
      };
      let removed = PROCESSES.lock().ok().and_then(|mut processes| processes.remove(&process.identity));
      drop(removed);
      for listener in listeners { deliver(listener, result.clone()); }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  #[test]
  fn process_incarnation_not_pid_or_document_defines_identity() {
    let first = NativeRendererProcessIdentity { child_id: 4, pid: 100, observer: 1 };
    let reused = NativeRendererProcessIdentity { child_id: 4, pid: 100, observer: 2 };
    assert_ne!(first, reused);
    assert_eq!(first, first.clone());
    assert!(same_process_incarnation(Some(&first), &first));
    assert!(!same_process_incarnation(Some(&first), &reused));
    assert!(!same_process_incarnation(None, &first));
  }

  #[test]
  fn loss_claim_is_synchronous_and_single_use() {
    let delivered = Arc::new(AtomicBool::new(false));
    let captured = delivered.clone();
    let listener = Arc::new(Listener {
      active: AtomicBool::new(true),
      completion: Mutex::new(Some(Box::new(move |_| { captured.store(true, Ordering::Release); }))),
      inline: true,
    });
    deliver(listener.clone(), Ok(()));
    assert!(delivered.load(Ordering::Acquire));
    delivered.store(false, Ordering::Release);
    deliver(listener, Ok(()));
    assert!(!delivered.load(Ordering::Acquire));
  }
}
