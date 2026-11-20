//! The URL a webview navigates to or sees for a custom scheme, by platform.
//!
//! The shell names a document by its logical URL, `html-asset://<authority>/<path>`. WKWebView
//! serves a custom scheme as it is named. WebView2 (through wry) only serves
//! `http://<scheme>.localhost/<path>`, so on Windows the authority becomes the first path
//! segment: `http://html-asset.localhost/<authority>/<path>`. Navigation hooks, request
//! policies and `ObservedDocument::url` see the platform shape; grants and tokens are written
//! in the logical one. [`platform_url`] and [`logical_url`] are the only translators for such
//! schemes. A scheme handler is handed a third shape there, which
//! [`handler_logical_url_for`] reads.
//!
//! The renderer pages of `genoffice-app-<host>` keep their own translators in
//! `app_scheme` (`webview_url`, `electron_url`): their host lives in the scheme name.

use tauri::Url;

/// Which shape a platform serves custom schemes in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flavor {
    /// `<scheme>://<authority>/<path>` (WKWebView).
    Scheme,
    /// `http://<scheme>.localhost/<authority>/<path>` (WebView2).
    LocalhostHttp,
}

impl Flavor {
    pub const CURRENT: Self = if cfg!(windows) {
        Self::LocalhostHttp
    } else {
        Self::Scheme
    };
}

/// Schemes a webview resolves itself; they have no logical shape.
fn is_builtin(scheme: &str) -> bool {
    matches!(
        scheme,
        "http" | "https" | "about" | "data" | "blob" | "file" | "ws" | "wss" | "tauri"
    )
}

/// The URL to give the webview for a logical custom-scheme URL; anything else is unchanged.
pub fn platform_url(logical: &Url) -> Url {
    platform_url_for(Flavor::CURRENT, logical)
}

/// The logical URL behind a platform URL; `None` when `url` is not a custom scheme's.
pub fn logical_url(url: &Url) -> Option<Url> {
    logical_url_for(Flavor::CURRENT, url)
}

/// `url` as a logical URL when it is one's platform shape, otherwise unchanged.
pub fn into_logical(url: Url) -> Url {
    logical_url(&url).unwrap_or(url)
}

/// [`platform_url`] on a URL text; text that is not a URL is returned unchanged.
pub fn platform_url_text(logical: &str) -> String {
    Url::parse(logical).map_or_else(|_| logical.to_owned(), |url| platform_url(&url).into())
}

/// The logical URL of a request as a scheme handler is handed it; text that is not a URL is
/// returned unchanged.
pub fn handler_logical_url_text(text: &str) -> String {
    Url::parse(text).map_or_else(
        |_| text.to_owned(),
        |url| handler_logical_url_for(Flavor::CURRENT, url).into(),
    )
}

/// WKWebView hands a handler the logical URL. For the page's
/// `http://<scheme>.localhost/<authority>/<path>` wry hands a WebView2 handler
/// `<scheme>://localhost/<authority>/<path>`: its `revert_uri_work_around` puts the scheme
/// back and nothing else. That is neither shape, and read as a logical URL its authority is
/// `localhost`.
pub fn handler_logical_url_for(flavor: Flavor, url: Url) -> Url {
    let url = logical_url_for(flavor, &url).unwrap_or(url);
    if flavor != Flavor::LocalhostHttp
        || is_builtin(url.scheme())
        || url.host_str() != Some("localhost")
    {
        return url;
    }
    let rest = url.path().strip_prefix('/').unwrap_or("");
    let (authority, path) = match rest.split_once('/') {
        Some((authority, path)) => (authority, format!("/{path}")),
        None => (rest, "/".to_owned()),
    };
    if authority.is_empty() {
        return url;
    }
    let Ok(mut out) = Url::parse(&format!("{}://{authority}/", url.scheme())) else {
        return url;
    };
    out.set_path(&path);
    out.set_query(url.query());
    out.set_fragment(url.fragment());
    out
}

pub fn platform_url_for(flavor: Flavor, logical: &Url) -> Url {
    if flavor == Flavor::Scheme || is_builtin(logical.scheme()) {
        return logical.clone();
    }
    let mut path = String::new();
    if let Some(authority) = logical.host_str() {
        path.push('/');
        path.push_str(authority);
    }
    path.push_str(logical.path());
    let mut url = Url::parse(&format!("http://{}.localhost/", logical.scheme()))
        .unwrap_or_else(|_| logical.clone());
    url.set_path(&path);
    url.set_query(logical.query());
    url.set_fragment(logical.fragment());
    url
}

