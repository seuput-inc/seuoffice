//! PDF vendor host adapters.

pub(crate) mod native_callbacks;
pub(crate) mod native_engine;
pub(crate) mod pdfium_session;
mod binary_job;
mod compress;
mod crypto;
mod signed_commit;
#[cfg(target_os = "linux")]
mod native_token_dialog;
mod file_ops;
mod image_info;
mod image_output;
mod job_source;
mod note_store;
mod page_ops;
mod store;
mod thumbnail;
mod pdf_codec;
mod native_shell;
mod native_tools_shell;
mod tabs;
mod windows;
pub(crate) mod text_reader;
pub(crate) mod reload;
