//! What the Telegram channel keeps for the `telegram_history` MCP tool.
//!
//! The Bot API has no "fetch past messages" call, so the only honest source is
//! what Kurisu itself saw. Every message the owner sends to the bot's DM is
//! written to the store the moment its update arrives (before any turn runs),
//! so a message that arrived while the agent was asleep, mid-turn or after a
//! failed reply is still there afterwards, flagged `answered: false` until an
//! agent reply is actually sent. Kurisu's own texts to the DM are kept too, so
//! the exchange reads as a chat.
//!
//! Only the owner's DM is recorded: group and channel content is not read by
//! sessions through this tool.

use std::sync::Arc;

use serde_json::{json, Value};

use crate::server::Server;

/// A short marker for an incoming attachment, without the file id.
pub fn attachment_marker(att: &Value) -> Value {
    let name = att["name"].as_str().unwrap_or("");
    let mime = att["mime"].as_str().unwrap_or("");
    let kind = if mime.starts_with("image/") {
        "photo"
    } else if super::bridge::is_audio_attachment(name, mime) {
        "voice"
    } else if mime.starts_with("video/") {
        "video"
    } else {
        "file"
    };
    json!({"kind": kind, "name": name, "mime": mime})
}

/// Store one message from the owner. Never fails the message itself: a store
/// write error is logged and the update is handled as before.
pub(crate) async fn inbound(server: &Arc<Server>, chat_id: &str, update_id: i64, message_id: Option<i64>,
                            text: &str, attachment: Option<&Value>, at: i64) {
    let marker = attachment.map(attachment_marker)
        .or_else(|| text.trim().is_empty().then(|| json!({"kind": "other", "name": "", "mime": ""})));
    let r = server.with_store(|st| st.tg_record_inbound(chat_id, message_id, update_id, text.trim(), marker.as_ref(), at)).await;
    if let Err(e) = r {
        super::log(format!("could not record update {update_id} in telegram history: {e:#}"));
    }
}

/// Store text Kurisu sent to the owner's DM (chunks of one reply are joined).
pub(crate) async fn agent(server: &Arc<Server>, chat_id: &str, update_id: Option<i64>, text: &str, at: i64) {
    if text.trim().is_empty() {
        return;
    }
    if let Err(e) = server.with_store(|st| st.tg_record_agent(chat_id, update_id, text, at)).await {
        super::log(format!("could not record an outgoing message in telegram history: {e:#}"));
    }
}

/// An agent reply went out for this inbound message.
pub(crate) async fn replied(server: &Arc<Server>, chat_id: &str, update_id: i64, at: i64) {
    if let Err(e) = server.with_store(|st| st.tg_mark_answered(chat_id, update_id, at)).await {
        super::log(format!("could not mark update {update_id} answered: {e:#}"));
    }
}

/// Fill in a voice message's transcript once speech-to-text produced it.
pub(crate) async fn transcript(server: &Arc<Server>, chat_id: &str, update_id: i64, text: &str) {
    if text.trim().is_empty() {
        return;
    }
    if let Err(e) = server.with_store(|st| st.tg_set_transcript(chat_id, update_id, text)).await {
        super::log(format!("could not store the transcript of update {update_id}: {e:#}"));
    }
}
