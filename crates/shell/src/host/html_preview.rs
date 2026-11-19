//! The `html-preview://view-<id>/` scheme: the document the HTML editor's preview frame loads.
//!
//! Rules follow apps/html/src/main/preview-protocol.ts: the host names a view, the answer is
//! the buffer that view's renderer pushed last with a `<base>` at its document's directory, and
//! it is served as an uncached HTML page. Unlike Electron's handler, which answered any page
//! for any view number, a view is served only its own buffer or, as a presentation view, its
//! owner's (`HtmlEditors::preview_entry`); the scheme boundary in `asset_protocol` has already
//! tied the request to the view whose document asked.

use main_core::docs::lazy_media::MediaAnswer;
use main_core::html::preview_document::{ASSET_SCHEME, PREVIEW_SCHEME, build_preview_document};
use main_core::html::preview_protocol::{PREVIEW_DOCUMENT_HEADERS, PreviewEntry};
use shell_contract::ViewId;

use crate::native::url::Flavor;

/// The view an address names. `Err` is the status Electron's handler answered: 400 for an
/// address that does not parse, 404 for a host that is no view.
pub(crate) fn view_of(uri: &str) -> Result<ViewId, u16> {
    view_of_for(uri, cfg!(windows))
}

/// `localhost_shape`: whether a scheme handler is handed `html-preview://localhost/view-3/` for
/// the page's `http://html-preview.localhost/view-3/`, as wry does on Windows (see
/// `document_assets::docx_original_uri_for`). The view is then the first path segment; read as
/// the host it was `localhost`, and every preview was answered 404 there.
fn view_of_for(uri: &str, localhost_shape: bool) -> Result<ViewId, u16> {
    // `http://html-preview.localhost/view-3/` is the Windows shape of `html-preview://view-3/`;
    // the handler only ever sees its own scheme, so both shapes are read on every OS
    let wire = tauri::Url::parse(uri).map_err(|_| 400u16)?;
    let url = crate::native::url::logical_url_for(crate::native::url::Flavor::LocalhostHttp, &wire)
        .unwrap_or(wire);
    if url.scheme() != PREVIEW_SCHEME {
        return Err(400);
    }
    let named = if localhost_shape && url.host_str() == Some("localhost") {
        url.path()
            .strip_prefix('/')
            .and_then(|rest| rest.split('/').next())
    } else {
        url.host_str()
    };
    // `^view-(\d+)$` on the host, which carries a port when the address names one
    let digits = named
        .filter(|_| url.port().is_none())
        .and_then(|host| host.strip_prefix("view-"))
        .filter(|digits| !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()));
    digits
        .and_then(|digits| digits.parse::<u32>().ok())
        .map(ViewId)
        .ok_or(404)
}

/// The `<base href>` the page loads the document's folder through. The document's address is
/// `html-asset://local/<folder>/` (`assetBaseHref`); `local` stands for no host and the handler
/// reads the path alone, as it does for `md-asset`. WebView2 serves the scheme only as
/// `http://html-asset.localhost/<path>`, so there the placeholder is dropped and the folder is
/// the whole path. Anything that is not such an address is left as it is.
fn page_base_href(flavor: Flavor, base_href: &str) -> String {
    let Ok(logical) = tauri::Url::parse(base_href) else {
        return base_href.to_owned();
    };
    if flavor == Flavor::Scheme
        || logical.scheme() != ASSET_SCHEME
        || logical.host_str() != Some("local")
    {
        return base_href.to_owned();
    }
    let Ok(mut url) = tauri::Url::parse(&format!("http://{ASSET_SCHEME}.localhost/")) else {
        return base_href.to_owned();
    };
    url.set_path(logical.path());
    url.set_query(logical.query());
    url.set_fragment(logical.fragment());
    url.into()
}

