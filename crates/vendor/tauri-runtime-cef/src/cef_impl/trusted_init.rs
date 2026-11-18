// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use serde::{Deserialize, Serialize};

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct Script {
  pub(crate) source: String,
  pub(crate) main_only: bool,
}

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct Initialization {
  pub(crate) seed: String,
  pub(crate) browser_pid: u32,
  pub(crate) mode: crate::TrustedInitMode,
  pub(crate) scripts: Vec<Script>,
}

pub(crate) fn random_capability() -> Option<String> {
  let mut bytes = [0u8; 32];
  getrandom::fill(&mut bytes).ok()?;
  let mut encoded = String::with_capacity(64);
  const HEX: &[u8; 16] = b"0123456789abcdef";
  for byte in bytes {
    encoded.push(HEX[(byte >> 4) as usize] as char);
    encoded.push(HEX[(byte & 15) as usize] as char);
  }
  Some(encoded)
}

