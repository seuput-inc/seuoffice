//! The hub as the relays of an agent session use it (`AgentRelayHub` of
//! `apps/shell/src/main/agent/agent-mcp-relay.ts`): the editor list of the
//! person's servers, a call through the hub's one connection per server and its
//! cancellation. Core thread only, like the port.

use jscompat::JsonValue;
use main_core::shell::mcp_client::hub::Reply;

use super::McpHubPort;

impl McpHubPort {
    /// `hub.editorList()`: the `McpServerView[]` as JSON
    pub fn relay_editor_list(&self) -> Vec<JsonValue> {
        self.hub.ipc.borrow().hub.editor_list()
    }

    /// `hub.call(caller, server, tool, input, callId)`: `done` runs once on the
    /// core thread with the answer shaped for the AI (`Ok`) or the message of
    /// what rejected
    pub fn relay_call(
        &self,
        caller: &str,
        server: &str,
        tool: &str,
        input: &JsonValue,
        call_id: &str,
        done: Box<dyn FnOnce(Result<Option<JsonValue>, String>)>,
    ) {
        let reply = self
            .hub
            .ipc
            .borrow_mut()
            .hub
            .call(caller, server, tool, input, call_id);
        match reply {
            Reply::Now(value) => {
                self.hub.pump();
                done(Ok(Some(value)));
            }
            Reply::Later(ticket) => {
                self.hub.agent_calls.borrow_mut().insert(
                    ticket,
                    Box::new(move |value| done(value.map_err(|error| error.message))),
                );
                self.hub.pump();
            }
        }
    }

    /// `hub.cancel(caller, callId)`
    pub fn relay_cancel(&self, caller: &str, call_id: &str) {
        self.hub.ipc.borrow_mut().hub.cancel(caller, call_id);
        self.hub.pump();
    }
}
