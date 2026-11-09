//! Guild ingress is routed only to bot-created private, non-invitable threads.
use super::{gateway::Rejected, rest::Rest};
use crate::channels::dispatch::Dispatcher;
use anyhow::{anyhow, Result};
use reqwest::Method;
use serde_json::{json, Value};
use std::sync::Arc;

fn validate(channel: &Value, guild: &str, parent: &str) -> Result<()> {
    if channel["guild_id"] != guild || channel["parent_id"] != parent
        || channel["type"] != 12 || channel["thread_metadata"]["invitable"] != false {
        return Err(anyhow!(Rejected("Private conversation unavailable.")));
    }
    Ok(())
}

pub async fn destination(engine: &Arc<Dispatcher>, rest: &Arc<Rest>, guild: &str, user: &str, source: &str) -> Result<String> {
    let _guard=engine.guild_routes.lock().await;
    let state=&engine.server.channels;
    let person=state.person("discord",user)?.ok_or_else(||anyhow!(Rejected("Pair in the bot DM first.")))?;
    if let Some(route)=state.thread_owner(source)? {
        if route["guild"]!=guild || route["person"]!=person || route["user"]!=user || route["status"]!="active" {
            return Err(anyhow!(Rejected("Private conversation unavailable.")));
        }
        let channel=rest.request(Method::GET,&format!("/channels/{source}"),json!({})).await?;
        validate(&channel,guild,route["parent"].as_str().unwrap())?;
        return Ok(source.into());
    }
    let parent=rest.request(Method::GET,&format!("/channels/{source}"),json!({})).await?;
    if parent["type"]!=0 || parent["guild_id"]!=guild {
        return Err(anyhow!(Rejected("Use a guild text channel or your own Kurisu private thread.")));
    }
    let route=state.thread_for(guild,source,&person)?;
    let thread=match route {
        Some(route)=> {
            if route["user"]!=user { return Err(anyhow!(Rejected("Private conversation unavailable."))); }
            route["thread"].as_str().ok_or_else(||anyhow!(Rejected("Thread creation has an uncertain receipt; local owner must reconcile it.")))?.to_string()
        }
        None=> {
            state.reserve_thread(guild,source,&person,user)?;
            let created=rest.request(Method::POST,&format!("/channels/{source}/threads"),json!({"name":format!("Kurisu {person}"),"type":12,"invitable":false,"auto_archive_duration":1440})).await?;
            let id=created["id"].as_str().ok_or_else(||anyhow!(Rejected("Thread creation has an uncertain receipt; local owner must reconcile it.")))?;
            validate(&created,guild,source)?;
            state.thread_created(guild,source,&person,id)?;
            id.to_string()
        }
    };
    let channel=rest.request(Method::GET,&format!("/channels/{thread}"),json!({})).await?;
    validate(&channel,guild,source)?;
    // PUT is idempotent. Persist the receipt before inviting, so restart can retry only the invite.
    rest.request(Method::PUT,&format!("/channels/{thread}/thread-members/{user}"),json!({})).await?;
    state.thread_active(guild,source,&person)?;
    Ok(thread)
}
