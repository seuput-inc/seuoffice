//! Production hosts for the D2 document editors.

mod docs_crypto;
mod docs_csv;
mod docs_engine;
pub(crate) mod docs_headless;
mod docs_image;
mod docs_lazy;
mod docs_lazy_zip;
mod docs_legacy;
mod docs_media;
mod docs_merge;
mod docs_pdf;
mod docs_print;
mod docs_shell;
pub(crate) mod docs_tabs;
mod docs_vendor;
mod epub_book;
mod epub_engine;
mod epub_open_files;
mod epub_ranges;
mod epub_save;
mod epub_shell;
pub(crate) mod html_headless;
mod html_host;
pub(crate) mod hwp_headless;
pub(crate) mod hwp_host;
pub(crate) mod hwp_parse;
mod hwp_tracked;
mod job_migration;
pub(crate) mod markdown_headless;
mod markdown_host;
mod native_save;
pub(crate) mod odt_export;
pub(crate) mod pdf;

pub(super) static RESULT_STORE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

use shell_contract::Hosts;

struct DocsMedia {
    host: docs_lazy::LazyHost,
    fonts_dir: String,
}

static DOCS_MEDIA: std::sync::OnceLock<DocsMedia> = std::sync::OnceLock::new();

pub(crate) fn docx_media_response_for_permit(
    permit: main_core::docs::byte_handoff::MediaPermit,
    uri: &str,
    cancel: shell_contract::runtime::CancelToken,
) -> Result<main_core::docs::lazy_media::MediaAnswer, String> {
    let media = DOCS_MEDIA
        .get()
        .ok_or_else(|| "The document media service is not available".to_owned())?;
    media
        .host
        .response_for(permit, uri, &media.fonts_dir, cancel)
}

thread_local! {
    static ACTIVE_DOCS: std::cell::Cell<Option<(shell_contract::ViewId, bool)>> =
        const { std::cell::Cell::new(None) };
}

pub(crate) fn set_active_docs_resolver(
    view: Option<shell_contract::ViewId>,
    null_if_destroyed: bool,
) {
    ACTIVE_DOCS.with(|active| active.set(view.map(|view| (view, null_if_destroyed))));
}

use super::HostContext;

pub fn install(hosts: &mut Hosts, cx: &HostContext) -> Result<(), String> {
    if DOCS_MEDIA.get().is_none() {
        let fonts_dir = super::d1::template_fonts_dir(&cx.app)?;
        let media = DocsMedia {
            host: docs_lazy::LazyHost::new()?,
            fonts_dir,
        };
        DOCS_MEDIA
            .set(media)
            .map_err(|_| "document media service is already initialized".to_owned())?;
    }
    let media = DOCS_MEDIA
        .get()
        .ok_or("document media service is not available")?;
    hosts.insert(docs_engine::host(&media.host));
    let docs_shell = docs_shell::host(cx, hosts)?;
    hosts.insert(docs_shell);
    hosts.insert(html_host::engine(cx));
    hosts.insert(html_host::shell(cx));
    let markdown_shell = markdown_host::shell(cx, hosts)?;
    hosts.insert(markdown_host::engine(cx));
    hosts.insert(markdown_shell);
    hosts.insert(epub_shell::host(cx.ui.clone()));
    hosts.insert(epub_engine::host());
    hwp_host::install(hosts, cx);
    pdf::install(hosts, cx);
    Ok(())
}
