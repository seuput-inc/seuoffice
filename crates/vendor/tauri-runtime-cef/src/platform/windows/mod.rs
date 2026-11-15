// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

mod event_loop;
pub(crate) mod focus;
mod icon;
mod monitor;
mod webview;
mod window;
pub(crate) use webview::destroy_browser_host_window;