pub fn logical_url_for(flavor: Flavor, url: &Url) -> Option<Url> {
    match flavor {
        Flavor::Scheme => (!is_builtin(url.scheme())).then(|| url.clone()),
        Flavor::LocalhostHttp => {
            if !matches!(url.scheme(), "http" | "https") {
                return None;
            }
            let scheme = url.host_str()?.strip_suffix(".localhost")?;
            if scheme.is_empty() || is_builtin(scheme) {
                return None;
            }
            let rest = url.path().strip_prefix('/')?;
            let (authority, path) = match rest.split_once('/') {
                Some((authority, path)) => (authority, format!("/{path}")),
                None => (rest, "/".to_owned()),
            };
            let mut out = Url::parse(&format!("{scheme}://{authority}/")).ok()?;
            out.set_path(&path);
            out.set_query(url.query());
            out.set_fragment(url.fragment());
            Some(out)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(text: &str) -> Url {
        Url::parse(text).unwrap()
    }

    #[test]
    fn a_logical_url_becomes_an_http_localhost_url_on_windows_and_stays_on_macos() {
        let logical = url("html-asset://cap123/doc/index.html?x=1#top");
        assert_eq!(platform_url_for(Flavor::Scheme, &logical), logical);
        assert_eq!(
            platform_url_for(Flavor::LocalhostHttp, &logical).as_str(),
            "http://html-asset.localhost/cap123/doc/index.html?x=1#top"
        );
        let print = url("drawio-app://editor/__ieum/print/0123");
        assert_eq!(
            platform_url_for(Flavor::LocalhostHttp, &print).as_str(),
            "http://drawio-app.localhost/editor/__ieum/print/0123"
        );
    }

    #[test]
    fn the_two_shapes_translate_back_and_forth_exactly() {
        for logical in [
            "html-asset://cap123/_document",
            "md-asset://local/doc/image.png?v=2",
            "seuoffice-slide-media://deck-7/slide/1.png",
            "drawio-app://editor/index.html",
            "html-asset://cap123",
        ] {
            let logical = url(logical);
            let platform = platform_url_for(Flavor::LocalhostHttp, &logical);
            let back = logical_url_for(Flavor::LocalhostHttp, &platform).unwrap();
            assert_eq!(back.scheme(), logical.scheme());
            assert_eq!(back.host_str(), logical.host_str());
            assert_eq!(back.query(), logical.query());
            assert_eq!(
                back.path().trim_end_matches('/'),
                logical.path().trim_end_matches('/')
            );
        }
    }

    /// The same table as `packages/electron-utils/tests/custom-scheme-url.test.ts`: the page-side
    /// helper and the bridge must name every URL as the shell does.
    #[test]
    fn the_shape_matches_the_page_side_helper() {
        for (logical, platform) in [
            (
                "epub-asset://tok/OEBPS/a%20b.css?x=1#y",
                "http://epub-asset.localhost/tok/OEBPS/a%20b.css?x=1#y",
            ),
            ("epub-asset://tok/", "http://epub-asset.localhost/tok/"),
            (
                "html-preview://view-3/",
                "http://html-preview.localhost/view-3/",
            ),
            (
                "seuoffice-docx-media://handoff/abc",
                "http://seuoffice-docx-media.localhost/handoff/abc",
            ),
            (
                "seuoffice-slide-media://archive/id-1?mime=image%2Fpng",
                "http://seuoffice-slide-media.localhost/archive/id-1?mime=image%2Fpng",
            ),
            (
                "drawio-app://editor/index.html?embed=1",
                "http://drawio-app.localhost/editor/index.html?embed=1",
            ),
        ] {
            let logical = url(logical);
            let platform_url = platform_url_for(Flavor::LocalhostHttp, &logical);
            assert_eq!(platform_url.as_str(), platform);
            assert_eq!(
                logical_url_for(Flavor::LocalhostHttp, &platform_url).unwrap(),
                logical
            );
        }
    }

    #[test]
    fn urls_the_webview_resolves_itself_have_no_logical_shape() {
        assert!(logical_url_for(Flavor::LocalhostHttp, &url("https://example.org/a")).is_none());
        assert!(logical_url_for(Flavor::LocalhostHttp, &url("http://localhost/a")).is_none());
        assert!(logical_url_for(Flavor::LocalhostHttp, &url("http://tauri.localhost/a")).is_none());
        assert!(logical_url_for(Flavor::Scheme, &url("https://example.org/a")).is_none());
        assert!(logical_url_for(Flavor::Scheme, &url("about:blank")).is_none());
        let web = url("https://example.org/a");
        assert_eq!(platform_url_for(Flavor::LocalhostHttp, &web), web);
        assert_eq!(
            platform_url_for(Flavor::LocalhostHttp, &url("about:blank")).as_str(),
            "about:blank"
        );
    }

    #[test]
    fn text_helpers_leave_what_is_not_a_url_alone() {
        assert_eq!(platform_url_text("not a url"), "not a url");
        assert_eq!(handler_logical_url_text("not a url"), "not a url");
    }

    #[test]
    fn a_webview2_handler_address_keeps_its_authority_in_the_path() {
        let handed = |text: &str| handler_logical_url_for(Flavor::LocalhostHttp, url(text));
        // what wry hands the handler for http://drawio-app.localhost/editor/index.html?ui=min
        assert_eq!(
            handed("drawio-app://localhost/editor/index.html?ui=min").as_str(),
            "drawio-app://editor/index.html?ui=min"
        );
        assert_eq!(
            handed("epub-asset://localhost/0123456789abcdef/OEBPS/ch1.xhtml").as_str(),
            "epub-asset://0123456789abcdef/OEBPS/ch1.xhtml"
        );
        // the page's own shape and the logical one are read as before
        assert_eq!(
            handed("http://drawio-app.localhost/editor/index.html").as_str(),
            "drawio-app://editor/index.html"
        );
        assert_eq!(
            handed("drawio-app://editor/index.html").as_str(),
            "drawio-app://editor/index.html"
        );
        // nothing after the placeholder host names no authority
        assert_eq!(
            handed("drawio-app://localhost/").as_str(),
            "drawio-app://localhost/"
        );
        // WKWebView hands the logical address, where `localhost` is an authority like any other
        assert_eq!(
            handler_logical_url_for(Flavor::Scheme, url("drawio-app://localhost/editor/a.js"))
                .as_str(),
            "drawio-app://localhost/editor/a.js"
        );
    }
}
