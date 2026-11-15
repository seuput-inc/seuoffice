// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

#![allow(clippy::arc_with_non_send_sync)]
#![allow(clippy::too_many_arguments)]

mod browser_lifetime;
mod cef_impl;
mod context_menu;
pub use context_menu::{
  ContextMenuDone, ContextMenuParams, EditFlags, set_context_menu_handler,
};
mod devtools;
mod dialog;
/// The `Last Version` breadcrumb in the root cache path, and what a rollback to an older CEF
/// does with the profile a newer one left there.
mod downgrade;
/// The diagnostic environment variables Chromium and CEF read, and the policy over them.
mod environment;
mod external_message_pump;
mod frame;
#[cfg(target_os = "windows")]
#[path = "platform/windows/generate_caption.rs"]
pub mod generate_caption;
#[cfg(target_os = "linux")]
#[path = "platform/linux/generate_caption.rs"]
pub mod generate_caption;
mod inactive_window;
mod modal_window;
mod native_scheme;
mod native_context;
mod native_invoke;
mod native_evaluation;
pub use native_evaluation::NativeEvaluation;
mod native_menu;
pub use native_menu::{NativeMenuActivation, capture_native_menu_activation};
pub use webview::NativeProtocolAdmission;
pub use native_invoke::{NativeChannelQueue, NativeReplySink, NativeSyncRequest};
mod native_scheme_response;
pub use native_scheme_response::{NativeSchemeCancelSubscription, NativeSchemeResponseTarget, PreparedNativeSchemeResponse, AcceptedNativeSchemeResponse, RejectedNativeSchemeResponse};
mod browser_close;
pub use browser_close::{NativeBrowserClose, NativeBrowserCloseOutcome, NativeBrowserCloseSubscription, NativeBrowserCreationOutcome, NativeBrowserCreationSubscription, NativeBrowserSubmissionGuard};
mod native_reload;
pub use native_reload::{NativeReloadAdmission, NativeReloadFailureSubscription, NativeReloadIdentity, NativeReloadReservation};
mod native_browser_protocol;
pub use native_browser_protocol::NativeBrowserProtocol;
mod native_window_close;
pub use native_window_close::{NativeWindowClose, NativeWindowCloseOutcome, NativeWindowCloseSubscription, NativeWindowClosingSubscription, NativeWindowEffect, NativeWindowSubmissionGuard};
mod native_observed_navigation;
pub use native_observed_navigation::{NativeObservedDocument, NativeObservedDocumentLoss, NativeObservedDocumentSubscription, NativeObservedNavigation};
mod browser_request;
pub use browser_request::{BrowserResourceRequest, BrowserResourceRequestHandler};
mod network_session;
pub use network_session::{NativeNetworkSession, NetworkSessionRequest, NetworkSessionRequestHandler};
pub use native_context::{NATIVE_ATTESTATION_HEADER, NativeContextGatePlan, NativeContextRevocation, NativeContextToken, NativeDocumentReady, NativeEvalContext, redeem_native_context};
pub use cef_impl::native_eval::NativeEvalError;
mod navigation_event;
pub use navigation_event::NavigationStart;
pub use inactive_window::{AsCefWindowDispatcher, WindowCefExt};
#[cfg(target_os = "macos")]
pub use inactive_window::{AsCefRuntimeHandle, native_app_key_window_handle};
pub use dialog::{
  NativeDialogKind, NativeDialogObservation, NativeDialogSnapshot, NativeDialogToken,
};
pub use environment::DebugEnvironment;
mod frame_navigation;
/// The languages the user asked their operating system for.
mod locale;
mod macros;
mod offscreen;
pub use offscreen::{CursorHandler, Offscreen, OffscreenView};
mod platform;
mod popup;
mod renderer_process;
mod native_renderer_process;
pub use native_renderer_process::{NativeRendererProcess, NativeRendererProcessIdentity, NativeRendererProcessSubscription};
mod runtime;
// `SandboxPolicy` itself is public API and lives in `runtime`; this module holds the
// decision behind it.
mod sandbox;
mod scheme_request;
pub use scheme_request::SchemeRequestProvenance;
/// Helpers for the Chromium command line the runtime hands to CEF.
mod switches;
mod tauri_ext;
mod webview;
mod view_event;
pub use view_event::{ViewEvent, ViewEventHandler};
pub use devtools::{DevToolsMessageIdExhausted, allocate_devtools_message_id};
pub use frame::{FrameEvent, FrameEventHandler, FrameEventKind};
pub use frame_navigation::{FrameNavigationState, NativeDocumentToken, NativeNavigationToken};
mod window;
mod window_builder;
mod window_handle;

pub use cef::sys::CEF_API_VERSION_LAST;
#[cfg(target_os = "macos")]
pub use platform::macos::setup_application as prepare_macos_application;
pub use runtime::*;
pub use tauri_ext::*;
/// Marks the application entry point so non-browser CEF processes (renderer, GPU, ...) are handled.
pub use tauri_macros::cef_entry_point;
pub use webview::*;
pub use window::{CefWindowDispatcher, NativeWindowHandle, NativeWindowToken};
pub use window_builder::WindowBuilderWrapper;
