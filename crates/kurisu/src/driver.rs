//! ACP-shaped driver interface. Every harness looks like an ACP agent to the
//! rest of Kurisu: new/load session, prompt with streamed `SessionUpdate`s,
//! cancel, close. Types are the official `agent-client-protocol` v1 schema.

use std::path::PathBuf;

use agent_client_protocol::v1::schema::{
    ContentBlock, ContentChunk, LoadSessionRequest, NewSessionRequest, PromptRequest, PromptResponse, SessionId,
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

/// The reply to one prompt: the assistant text after the agent echoed that
/// prompt back, up to the next user message.
///
/// ACP's `session/load` replays the conversation as `session/update` frames.
/// agy sends that replay before it answers the load request, but a large
/// history can still be in flight when the load response wakes the next
/// prompt, so the replay lands in that prompt's stream. Summing every
/// `AgentMessageChunk` then returns the previous turns' text as this turn's
/// reply. agy echoes the prompt as a user chunk before it starts working, so
/// the text after the last echo (and before the next user chunk) is the only
/// text that answers this message. Agents that do not echo keep the old
/// behaviour.
pub fn turn_reply(updates: &[SessionNotification], prompt: &str) -> String {
    let prompt = prompt.trim();
    let mut echo = String::new();
    let mut start = None;
    for (i, n) in updates.iter().enumerate() {
        let SessionUpdate::UserMessageChunk(c) = &n.update else { continue };
        let ContentBlock::Text(t) = &c.content else { continue };
        if echo.is_empty() || prompt.starts_with(echo.as_str()) {
            echo.push_str(&t.text);
        } else {
            echo.clone_from(&t.text);
        }
        if echo.trim() == prompt {
            start = Some(i + 1);
            echo.clear();
        }
    }
    let Some(start) = start else { return agent_text(updates) };
    let mut out = String::new();
    for n in &updates[start..] {
        match &n.update {
            SessionUpdate::UserMessageChunk(_) => break,
            SessionUpdate::AgentMessageChunk(c) => {
                if let ContentBlock::Text(t) = &c.content {
                    out.push_str(&t.text);
                }
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(update: SessionUpdate) -> SessionNotification {
        SessionNotification::new("s", update)
    }
    fn user(text: &str) -> SessionNotification {
        note(SessionUpdate::UserMessageChunk(ContentChunk::new(ContentBlock::Text(TextContent::new(text)))))
    }
    fn agent(text: &str) -> SessionNotification {
        note(SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(TextContent::new(text)))))
    }

    #[test]
    fn turn_reply_ignores_replayed_history() {
        // What agy sends for session/load: the whole conversation, in order.
        let replay = vec![user("ping"), agent("old reply"), user("old question"), agent("older reply")];
        let mut updates = replay.clone();
        updates.push(user("continue"));
        updates.push(agent("new "));
        updates.push(agent("reply"));
        assert_eq!(turn_reply(&updates, "continue"), "new reply");
        // A replay frame that trails past the echo must not join the reply either.
        updates.push(user("replayed question"));
        updates.push(agent("stale"));
        assert_eq!(turn_reply(&updates, "continue"), "new reply");
        // An agent that does not echo the prompt keeps the old behaviour.
        assert_eq!(turn_reply(&replay, "continue"), "old replyolder reply");
    }
}