/// The page served for an entry: `buildPreviewDocument(text, baseHref)` with the handler's
/// headers.
pub(crate) fn answer(entry: &PreviewEntry) -> MediaAnswer {
    let base_href = entry
        .base_href
        .as_deref()
        .map(|href| page_base_href(Flavor::CURRENT, href));
    let body = build_preview_document(&entry.text, base_href.as_deref()).into_bytes();
    let mut headers: Vec<(String, String)> = PREVIEW_DOCUMENT_HEADERS
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect();
    headers.push(("Content-Length".to_owned(), body.len().to_string()));
    MediaAnswer {
        status: 200,
        headers,
        body,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_host_names_the_view() {
        assert_eq!(view_of("html-preview://view-3/"), Ok(ViewId(3)));
        assert_eq!(
            view_of("html-preview://view-0012/index?x=1#y"),
            Ok(ViewId(12))
        );
        assert_eq!(
            view_of("http://html-preview.localhost/view-7/"),
            Ok(ViewId(7))
        );
    }

    #[test]
    fn the_handler_address_of_windows_names_the_view_in_its_path() {
        // what wry hands the handler on Windows for http://html-preview.localhost/view-2/?v=0
        assert_eq!(
            view_of_for("html-preview://localhost/view-2/?v=0", true),
            Ok(ViewId(2))
        );
        assert_eq!(
            view_of_for("html-preview://localhost/other/", true),
            Err(404)
        );
        assert_eq!(view_of_for("html-preview://localhost/", true), Err(404));
        // elsewhere `localhost` is a host like any other, and names no view
        assert_eq!(
            view_of_for("html-preview://localhost/view-2/", false),
            Err(404)
        );
        for localhost_shape in [false, true] {
            assert_eq!(
                view_of_for("html-preview://view-3/", localhost_shape),
                Ok(ViewId(3))
            );
        }
    }

    #[test]
    fn an_address_that_names_no_view_is_not_found() {
        for uri in [
            "html-preview://view-/",
            "html-preview://view-+3/",
            "html-preview://view-3x/",
            "html-preview://view--3/",
            "html-preview://view-3:80/",
            "html-preview://other/",
            "html-preview://view-4294967296/",
            "http://html-preview.localhost/other-3/",
        ] {
            assert_eq!(view_of(uri), Err(404), "{uri}");
        }
    }

    #[test]
    fn an_address_that_does_not_parse_or_is_another_scheme_is_a_bad_request() {
        for uri in ["", "view-3", "html-asset://view-3/", "file:///view-3/"] {
            assert_eq!(view_of(uri), Err(400), "{uri:?}");
        }
    }

    #[test]
    fn the_page_is_the_buffer_with_the_documents_base_and_is_never_cached() {
        let entry = PreviewEntry {
            text: "<html><head></head><body>x</body></html>".to_owned(),
            base_href: Some("html-asset://local/docs/".to_owned()),
        };
        let page = answer(&entry);
        assert_eq!(page.status, 200);
        let body = String::from_utf8(page.body.clone()).unwrap();
        let base = page_base_href(Flavor::CURRENT, "html-asset://local/docs/");
        assert!(body.contains(&format!(r#"<head><base href="{base}">"#)));
        let header = |name: &str| {
            page.headers
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.as_str())
        };
        assert_eq!(header("Content-Type"), Some("text/html; charset=utf-8"));
        assert_eq!(header("Cache-Control"), Some("no-store"));
        assert_eq!(
            header("Content-Length"),
            Some(page.body.len().to_string().as_str())
        );
    }

    #[test]
    fn the_base_drops_its_placeholder_host_where_webview2_serves_the_scheme() {
        let base = "html-asset://local/C:/My%20Docs/site/";
        assert_eq!(page_base_href(Flavor::Scheme, base), base);
        assert_eq!(
            page_base_href(Flavor::LocalhostHttp, base),
            "http://html-asset.localhost/C:/My%20Docs/site/"
        );
        assert_eq!(
            page_base_href(Flavor::LocalhostHttp, "html-asset://local/home/me/"),
            "http://html-asset.localhost/home/me/"
        );
        for other in [
            "html-asset://token/doc/",
            "https://example.test/",
            "not a url",
        ] {
            assert_eq!(page_base_href(Flavor::LocalhostHttp, other), other);
        }
    }

    #[test]
    fn an_unsaved_document_gets_no_base_and_the_authors_base_is_kept() {
        let bare = answer(&PreviewEntry {
            text: "<p>x</p>".to_owned(),
            base_href: None,
        });
        assert_eq!(bare.body, b"<p>x</p>");
        let authored = answer(&PreviewEntry {
            text: r#"<head><base href="https://example.test/"></head>"#.to_owned(),
            base_href: Some("html-asset://local/docs/".to_owned()),
        });
        assert!(
            !String::from_utf8(authored.body)
                .unwrap()
                .contains("html-asset")
        );
    }
}
