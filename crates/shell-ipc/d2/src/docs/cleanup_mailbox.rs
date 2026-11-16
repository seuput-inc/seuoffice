//! The Docs actor's use of the shared generated-output mailbox.
use super::Docs;
use crate::docs::hosts::GeneratedActorWake;
use shell_contract::runtime::Services;

pub(super) use crate::generated_mailbox::{CleanupMailbox, Finished, begin, callback, wake};

impl crate::generated_mailbox::Actor for Docs {
    fn services(&self) -> &Services {
        &self.services
    }
    fn mailbox(&self) -> &CleanupMailbox {
        &self.cleanup_mailbox
    }
    fn register_cleanup(
        &self,
        wake: GeneratedActorWake,
    ) -> Result<Box<dyn shell_contract::hosts::GeneratedCleanupRegistration>, String> {
        self.shell.register_generated_actor_cleanup(wake)
    }
}
