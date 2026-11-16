use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use shell_contract::runtime::RendererGeneration;
use tauri::UriSchemeResponder;
use tauri::http;

use crate::native::{BrowserState, NavigationIdentity, SchemeRequestContext};

pub(crate) type Admission =
    Arc<dyn Fn(&mut dyn FnMut() -> Result<(), String>) -> Result<(), String> + Send + Sync>;

struct Entry {
    browser: BrowserState,
    body: Vec<u8>,
    authority: Admission,
    lifetime: RendererGeneration,
    claimed: bool,
    navigation: Option<NavigationIdentity>,
}

static PRINTS: OnceLock<Mutex<HashMap<String, Entry>>> = OnceLock::new();
fn prints() -> &'static Mutex<HashMap<String, Entry>> {
    PRINTS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) struct PrintGrant {
    token: String,
    lifetime: RendererGeneration,
}

impl PrintGrant {
    pub(crate) fn served(&self) -> Result<bool, String> {
        let entries = prints()
            .lock()
            .map_err(|_| "The diagram print registry is unavailable")?;
        let entry = entries
            .get(&self.token)
            .filter(|entry| entry.lifetime.same_generation(&self.lifetime))
            .ok_or("The diagram print grant was released")?;
        if !entry.claimed {
            return Ok(false);
        }
        if entry.navigation.is_none()
            || entry.navigation.as_ref() != entry.browser.navigation().as_ref()
        {
            return Err("The diagram print navigation changed".into());
        }
        Ok(entry.browser.document().is_some())
    }
    pub(crate) fn matches_target(
        &self,
        target: &crate::host::contents::OwnedPrintTarget,
    ) -> Result<bool, String> {
        let entries = prints()
            .lock()
            .map_err(|_| "The diagram print registry is unavailable")?;
        Ok(entries.get(&self.token).is_some_and(|entry| {
            entry.lifetime.same_generation(&self.lifetime)
                && entry.claimed
                && entry.navigation.is_some()
                && entry.navigation.as_ref() == entry.browser.navigation().as_ref()
                && entry.browser.is_same_browser(&target.frame)
                && entry.browser.document().as_ref() == Some(&target.document)
        }))
    }
    /// The URL to navigate the print browser to, in the shape the platform serves
    /// (`crate::native::url`); the registry and the scheme handler speak the logical one.
    pub(crate) fn url(&self) -> String {
        crate::native::url::platform_url_text(&format!(
            "drawio-app://editor/__ieum/print/{}",
            self.token
        ))
    }
}

impl Drop for PrintGrant {
    fn drop(&mut self) {
        self.lifetime.revoke();
        if let Ok(mut entries) = prints().lock()
            && entries
                .get(&self.token)
                .is_some_and(|entry| entry.lifetime.same_generation(&self.lifetime))
        {
            entries.remove(&self.token);
        }
    }
}

pub(crate) fn reserve(
    browser: BrowserState,
    body: Vec<u8>,
    authority: Admission,
) -> Result<PrintGrant, String> {
    let mut random = [0u8; 16];
    getrandom::fill(&mut random).map_err(|error| error.to_string())?;
    let token: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
    let lifetime = RendererGeneration::new();
    let mut entry = Some(Entry {
        browser,
        body,
        authority: authority.clone(),
        lifetime: lifetime.clone(),
        claimed: false,
        navigation: None,
    });
    authority(&mut || {
        let mut entries = prints()
            .lock()
            .map_err(|_| "The diagram print registry is unavailable")?;
        if entries.contains_key(&token) {
            return Err("The diagram print token is already in use".into());
        }
        entries.insert(
            token.clone(),
            entry
                .take()
                .ok_or("The diagram print reservation was consumed")?,
        );
        Ok(())
    })?;
    Ok(PrintGrant { token, lifetime })
}

fn matches(entry: &Entry, origin: &SchemeRequestContext) -> bool {
    let (Some(browser), Some(navigation)) = (origin.browser_state(), origin.navigation()) else {
        return false;
    };
    entry.browser.is_same_browser(browser)
        && entry.browser.navigation().as_ref() == Some(navigation)
        && origin
            .document()
            .is_none_or(|document| entry.browser.document().as_ref() == Some(document))
        && entry.lifetime.is_active()
        && !entry.claimed
}

/// The root request must be the browser's single admitted navigation; no frame is consulted.
pub(crate) fn serve(token: &str, origin: &SchemeRequestContext, responder: UriSchemeResponder) {
    let captured = prints().lock().ok().and_then(|entries| {
        entries
            .get(token)
            .filter(|entry| matches(entry, origin))
            .map(|entry| {
                (
                    entry.body.clone(),
                    entry.authority.clone(),
                    entry.lifetime.clone(),
                )
            })
    });
    let Some((body, authority, lifetime)) = captured else {
        return denied(responder);
    };
    let csp = main_core::diagram::drawio_protocol::print_page_csp();
    let mut builder = http::Response::builder().status(200);
    for (name, value) in
        main_core::diagram::drawio_protocol::response_headers("text/html; charset=utf-8", &csp)
    {
        builder = builder.header(name, value);
    }
    let Ok(response) = builder.body(std::borrow::Cow::Owned(body)) else {
        return denied(responder);
    };
    let Ok(prepared) = origin.response_gate().prepare(response) else {
        return denied(responder);
    };
    let mut pending = Some((prepared, responder));
    let mut accepted = None;
    let _ = authority(&mut || {
        lifetime
            .with_active(|| {
                let mut entries = prints()
                    .lock()
                    .map_err(|_| "The diagram print registry is unavailable")?;
                let entry = entries
                    .get_mut(token)
                    .filter(|entry| {
                        entry.lifetime.same_generation(&lifetime) && matches(entry, origin)
                    })
                    .ok_or("The diagram print request was released")?;
                let (prepared, responder) = pending
                    .take()
                    .ok_or("The diagram print response was consumed")?;
                match prepared.accept(responder) {
                    Ok(response) => {
                        entry.navigation = origin.navigation().cloned();
                        entry.claimed = true;
                        accepted = Some(Ok(response));
                    }
                    Err(rejected) => accepted = Some(Err(rejected)),
                }
                Ok(())
            })
            .ok_or_else(|| "The diagram print grant was released".to_owned())?
    });
    match accepted {
        Some(Ok(answer)) => {
            let _ = answer.submit();
        }
        Some(Err(rejected)) => denied(rejected.into_responder()),
        None => {
            if let Some((_, responder)) = pending {
                denied(responder);
            }
        }
    }
}

fn denied(responder: UriSchemeResponder) {
    let mut response = http::Response::new(Vec::<u8>::new());
    *response.status_mut() = http::StatusCode::NOT_FOUND;
    responder.respond(response);
}
