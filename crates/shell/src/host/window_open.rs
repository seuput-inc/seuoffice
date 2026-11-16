//! `window.open` and `target=_blank` of the shell's pages. CEF is always
//! answered "no new window"; what the Electron build's `setWindowOpenHandler`
//! of each app did instead is this table, by the surface of the page that
//! asked: open the URL in the operating system's browser (and mail client),
//! or nothing. A page without a bridge has no surface and gets nothing.

use shell_contract::Surface;
use shell_contract::{ViewId, runtime::{RendererGeneration, Views}};
use crate::native::DocumentContext;

use crate::ipc::native_owner::{self, NativeOwnerAuthority};
use crate::views::ViewTable;

/// What a page may open outside the app
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenPolicy {
    /// nothing opens
    Deny,
    /// `http:` and `https:` (`safeExternalUrl(url)`)
    Web,
    /// `http:`, `https:` and `mailto:`
    WebAndMail,
}

/// One match on the surface: the `setWindowOpenHandler` of the page's app
pub fn policy_of(surface: Option<Surface>) -> OpenPolicy {
    use OpenPolicy::*;
    match surface {
        Some(
            Surface::Docs
            | Surface::Slides
            | Surface::Shell
            | Surface::ShellAgent
            | Surface::ShellDocPicker
            | Surface::ShellGenerate
            | Surface::ShellPdfTools
            | Surface::ShellQuickLook,
        ) => Web,
        Some(Surface::Html | Surface::Hwp | Surface::Markdown | Surface::Pdf) => WebAndMail,
        Some(
            Surface::Diagram
            | Surface::Epub
            | Surface::Mindmap
            | Surface::Sheets
            | Surface::ShellPdfPassword
            | Surface::ShellInbox
            | Surface::ShellUpdate,
        )
        | None => Deny,
    }
}

/// The URL to hand to the OS for a page's `window.open(url)`, or none
pub fn external_target(policy: OpenPolicy, url: &str) -> Option<String> {
    use main_core::packages::electron_utils::safe_external_url::{
        SafeExternalUrlOptions, safe_external_url,
    };
    match policy {
        OpenPolicy::Deny => None,
        OpenPolicy::Web => safe_external_url(Some(url), None),
        OpenPolicy::WebAndMail => safe_external_url(
            Some(url),
            Some(&SafeExternalUrlOptions {
                allowed_protocols: Some(vec!["http:".into(), "https:".into(), "mailto:".into()]),
            }),
        ),
    }
}

/// An exact originating native document asked to open a URL. The constructor
/// supplies its retained creation generation and the popup producer's token;
/// a current-view lookup alone never authorizes the effect.
pub fn requested(
    surface: Option<Surface>, views: &ViewTable, view: ViewId,
    generation: &RendererGeneration, origin: &DocumentContext, url: &str,
) -> Result<(), String> {
    let Some(target) = external_target(policy_of(surface), url) else {
        crate::trace::trace!("window", "window.open of {surface:?} refused: {url}");
        return Ok(());
    };
    if !views.sender_of(view).is_some_and(|sender| Some(sender.surface) == surface) {
        return Err("The external-link source surface is no longer available.".into());
    }
    let owner = native_owner::capture_owner(views, view, generation)?;
    if !owner.context.authority_ref::<NativeOwnerAuthority>().is_some_and(|proof| proof.origin == *origin) {
        return Err("The external-link source does not match the native popup origin.".into());
    }
    let cancel = owner.loss_token().ok_or("The external-link source owner lease is unavailable.")?;
    std::thread::Builder::new()
        .name("shell-open-external".into())
        .spawn(move || {
            // Admission selects the immutable operation; the OS call must not
            // execute under a native-context, renderer or cancellation lock.
            let open = native_owner::with_owner_effect(&owner, &cancel, || move || super::os_shell::open_external(&target));
            match open {
                Some(open) => {
                    if let Err(error) = open() { eprintln!("shell: opening an external link: {error}"); }
                }
                None => eprintln!("shell: external-link source was revoked before dispatch"),
            }
        })
        .map(|_| ())
        .map_err(|error| format!("cannot start the thread that opens {url}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(surface: Surface, url: &str) -> Option<String> {
        external_target(policy_of(Some(surface)), url)
    }

    #[test]
    fn each_surface_has_its_policy() {
        assert_eq!(policy_of(Some(Surface::Docs)), OpenPolicy::Web);
        assert_eq!(policy_of(Some(Surface::Shell)), OpenPolicy::Web);
        assert_eq!(policy_of(Some(Surface::ShellQuickLook)), OpenPolicy::Web);
        assert_eq!(policy_of(Some(Surface::Markdown)), OpenPolicy::WebAndMail);
        assert_eq!(policy_of(Some(Surface::Pdf)), OpenPolicy::WebAndMail);
        assert_eq!(policy_of(Some(Surface::Sheets)), OpenPolicy::Deny);
        assert_eq!(policy_of(Some(Surface::ShellUpdate)), OpenPolicy::Deny);
        assert_eq!(policy_of(None), OpenPolicy::Deny);
    }

    #[test]
    fn what_may_leave_the_app() {
        for surface in [Surface::Docs, Surface::Html] {
            assert_eq!(
                target(surface, "https://example.com/a?b=1").as_deref(),
                Some("https://example.com/a?b=1")
            );
            assert_eq!(target(surface, "javascript:alert(1)"), None);
            assert_eq!(target(surface, "file:///etc/passwd"), None);
            assert_eq!(target(surface, "not a url"), None);
        }
        // mailto only where the app allowed it
        assert_eq!(target(Surface::Docs, "mailto:a@example.com"), None);
        assert_eq!(
            target(Surface::Html, "mailto:a@example.com").as_deref(),
            Some("mailto:a@example.com")
        );
        // the URL is handed over trimmed
        assert_eq!(
            target(Surface::Docs, "  https://example.com/x \n").as_deref(),
            Some("https://example.com/x")
        );
        assert_eq!(target(Surface::Diagram, "https://example.com/"), None);
        assert_eq!(
            external_target(policy_of(None), "https://example.com/"),
            None
        );
    }

    #[test]
    fn generate_is_web_only_without_changing_other_apps_mail_policy() {
        assert_eq!(target(Surface::ShellGenerate, "https://example.com/").as_deref(), Some("https://example.com/"));
        assert_eq!(target(Surface::ShellGenerate, "http://example.com/").as_deref(), Some("http://example.com/"));
        for url in ["mailto:a@example.com", "file:///tmp/document", "javascript:alert(1)", "data:text/html,x"] {
            assert_eq!(target(Surface::ShellGenerate, url), None);
        }
        for surface in [Surface::Html, Surface::Hwp, Surface::Markdown, Surface::Pdf] {
            assert_eq!(target(surface, "mailto:a@example.com").as_deref(), Some("mailto:a@example.com"));
        }
        assert_eq!(external_target(policy_of(None), "https://example.com/"), None);
    }
}
