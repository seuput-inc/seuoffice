//! The Windows adapter (WebView2 through wry; `webview2-com` 0.39 on `windows` 0.62).
//!
//! This file wires the files slice S1w owns; each implements the trait named in
//! `platform/mod.rs`:
//!
//! - `observer.rs`: `Observe` (`NavigationCompleted`, `ProcessFailed`, `SourceChanged`, focus)
//! - `page.rs`: `PageDriver` and `PrintOps`, with the CDP driver in `cdp.rs`
//! - `window.rs`: `WindowOps`
//! - `policy.rs`: `PolicyOps`
//! - `spell.rs`: `SpellOps` over the Windows spell checker and its user dictionary
//! - `caption.rs`: `CaptionStyle`, `configure`, `update` (Generate's DWM caption, Windows 11
//!   build 22000+; Windows 10 keeps the default caption and logs once)
//!
//! Each file starts with `use super::Platform;` and `impl <Trait> for Platform`.

pub mod caption;
mod cdp;
mod observer;
mod page;
mod policy;
mod spell;
mod window;

/// The marker type every Windows trait impl is written for.
pub(crate) struct Platform;
