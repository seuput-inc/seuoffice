//! The network under the mind map's link preview and picture fetch (apps/mindmap
//! `link-preview.ts`, `ai-image.ts`): the operating system's resolver and the
//! shell's HTTP transport, under main-core's SSRF guard.

use std::sync::Arc;

use main_core::packages::electron_utils::safe_remote_url::{Fetcher, Resolver};
use shell_contract::Hosts;
use shell_ipc_d1::mindmap::{LinkNet, LinkNetHost};

use crate::host::net::ai::fetch::{guard_fetcher, guard_resolver};

struct Net;

impl LinkNet for Net {
    fn resolver(&self) -> Box<dyn Resolver> {
        guard_resolver()
    }

    fn fetcher(&self) -> Box<dyn Fetcher> {
        guard_fetcher()
    }
}

pub(super) fn install(hosts: &mut Hosts) {
    let net: LinkNetHost = Arc::new(Net);
    hosts.insert(net);
}

#[cfg(test)]
mod tests {
    use super::*;
    use shell_contract::value::IpcValue;
    use shell_ipc_d1::mindmap::read_link_preview;

    fn refused(reason: &str) -> IpcValue {
        IpcValue::Object(vec![
            ("ok".to_owned(), IpcValue::Bool(false)),
            ("reason".to_owned(), IpcValue::String(reason.to_owned())),
        ])
    }

    fn temp() -> String {
        std::env::temp_dir().to_string_lossy().into_owned()
    }

    #[test]
    fn install_gives_the_mind_map_its_network() {
        let mut hosts = Hosts::default();
        install(&mut hosts);
        assert!(hosts.need::<LinkNetHost>(shell_contract::Batch::D1).is_ok());
    }

    #[test]
    fn the_resolver_answers_an_address_literal() {
        let found = Net.resolver().lookup_all("127.0.0.1").unwrap();
        assert!(found.iter().any(|a| a.address == "127.0.0.1"));
    }

    #[test]
    fn the_guard_refuses_an_internal_address_before_any_request() {
        let preview = read_link_preview(&Net, &temp(), Some("http://127.0.0.1:9/"));
        assert_eq!(preview, refused("unreachable"));
    }

    #[test]
    fn a_link_that_is_not_a_web_address_is_not_fetched() {
        let preview = read_link_preview(&Net, &temp(), Some("file:///etc/hosts"));
        assert_eq!(preview, refused("not-web"));
    }
}
