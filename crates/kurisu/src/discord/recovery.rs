//! Recovery runs outside the WebSocket read loop; HTTP stalls cannot starve heartbeats.
use super::{
    gateway::{dispatch_saved, Rejected},
    rest::Rest,
};
use crate::channels::dispatch::Dispatcher;
use anyhow::{anyhow, bail, Result};
use reqwest::Method;
use serde_json::json;
use std::{sync::Arc, time::Duration};

pub async fn step(
    engine: &Arc<Dispatcher>,
    rest: &Arc<Rest>,
    guild: &str,
    account: &str,
) -> Result<()> {
    let state = &engine.server.channels;
    for job in state.backfills(account)? {
        let destination = job["destination"].as_str().unwrap();
        if job["guild"].as_str().is_some_and(|g| g != guild) {
            bail!("Recovery guild does not match configuration");
        }
        let path = match job["before"].as_str() {
            Some(before) => format!("/channels/{destination}/messages?limit=100&before={before}"),
            None => format!("/channels/{destination}/messages?limit=100"),
        };
        // Newest-to-oldest pages are staged durably before releasing a channel in oldest-first order.
        let response = rest.request(Method::GET, &path, json!({})).await?;
        let messages = response
            .as_array()
            .ok_or_else(|| anyhow!("Invalid Discord history response"))?;
        let mut minimum = None;
        for message in messages {
            let id = message["id"]
                .as_str()
                .ok_or_else(|| anyhow!("Missing Discord history ID"))?
                .parse::<u64>()?;
            minimum = Some(minimum.map_or(id, |old: u64| old.min(id)));
        }
        if let (Some(minimum), Some(before)) = (minimum, job["before"].as_str()) {
            if minimum >= before.parse::<u64>()? {
                bail!("Discord history pagination did not advance");
            }
        }
        let done = messages.len() < 100
            || minimum
                .is_some_and(|id| id <= job["lower"].as_str().unwrap().parse::<u64>().unwrap());
        let before = minimum.map(|n| n.to_string());
        state.backfill_page(account, &job, messages, before.as_deref(), done)?;
    }
    for event in state.gateway_pending(account)? {
        let key = event["key"].as_str().unwrap();
        let result = dispatch_saved(
            engine,
            rest,
            guild,
            event["event"].as_str().unwrap(),
            event["data"].clone(),
        )
        .await;
        match result {
            Ok(()) => state.gateway_done(account, key, "done", None)?,
            Err(e) if e.downcast_ref::<Rejected>().is_some() || e.downcast_ref::<crate::channels::state::Denied>().is_some() => {
                state.gateway_done(account, key, "rejected", Some(&rest.safe(&e.to_string())))?;
                eprintln!("ERROR Discord input rejected: {}",rest.safe(&e.to_string()));
            }
            Err(e) => {
                state.gateway_done(account, key, "pending", Some(&rest.safe(&e.to_string())))?;
                eprintln!(
                    "ERROR Discord recovery event retained for retry: {}",
                    rest.safe(&e.to_string())
                );
            }
        }
    }
    Ok(())
}
pub async fn run(engine: &Arc<Dispatcher>, rest: &Arc<Rest>, guild: &str, account: &str) {
    loop {
        if let Err(e) = step(engine, rest, guild, account).await {
            eprintln!(
                "ERROR Discord recovery retained for retry: {}",
                rest.safe(&e.to_string())
            );
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}
