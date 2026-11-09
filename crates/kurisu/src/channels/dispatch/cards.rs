//! Durable permission cards; decisions still belong to the existing server/harness.
use super::*;

impl Dispatcher {
    pub(crate) async fn sync_prompts(&self, to: &Address, session: &str) -> Result<()> {
        let _guard = self.ui.lock().await;
        let approvals = op(
            &self.server,
            json!({"op":"approvals","name":session,"all":true}),
        )
        .await?;
        for approval in approvals.as_array().into_iter().flatten() {
            let request = approval["id"]
                .as_str()
                .ok_or_else(|| anyhow!("Missing approval ID"))?;
            let key = format!("ui:{}:{session}:{request}", to.destination);
            let old = self.server.channels.relay(&key)?;
            let mut status = approval["status"]
                .as_str()
                .unwrap_or("cancelled")
                .to_string();
            if status == "pending"
                && approval["expires_at"]
                    .as_i64()
                    .is_some_and(|t| t <= super::super::now())
            {
                status = "expired".into();
            }
            if old.is_none() && status != "pending" {
                continue;
            }
            let mut card = if let Some(card) = old {
                card
            } else {
                let question = approval["kind"] == "question";
                let full = format!(
                    "{}\nSession: {}\nRequest: {request}\n{}",
                    approval["title"].as_str().unwrap_or("Permission requested"),
                    approval["session_name"].as_str().unwrap_or(session),
                    serde_json::to_string_pretty(&approval["input"])?
                );
                let text = if full.encode_utf16().count() > 1700 {
                    self.publish(to, &format!("{key}:details"), &full).await?;
                    format!(
                        "{}\nSession: {}\nRequest: {request}\nFull request is above or attached.",
                        if question {
                            "Question pending"
                        } else {
                            "Permission requested"
                        },
                        approval["session_name"].as_str().unwrap_or(session)
                    )
                } else {
                    full
                };
                let mut choices: Vec<(String, Value)> = Vec::new();
                if question {
                    let qs = approval["input"]["questions"].as_array();
                    if let Some(q) = qs
                        .filter(|qs| qs.len() == 1)
                        .and_then(|qs| qs.first())
                        .filter(|q| q["multiSelect"] != true)
                    {
                        if let Some(options) = q["options"]
                            .as_array()
                            .filter(|o| !o.is_empty() && o.len() <= 4)
                        {
                            for option in options {
                                if let Some(label) = option["label"].as_str() {
                                    choices.push((
                                        label.into(),
                                        json!({"kind":"answer","answers":{"0":label}}),
                                    ));
                                }
                            }
                        }
                    }
                } else {
                    choices.push(("Approve".into(), json!({"kind":"approval","allow":true})));
                }
                choices.push((
                    if question { "Dismiss" } else { "Deny" }.into(),
                    json!({"kind":"approval","allow":false}),
                ));
                let mut buttons = Vec::new();
                for (label, mut data) in choices {
                    data["session"] = json!(session);
                    data["request"] = json!(request);
                    data["card"] = json!(key);
                    data["destination"] = json!(to.destination);
                    buttons.push((label, self.server.channels.action(&to.person, &data)?));
                }
                let mut text = text;
                if question && buttons.len() == 1 {
                    text.push_str(
                        "\nFree-text/multiple questions remain queued for the owner interface.",
                    );
                }
                let card = json!({"key":key,"to":to,"session":session,"request":request,"text":text,"buttons":buttons,"status":"pending","rendered":null});
                self.server.channels.save_relay(&key, &card)?;
                card
            };
            // The original recipient owns this card even if another person selects the session.
            if card["to"] != serde_json::to_value(to)? {
                continue;
            }
            let text = if status == "pending" {
                card["text"].as_str().unwrap().to_string()
            } else {
                format!("{}\n\nRequest {}.", card["text"].as_str().unwrap(), status)
            };
            let ids = self.publish(to, &key, &text).await?;
            let message = ids
                .last()
                .ok_or_else(|| anyhow!("No prompt message receipt"))?;
            if card["rendered"] != status {
                let buttons = if status == "pending" {
                    serde_json::from_value(card["buttons"].clone())?
                } else {
                    vec![]
                };
                self.wire
                    .controls(to, message, &self.wire.redact(&text), buttons)
                    .await?;
                card["rendered"] = json!(status);
            }
            card["status"] = json!(status);
            self.server.channels.save_relay(&key, &card)?;
        }
        Ok(())
    }

