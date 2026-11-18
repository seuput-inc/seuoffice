// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use cef::{CefString, ImplSchemeRegistrar, SchemeOptions, SchemeRegistrar};

pub(crate) const SWITCH: &str = "tauri-cef-native-schemes";

pub(crate) fn valid_scheme(name: &str) -> bool {
  let mut bytes = name.bytes();
  matches!(bytes.next(), Some(b'a'..=b'z'))
    && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'+' | b'-' | b'.'))
}

pub(crate) fn child_schemes(args: impl IntoIterator<Item = std::ffi::OsString>) -> Vec<String> {
  let prefix = format!("--{SWITCH}=");
  let mut child = false;
  let mut value = None;
  for arg in args {
    let Some(arg) = arg.to_str() else { continue };
    if arg.starts_with("--type=") { child = true; }
    if let Some(schemes) = arg.strip_prefix(&prefix) { value = Some(schemes.to_owned()); }
  }
  if !child { return Vec::new(); }
  let Some(value) = value else { return Vec::new() };
  let schemes: Vec<_> = value.split(',').collect();
  if schemes.iter().any(|name| !valid_scheme(name)) {
    eprintln!("invalid native custom scheme list in CEF child process");
    std::process::exit(1);
  }
  schemes.into_iter().map(str::to_owned).collect()
}

pub(crate) fn register(registrar: Option<&mut SchemeRegistrar>, schemes: &[String]) {
  let Some(registrar) = registrar else {
    if !schemes.is_empty() {
      log::error!("CEF did not provide a custom-scheme registrar");
      std::process::exit(1);
    }
    return;
  };
  let options = (SchemeOptions::STANDARD.get_raw() | SchemeOptions::SECURE.get_raw()
    | SchemeOptions::CORS_ENABLED.get_raw() | SchemeOptions::FETCH_ENABLED.get_raw()) as i32;
  for scheme in schemes {
    if registrar.add_custom_scheme(Some(&CefString::from(scheme.as_str())), options) == 0 {
      log::error!("CEF refused native custom scheme {scheme:?}");
      std::process::exit(1);
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn child_receives_only_valid_schemes_and_browser_arguments_do_not_configure_them() {
    let switch = format!("--{SWITCH}=seuoffice-docx-media,html-asset,md-asset");
    let args = |items: &[&str]| items.iter().map(|s| std::ffi::OsString::from(*s)).collect::<Vec<_>>();
    assert!(child_schemes(args(&["app", &switch])).is_empty());
    assert_eq!(child_schemes(args(&["app", "--type=renderer", &switch])), vec!["seuoffice-docx-media", "html-asset", "md-asset"]);
    assert!(!valid_scheme("https://example.test"));
    assert!(!valid_scheme("UPPER"));
  }
}
