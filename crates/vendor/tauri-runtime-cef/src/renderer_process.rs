// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

//! Browser identities, including popup browsers. A snapshot is not a crash permit.
use std::{collections::HashMap, sync::{LazyLock, Mutex}};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Process { pid: Option<u32>, generation: u64 }
#[derive(Default)]
struct Registry { browsers: HashMap<i32, Process> }
impl Registry {
  fn created(&mut self, browser: i32) {
    self.browsers.insert(browser, Process { pid: None, generation: 0 });
  }
  fn invalidate(&mut self, browser: i32) {
    if let Some(process) = self.browsers.get_mut(&browser) {
      process.pid = None;
      process.generation = process.generation.saturating_add(1);
    }
  }
  fn identified(&mut self, browser: i32, pid: u32) {
    if pid != 0 && let Some(process) = self.browsers.get_mut(&browser) {
      if process.pid.is_some_and(|previous| previous != pid) {
        process.generation = process.generation.saturating_add(1);
      }
      process.pid = Some(pid);
    }
  }
  fn closed(&mut self, browser: i32) { self.browsers.remove(&browser); }
}
static REGISTRY: LazyLock<Mutex<Registry>> = LazyLock::new(|| Mutex::new(Registry::default()));
pub(crate) fn created(browser: i32) { if let Ok(mut state) = REGISTRY.lock() { state.created(browser); } }
pub(crate) fn invalidate(browser: i32) { if let Ok(mut state) = REGISTRY.lock() { state.invalidate(browser); } }
pub(crate) fn identified(browser: i32, pid: u32) { if let Ok(mut state) = REGISTRY.lock() { state.identified(browser, pid); } }
pub(crate) fn closed(browser: i32) { if let Ok(mut state) = REGISTRY.lock() { state.closed(browser); } }

#[cfg(test)]
mod tests {
  use super::*;
  #[test]
  fn popup_and_root_are_separate_and_switches_revoke_identity() {
    let mut registry = Registry::default();
    registry.created(1);
    registry.created(2);
    registry.identified(1, 100);
    registry.identified(2, 100);
    registry.invalidate(1);
    assert_eq!(registry.browsers[&1].pid, None);
    assert_eq!(registry.browsers[&2].pid, Some(100));
    registry.identified(1, 200);
    assert_eq!(registry.browsers[&1], Process { pid: Some(200), generation: 1 });
    registry.closed(2);
    assert!(!registry.browsers.contains_key(&2));
  }
}
