//! ACP-shaped driver interface. Every harness looks like an ACP agent to the
//! rest of Kurisu: new/load session, prompt with streamed `SessionUpdate`s,
//! cancel, close. Types are the official `agent-client-protocol` v1 schema.

use std::path::PathBuf;

use agent_client_protocol::v1::schema::{
    ContentBlock, LoadSessionRequest, NewSessionRequest, PromptRequest, PromptResponse, SessionId,
    SessionNotification, SessionUpdate, TextContent,
};
use anyhow::Result;
use tokio::sync::mpsc;

pub type Updates = mpsc::Sender<SessionNotification>;

#[allow(async_fn_in_trait, dead_code)]
pub trait Driver {
    fn harness(&self) -> &'static str;
    async fn new_session(&self, cwd: PathBuf) -> Result<SessionId>;
    async fn load_session(&self, id: &SessionId, cwd: PathBuf) -> Result<()>;
    async fn prompt(&self, id: &SessionId, text: &str, updates: Updates) -> Result<PromptResponse>;
    async fn cancel(&self, id: &SessionId) -> Result<()>;
    async fn close(&self, id: &SessionId) -> Result<()>;
}

/// Claude: the standalone claude-cli-acp crate, used in-process.
pub struct ClaudeDriver(pub claude_cli_acp::ClaudeCli);

impl Driver for ClaudeDriver {
    fn harness(&self) -> &'static str {
        "claude"
    }
    async fn new_session(&self, cwd: PathBuf) -> Result<SessionId> {
        Ok(self.0.new_session(NewSessionRequest::new(cwd)).await?.session_id)
    }
    async fn load_session(&self, id: &SessionId, cwd: PathBuf) -> Result<()> {
        self.0.load_session(LoadSessionRequest::new(id.clone(), cwd)).await
    }
    async fn prompt(&self, id: &SessionId, text: &str, updates: Updates) -> Result<PromptResponse> {
        let req = PromptRequest::new(id.clone(), vec![ContentBlock::Text(TextContent::new(text))]);
        self.0.prompt(req, updates).await
    }
    async fn cancel(&self, id: &SessionId) -> Result<()> {
        self.0.cancel(id).await
    }
    async fn close(&self, id: &SessionId) -> Result<()> {
        self.0.close(id).await
    }
}

/// Collect the agent's text from a batch of updates.
pub fn agent_text(updates: &[SessionNotification]) -> String {
    updates
        .iter()
        .filter_map(|n| match &n.update {
            SessionUpdate::AgentMessageChunk(c) => match &c.content {
                ContentBlock::Text(t) => Some(t.text.as_str()),
                _ => None,
            },
            _ => None,
        })
        .collect()
}
