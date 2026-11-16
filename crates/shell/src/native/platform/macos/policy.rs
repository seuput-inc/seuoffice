//! `PolicyOps` for WKWebView, which has no public per-request interception for http(s).
//!
//! What the policy cannot enforce request by request is enforced by what it declares it
//! refuses as a class ([`Blocks`], read, never probed): a `WKContentRuleList` blocks every
//! http(s) and ws(s) request when the policy declares `web`, and every `file:` request when
//! it declares `files`. A policy that declares no web block gets no blanket rule, so its
//! allow function still applies to navigations (the `on_navigation` hook) but not to
//! subresources. Loopback stays open when the policy declares it, for dev servers. Custom
//! schemes are never touched: the shell's scheme handlers authorize those themselves.
//!
//! Page JavaScript is switched per navigation by the chained delegate (`observer.rs`), which
//! also holds every navigation decision until the rules are installed. The browser learns the
//! policy is enforced (`observe_policy_installed`) just before that gate opens, so the
//! navigation it held for the policy replays under the rules.

use std::ptr::NonNull;

use block2::RcBlock;
use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2_foundation::{NSError, NSString};
use objc2_web_kit::{WKContentRuleList, WKContentRuleListStore, WKUserContentController};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tauri::webview::PlatformWebview;

use super::Platform;
use super::observer::gate_for;
use crate::native::browser::BrowserIdentity;
use crate::native::platform::PolicyOps;
use crate::native::policy::{Blocks, RequestPolicy};

/// The content rule list as WebKit's JSON, or `None` when nothing needs blocking.
/// (`url-filter` has no alternation, so each scheme family is a rule of its own.)
fn rules(blocks: Blocks) -> Option<String> {
    let Blocks {
        web,
        loopback_open,
        files,
    } = blocks;
    let block = |filter: &str, spare_loopback: bool| {
        let mut trigger = json!({ "url-filter": filter });
        if spare_loopback {
            trigger["unless-domain"] = json!(["localhost", "127.0.0.1"]);
        }
        json!({ "trigger": trigger, "action": { "type": "block" } })
    };
    let mut rules = Vec::new();
    if web {
        rules.push(block("^https?://", loopback_open));
        rules.push(block("^wss?://", loopback_open));
    }
    if files {
        rules.push(block("^file://", false));
    }
    (!rules.is_empty()).then(|| Value::Array(rules).to_string())
}

/// The store keeps compiled lists by identifier; the same rules always map to the same one.
fn identifier_of(rules: &str) -> String {
    let digest = Sha256::digest(rules.as_bytes());
    let hex: String = digest
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("seuoffice-policy-{hex}")
}

/// Compile and install the rules, then release the navigations held for them. Runs inside
/// `with_webview`, on the UI thread; WebKit answers the compile on the main queue.
fn install(
    platform: &PlatformWebview,
    browser: &BrowserIdentity,
    blocks: Blocks,
) -> Result<(), String> {
    let mtm = MainThreadMarker::new()
        .ok_or_else(|| "The request policy must be applied on the UI thread.".to_owned())?;
    let gate = gate_for(platform.inner())
        .ok_or_else(|| "The navigation observer is not attached.".to_owned())?;
    let Some(rules) = rules(blocks) else {
        browser.observe_policy_installed();
        gate.open();
        return Ok(());
    };
    let controller = NonNull::new(platform.controller().cast::<WKUserContentController>())
        .ok_or_else(|| "The web view has no content controller.".to_owned());
    // SAFETY: the pointer names the live content controller of this job; it is retained for
    // the asynchronous completion.
    let controller = controller.and_then(|controller| {
        unsafe { Retained::retain(controller.as_ptr()) }
            .ok_or_else(|| "The content controller could not be retained.".to_owned())
    });
    // SAFETY: the default store is a plain getter on the UI thread.
    let store = unsafe { WKContentRuleListStore::defaultStore(mtm) }
        .ok_or_else(|| "WebKit has no content rule list store.".to_owned());
    let (controller, store) = match (controller, store) {
        (Ok(controller), Ok(store)) => (controller, store),
        (Err(error), _) | (_, Err(error)) => {
            gate.fail();
            return Err(error);
        }
    };
    let browser = browser.clone();
    let block: RcBlock<dyn Fn(*mut WKContentRuleList, *mut NSError)> =
        RcBlock::new(move |list: *mut WKContentRuleList, error: *mut NSError| {
            // SAFETY: WebKit passes null or live objects that outlive this handler.
            match (unsafe { list.as_ref() }, unsafe { error.as_ref() }) {
                (Some(list), None) => {
                    // SAFETY: the controller is the web view's own, used on the UI thread.
                    unsafe { controller.addContentRuleList(list) };
                    browser.observe_policy_installed();
                    gate.open();
                }
                (_, error) => {
                    let reason = error
                        .map(|error| error.localizedDescription().to_string())
                        .unwrap_or_else(|| "no rule list".to_owned());
                    eprintln!("shell: the request policy rules were not installed: {reason}");
                    gate.fail();
                    browser.observe_policy_failed(&format!(
                        "The request policy rules were not installed: {reason}"
                    ));
                }
            }
        });
    // SAFETY: the block matches the completion handler type.
    unsafe {
        store.compileContentRuleListForIdentifier_encodedContentRuleList_completionHandler(
            Some(&*NSString::from_str(&identifier_of(&rules))),
            Some(&*NSString::from_str(&rules)),
            Some(&*block),
        );
    }
    Ok(())
}

impl PolicyOps for Platform {
    fn apply_policy(
        platform: &PlatformWebview,
        browser: &BrowserIdentity,
        policy: &RequestPolicy,
    ) -> Result<(), String> {
        install(platform, browser, policy.blocks())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_policy_that_declares_everything_blocks_the_web_and_files() {
        let everything = Blocks {
            web: true,
            loopback_open: false,
            files: true,
        };
        let rules: Value = serde_json::from_str(&rules(everything).unwrap()).unwrap();
        let filters: Vec<&str> = rules
            .as_array()
            .unwrap()
            .iter()
            .map(|rule| rule["trigger"]["url-filter"].as_str().unwrap())
            .collect();
        assert_eq!(filters, ["^https?://", "^wss?://", "^file://"]);
        assert!(rules[0]["trigger"].get("unless-domain").is_none());
    }

    #[test]
    fn a_policy_that_declares_files_only_blocks_only_files() {
        let only_files = Blocks {
            files: true,
            ..Blocks::NOTHING
        };
        let text = rules(only_files).unwrap();
        assert!(text.contains("^file://"));
        assert!(!text.contains("https"));
        assert_eq!(rules(Blocks::NOTHING), None);
    }

    #[test]
    fn loopback_stays_open_when_the_policy_declares_it() {
        let blocks = Blocks {
            web: true,
            loopback_open: true,
            files: true,
        };
        let rules: Value = serde_json::from_str(&rules(blocks).unwrap()).unwrap();
        assert_eq!(
            rules[0]["trigger"]["unless-domain"],
            json!(["localhost", "127.0.0.1"])
        );
        assert!(rules[2]["trigger"].get("unless-domain").is_none());
    }

    #[test]
    fn the_same_rules_share_one_identifier() {
        assert_eq!(identifier_of("[]"), identifier_of("[]"));
        assert_ne!(identifier_of("[]"), identifier_of("[{}]"));
    }
}
