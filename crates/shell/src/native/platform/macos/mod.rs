//! The macOS adapter (WKWebView through wry, `objc2` 0.6, `objc2-web-kit` 0.3, `objc2-app-kit` 0.3).
//!
//! This file only wires the files slice S1m owns; each implements the trait named in
//! `platform/mod.rs`:
//!
//! - `observer.rs`: `Observe` (chained `WKNavigationDelegate`)
//! - `page.rs`: `PageDriver`, with helpers in `edit.rs`
//! - `print.rs`: `PrintOps`
//! - `window.rs`: `WindowOps`
//! - `policy.rs`: `PolicyOps`
//! - `spell.rs`: `SpellOps`
//!
//! Each file starts with `use super::Platform;` and `impl <Trait> for Platform`.

mod edit;
mod observer;
mod page;
mod policy;
mod print;
mod spell;
mod window;

/// The marker type every macOS trait impl is written for.
pub(crate) struct Platform;
