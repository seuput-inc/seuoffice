pub(crate) mod print;

use std::path::PathBuf;
use std::sync::Arc;

use main_core::diagram::drawio_protocol::{self, DrawioTarget};

use crate::native::SchemeRequestContext;
use crate::native::url::{Flavor, platform_url_for};

pub(crate) struct DrawioAssets {
    root: PathBuf,
    bridge: PathBuf,
    ancestors: Vec<String>,
}

pub(crate) enum DrawioResponse {
    Asset {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    },
    Print {
        token: String,
    },
}

impl DrawioAssets {
    pub(crate) fn new(
        resources: Option<PathBuf>,
        dev_origins: Vec<String>,
    ) -> Result<Self, String> {
        let module = match resources
            .map(|path| path.join("modules/diagram"))
            .filter(|path| path.join("drawio").is_dir())
        {
            Some(path) => path,
            None => std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .and_then(std::path::Path::parent)
                .ok_or("The diagram module root is unavailable")?
                .join("apps/diagram/out"),
        };
        let mut ancestors = vec![crate::app_scheme::native_origin("diagram")];
        ancestors.extend(dev_origins);
        Ok(Self {
            root: module.join("drawio"),
            bridge: module.join("bridge/bridge.js"),
            ancestors,
        })
    }

    /// Print tokens require a separately captured print-view capability and are
    /// returned to the caller without consuming any document.
    pub(crate) fn resolve(&self, method: &str, uri: &str) -> DrawioResponse {
        if method != "GET" && method != "HEAD" {
            return empty(405);
        }
        let Some(root) = self.root.to_str() else {
            return empty(404);
        };
        let target = drawio_protocol::resolve_drawio_request(root, uri);
        let (root, relative, index) = match target {
            DrawioTarget::Refused { status } => return empty(status),
            DrawioTarget::Print { token } => return DrawioResponse::Print { token },
            DrawioTarget::Bridge => {
                let Some(parent) = self.bridge.parent() else {
                    return empty(404);
                };
                (parent.to_path_buf(), PathBuf::from("bridge.js"), false)
            }
            DrawioTarget::File { relative, .. } => {
                let index = relative == "index.html";
                (self.root.clone(), PathBuf::from(relative), index)
            }
        };
        let bytes = match crate::host::drawio_assets::read(&root, &relative, u64::MAX) {
            Ok(bytes) => bytes,
            Err(error) => {
                // a file draw.io asks for and does not ship is no news; its page, or a refusal
                // of the reader itself, is why the editor does not load
                if index || error.kind() != std::io::ErrorKind::NotFound {
                    crate::trace::trace!(
                        "scheme",
                        "draw.io: {} under {} could not be read: {error}",
                        relative.display(),
                        root.display()
                    );
                }
                return empty(404);
            }
        };
        let csp = csp_in_shape(
            drawio_protocol::drawio_csp(
                &self
                    .ancestors
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
            ),
            Flavor::CURRENT,
        );
        let content_type = drawio_protocol::content_type_of(relative.to_str().unwrap_or(""));
        let headers = drawio_protocol::response_headers(content_type, &csp)
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value))
            .collect();
        let body = if index {
            let html = String::from_utf8_lossy(&bytes);
            let Ok(html) = drawio_protocol::inject_bridge(&html) else {
                return empty(404);
            };
            let html = bridge_in_shape(html, Flavor::CURRENT);
            if method == "HEAD" {
                Vec::new()
            } else {
                html.into_bytes()
            }
        } else if method == "HEAD" {
            Vec::new()
        } else {
            bytes
        };
        DrawioResponse::Asset {
            status: 200,
            headers,
            body,
        }
    }
}

/// The injected `<script src="/__ieum/bridge.js">` names the bridge by an absolute path. On
/// WebView2 the page is `http://drawio-app.localhost/editor/index.html` and the host of the scheme
/// is its first path segment (`native::url`), so that path would miss `/editor`. There the tag
/// names the bridge by its whole address instead.
fn bridge_in_shape(html: String, flavor: Flavor) -> String {
    let logical = format!(
        "{}://{}{}",
        drawio_protocol::DRAWIO_SCHEME,
        drawio_protocol::DRAWIO_HOST,
        drawio_protocol::BRIDGE_SCRIPT_PATH
    );
    let Ok(logical) = tauri::Url::parse(&logical) else {
        return html;
    };
    let address = platform_url_for(flavor, &logical);
    if address == logical {
        return html;
    }
    html.replacen(
        &format!("<script src=\"{}\">", drawio_protocol::BRIDGE_SCRIPT_PATH),
        &format!("<script src=\"{address}\">"),
        1,
    )
}

/// A policy of `drawio_protocol` names the scheme as `drawio-app:`, which WebView2 does not match
/// against `http://drawio-app.localhost`; there both are named.
fn csp_in_shape(csp: String, flavor: Flavor) -> String {
    if flavor == Flavor::Scheme {
        return csp;
    }
    let own = format!("{}:", drawio_protocol::DRAWIO_SCHEME);
    csp.replace(
        &own,
        &format!("{own} http://{}.localhost", drawio_protocol::DRAWIO_SCHEME),
    )
}