    pub(super) async fn refresh_cards(&self) -> Result<()> {
        for card in self.server.channels.pending_cards()? {
            let to: Address = serde_json::from_value(card["to"].clone())?;
            self.sync_prompts(&to, card["session"].as_str().unwrap())
                .await?;
        }
        Ok(())
    }

    pub(super) async fn prompt_action(&self, to: &Address, data: &Value) -> Result<()> {
        let session = data["session"]
            .as_str()
            .ok_or_else(|| anyhow!("Missing prompt session"))?;
        if self.current(&to.person).await? != session {
            bail!("This prompt is not for your active session.");
        }
        let card = self
            .server
            .channels
            .relay(
                data["card"]
                    .as_str()
                    .ok_or_else(|| anyhow!("Missing prompt card"))?,
            )?
            .ok_or_else(|| anyhow!("Prompt expired"))?;
        if card["to"] != serde_json::to_value(to)? {
            bail!("This prompt belongs to another person or DM.");
        }
        let approvals = op(
            &self.server,
            json!({"op":"approvals","name":session,"all":true}),
        )
        .await?;
        let approval = approvals
            .as_array()
            .into_iter()
            .flatten()
            .find(|a| a["id"] == data["request"])
            .ok_or_else(|| anyhow!("Prompt is no longer available"))?;
        if approval["status"] == "pending" {
            if approval["expires_at"]
                .as_i64()
                .is_some_and(|t| t <= super::super::now())
            {
                bail!("This prompt expired.");
            }
            let request = if data["kind"] == "answer" {
                json!({"op":"answer","id":data["request"],"answers":data["answers"]})
            } else {
                json!({"op":if data["allow"]==true{"approve"}else{"deny"},"id":data["request"],"always":false,"session_wide":false})
            };
            let decision = op(&self.server, request).await;
            self.sync_prompts(to, session).await?;
            decision?;
        } else {
            self.sync_prompts(to, session).await?;
        }
        Ok(())
    }
    pub(super) async fn queued_notice(&self, to: &Address, event: &str, text: &str) -> Result<()> {
        let key = format!("release:{}:{event}", to.destination);
        let card = match self.server.channels.relay(&key)? {
            Some(v) => v,
            None => {
                let action=self.server.channels.action(&to.person,&json!({"kind":"release","destination":to.destination,"session":self.current(&to.person).await?,"event":event,"card":key}))?;
                let v = json!({"key":format!("{event}:queued"),"action":action,"text":text,"status":"pending"});
                self.server.channels.save_relay(&key, &v)?;
                v
            }
        };
        let ids = self
            .publish(to, card["key"].as_str().unwrap(), text)
            .await?;
        if card["rendered"] != true {
            self.wire
                .controls(
                    to,
                    ids.last().unwrap(),
                    text,
                    vec![(
                        "Release now".into(),
                        card["action"].as_str().unwrap().into(),
                    )],
                )
                .await?;
            let mut card = card;
            card["rendered"] = json!(true);
            self.server.channels.save_relay(&key, &card)?;
        }
        Ok(())
    }

    pub(super) async fn release_action(&self, to: &Address, data: &Value) -> Result<()> {
        if self.current(&to.person).await? != data["session"] {
            bail!("This queue belongs to a different selected session.");
        }
        let key = data["card"]
            .as_str()
            .ok_or_else(|| anyhow!("Missing queue card"))?;
        let mut card = self
            .server
            .channels
            .relay(key)?
            .ok_or_else(|| anyhow!("Queue card expired"))?;
        let count = if self
            .server
            .channels
            .job_status(data["event"].as_str().unwrap())?
            .as_deref()
            == Some("pending")
        {
            self.server.channels.force(&to.person)?
        } else {
            0
        };
        let text = format!(
            "{}\n\nReleased {count} pending messages.",
            card["text"].as_str().unwrap()
        );
        let ids = self
            .publish(to, card["key"].as_str().unwrap(), &text)
            .await?;
        self.wire
            .controls(to, ids.last().unwrap(), &text, vec![])
            .await?;
        card["status"] = json!("released");
        self.server.channels.save_relay(key, &card)?;
        Ok(())
    }
}
