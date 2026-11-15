// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

mod event_loop;
mod monitor;
mod modal;
mod taskbar;
mod utils;
mod webview;
mod window;

pub(crate) use window::CefX11Host;
pub(crate) use window::show_inactive;
pub(crate) use modal::{begin as begin_modal, end as end_modal, blocked as modal_blocks, before_close as before_close_modal, native_destroyed as native_destroyed_modal};
pub(crate) use modal::recover_focus as recover_modal_focus;
