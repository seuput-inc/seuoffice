//! PDF vendor host adapters.

mod binary_job;
mod compress;
pub(crate) mod crypto;
mod file_ops;
mod image_info;
mod image_output;
mod job_source;
pub(crate) mod native_callbacks;
pub(crate) mod native_engine;
mod native_shell;
mod native_tools_engine;
mod native_tools_shell;
mod note_store;
mod page_ops;
mod pdf_codec;
pub(crate) mod pdfium_session;
mod protected_token;
pub(crate) mod reload;
mod signed_commit;
mod store;
mod tabs;
pub(crate) mod text_reader;
mod thumbnail;
mod windows;

use std::sync::Arc;

use shell_contract::{
    Hosts,
    runtime::{AppPaths, PathName},
};
use shell_ipc_d2::pdf::{editor::hosts::PdfEngineHost, tools::PdfToolsEngineHost};

use crate::{host::HostContext, paths::ShellPaths};

pub(crate) fn install(hosts: &mut Hosts, cx: &HostContext) {
    let engine: PdfEngineHost = Arc::new(native_engine::NativePdfEngine::new(
        Arc::new(signed_commit::NativeSigningAuthority::new()),
        Arc::new(protected_token::UnavailableTokenPath),
    ));
    hosts.insert(engine);
    hosts.insert(native_shell::host(cx));
    let tools: PdfToolsEngineHost = Arc::new(native_tools_engine::NativePdfToolsEngine::new(
        ShellPaths::new().path(PathName::UserData),
    ));
    hosts.insert(tools);
    hosts.insert(native_tools_shell::host(cx));
    reload::install(hosts, cx);
}
