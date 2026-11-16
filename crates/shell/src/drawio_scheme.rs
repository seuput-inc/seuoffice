pub(crate) mod print;

use std::path::PathBuf;
use std::sync::Arc;

use main_core::diagram::drawio_protocol::{self, DrawioTarget};

use crate::native::SchemeRequestContext;

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
        let Ok(bytes) = crate::host::drawio_assets::read(&root, &relative, u64::MAX) else {
            return empty(404);
        };
        let csp = drawio_protocol::drawio_csp(
            &self
                .ancestors
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
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
    // The platform shape (`http://drawio-app.localhost/editor/...` on Windows) is translated
    // back: the resolver and the print registry speak `drawio-app://editor/...`.
    let uri = crate::native::url::logical_url_text(&request.uri().to_string());
    tauri::async_runtime::spawn_blocking(move || match assets.resolve(&method, &uri) {
        DrawioResponse::Asset {
            status,
            headers,
            body,
        } => {
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
    fn injected_index_and_print_route_remain_distinct() {
        let base = std::env::temp_dir()
            .canonicalize()
            .expect("temporary directory")
            .join(format!("drawio-scheme-{}", jscompat::random_uuid()));
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