fn empty(status: u16) -> DrawioResponse {
    DrawioResponse::Asset {
        status,
        headers: Vec::new(),
        body: Vec::new(),
    }
}

pub(crate) fn handle(
    assets: Arc<DrawioAssets>,
    origin: SchemeRequestContext,
    request: tauri::http::Request<Vec<u8>>,
    responder: tauri::UriSchemeResponder,
) {
    let method = request.method().as_str().to_owned();
    // What the handler is handed is translated back (on Windows wry hands
    // `drawio-app://localhost/editor/...` for the page's `http://drawio-app.localhost/editor/...`,
    // which the resolver refused as a host that is not the editor's, so the frame never loaded):
    // the resolver and the print registry speak `drawio-app://editor/...`.
    let uri = crate::native::url::handler_logical_url_text(&request.uri().to_string());
    tauri::async_runtime::spawn_blocking(move || match assets.resolve(&method, &uri) {
        DrawioResponse::Asset {
            status,
            headers,
            body,
        } => {
            // the editor's page and every refusal; its few hundred files are left out
            if status >= 400 || uri.contains("index.html") {
                crate::trace::trace!(
                    "scheme",
                    "draw.io {method} {} -> {status} ({} bytes)",
                    uri.chars().take(120).collect::<String>(),
                    body.len()
                );
            }
            let mut builder = tauri::http::Response::builder().status(status);
            for (name, value) in headers {
                builder = builder.header(name, value);
            }
            match builder.body(body) {
                Ok(response) => responder.respond(response),
                Err(_) => {
                    let mut response = tauri::http::Response::new(Vec::new());
                    *response.status_mut() = tauri::http::StatusCode::INTERNAL_SERVER_ERROR;
                    responder.respond(response);
                }
            }
        }
        DrawioResponse::Print { token } => print::serve(&token, &origin, responder),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bridge_is_named_by_its_address_where_webview2_serves_the_scheme() {
        let page =
            "<html><head><script src=\"/__ieum/bridge.js\"></script></head></html>".to_owned();
        assert_eq!(bridge_in_shape(page.clone(), Flavor::Scheme), page);
        assert_eq!(
            bridge_in_shape(page, Flavor::LocalhostHttp),
            "<html><head><script src=\"http://drawio-app.localhost/editor/__ieum/bridge.js\"></script></head></html>"
        );
    }

    #[test]
    fn the_policy_names_both_shapes_of_the_scheme_on_windows_only() {
        let csp = drawio_protocol::drawio_csp(&["http://genoffice-app-diagram.localhost"]);
        assert_eq!(csp_in_shape(csp.clone(), Flavor::Scheme), csp);
        let windows = csp_in_shape(csp, Flavor::LocalhostHttp);
        assert!(windows.contains("script-src drawio-app: http://drawio-app.localhost;"));
        assert!(windows.contains("img-src drawio-app: http://drawio-app.localhost data: blob:"));
        assert!(windows.contains("frame-ancestors http://genoffice-app-diagram.localhost"));
    }

    #[test]
    fn injected_index_and_print_route_remain_distinct() {
        let temp = std::env::temp_dir()
            .canonicalize()
            .expect("temporary directory");
        // The asset reader accepts drive and UNC roots only; canonicalization gives the
        // `\\?\` form on Windows.
        #[cfg(windows)]
        let temp = std::path::PathBuf::from(
            temp.to_str()
                .expect("UTF-8 temporary directory")
                .trim_start_matches(r"\\?\"),
        );
        let base = temp.join(format!("drawio-scheme-{}", jscompat::random_uuid()));
        let root = base.join("drawio");
        std::fs::create_dir_all(&root).expect("directory");
        std::fs::write(root.join("index.html"), "<html><head></head></html>").expect("index");
        let assets = DrawioAssets {
            root,
            bridge: base.join("bridge.js"),
            ancestors: vec!["genoffice-app:".into()],
        };
        match assets.resolve("GET", "drawio-app://editor/index.html") {
            DrawioResponse::Asset { status, body, .. } => {
                assert_eq!(status, 200);
                assert!(
                    String::from_utf8(body)
                        .expect("UTF-8")
                        .contains("/__ieum/bridge.js")
                );
            }
            DrawioResponse::Print { .. } => panic!("index became a print request"),
        }
        assert!(matches!(
            assets.resolve(
                "GET",
                "drawio-app://editor/__ieum/print/0123456789abcdef0123456789abcdef"
            ),
            DrawioResponse::Print { .. }
        ));
        assert!(matches!(
            assets.resolve("GET", "drawio-app://other/index.html"),
            DrawioResponse::Asset { status: 404, .. }
        ));
        std::fs::remove_dir_all(base).expect("cleanup");
    }
}
