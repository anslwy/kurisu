//! Durable permission cards; decisions still belong to the existing server/harness.
use super::*;

impl Dispatcher {
    pub(crate) async fn sync_prompts(&self, to: &Address, session: &str) -> Result<()> {
        let _guard = self.ui.lock().await;
        let approvals = self.op(
            to,
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
                let name = approval["session_name"].as_str().unwrap_or(session);
                let single = single_question(approval);
                let question = approval["kind"] == "question";
                let text = match &single {
                    Some(q) => question_text(name, q),
                    None => {
                        let full = format!(
                            "{}\nSession: {name}\nRequest: {request}\n{}",
                            approval["title"].as_str().unwrap_or("Permission requested"),
                            serde_json::to_string_pretty(&approval["input"])?
                        );
                        if full.encode_utf16().count() > 1700 {
                            self.publish(to, &format!("{key}:details"), &full).await?;
                            format!(
                                "{}\nSession: {name}\nRequest: {request}\nFull request is above or attached.",
                                if question { "Question pending" } else { "Permission requested" },
                            )
                        } else {
                            full
                        }
                    }
                };
                let mut choices: Vec<(String, Value)> = Vec::new();
                let multi = single.as_ref().is_some_and(|q| q["multiSelect"] == true);
                let labels = single.as_ref().map(option_labels).unwrap_or_default();
                for (i, label) in labels.iter().enumerate() {
                    choices.push(if multi {
                        (toggle_label(label, false), json!({"kind":"toggle","option":i}))
                    } else {
                        (label.clone(), json!({"kind":"answer","answers":{"0":label}}))
                    });
                }
                if multi {
                    choices.push(("Done".into(), json!({"kind":"done"})));
                }
                if !question {
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
                if question && single.is_none() {
                    text.push_str("\nMulti-question prompts are answered in the owner interface (/web or ku answer).");
                }
                let card = json!({"key":key,"to":to,"session":session,"request":request,"text":text,"buttons":buttons,
                                  "labels":labels,"multi":multi,"selected":[],"status":"pending","rendered":null});
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
                format!("{}\n\n{}", card["text"].as_str().unwrap(), outcome_line(approval, &status))
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
            let session=card["session"].as_str().ok_or_else(||anyhow!("Missing prompt session"))?.to_string();
            if !self.server.channels.selectable(&to,&session)? {
                let mut card=card;
                card["status"]=json!("revoked");
                self.server.channels.save_relay(card["key"].as_str().unwrap(),&card)?;
                continue;
            }
            self.sync_prompts(&to, &session).await?;
        }
        Ok(())
    }

    pub(super) async fn prompt_action(&self, to: &Address, data: &Value) -> Result<()> {
        let session = data["session"]
            .as_str()
            .ok_or_else(|| anyhow!("Missing prompt session"))?;
        if self.current(to).await? != session {
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
        let approvals = self.op(
            to,
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
            if data["kind"] == "toggle" {
                return self.toggle_choice(to, &card, data).await;
            }
            let request = if data["kind"] == "answer" {
                json!({"op":"answer","id":data["request"],"answers":data["answers"]})
            } else if data["kind"] == "done" {
                let picked = selected_labels(&card);
                if picked.is_empty() {
                    bail!("Pick at least one choice first (or reply with your own answer).");
                }
                json!({"op":"answer","id":data["request"],"answers":{"0":picked}})
            } else {
                json!({"op":if data["allow"]==true{"approve"}else{"deny"},"id":data["request"],"always":false,"session_wide":false})
            };
            let decision = self.op(to, request).await;
            self.sync_prompts(to, session).await?;
            decision?;
        } else {
            self.sync_prompts(to, session).await?;
        }
        Ok(())
    }
    /// Multi-select: flip one choice and redraw the card's buttons.
    async fn toggle_choice(&self, to: &Address, card: &Value, data: &Value) -> Result<()> {
        let _guard = self.ui.lock().await;
        let key = card["key"].as_str().ok_or_else(|| anyhow!("Missing prompt card"))?;
        let mut card = self.server.channels.relay(key)?.unwrap_or_else(|| card.clone());
        let i = data["option"].as_u64().ok_or_else(|| anyhow!("Missing choice"))?;
        let mut selected: Vec<u64> = card["selected"].as_array().into_iter().flatten().filter_map(Value::as_u64).collect();
        match selected.iter().position(|x| *x == i) {
            Some(p) => { selected.remove(p); }
            None => selected.push(i),
        }
        selected.sort_unstable();
        card["selected"] = json!(selected);
        let mut buttons: Vec<(String, String)> = serde_json::from_value(card["buttons"].clone())?;
        for (j, label) in card["labels"].as_array().into_iter().flatten().enumerate() {
            if let Some(b) = buttons.get_mut(j) {
                b.0 = toggle_label(label.as_str().unwrap_or(""), selected.contains(&(j as u64)));
            }
        }
        card["buttons"] = json!(buttons);
        let text = card["text"].as_str().unwrap_or("").to_string();
        let ids = self.publish(to, key, &text).await?;
        let message = ids.last().ok_or_else(|| anyhow!("No prompt message receipt"))?;
        self.wire.controls(to, message, &self.wire.redact(&text), buttons).await?;
        self.server.channels.save_relay(key, &card)?;
        Ok(())
    }

    /// A plain reply (or a voice note's transcript) while one of this person's question
    /// cards for `session` is pending answers it. True if it was used as the answer.
    pub(super) async fn answer_with_reply(&self, to: &Address, session: &str, input: &Incoming) -> Result<bool> {
        let approvals = self.op(to, json!({"op":"approvals","name":session})).await?;
        let Some((approval, card)) = approvals.as_array().into_iter().flatten().rev().find_map(|a| {
            let key = format!("ui:{}:{session}:{}", to.destination, a["id"].as_str()?);
            let card = self.server.channels.relay(&key).ok()??;
            (single_question(a).is_some() && card["to"] == serde_json::to_value(to).ok()? && card["status"] == "pending")
                .then(|| (a.clone(), card))
        }) else { return Ok(false) };
        let mut text = input.text.trim().to_string();
        if text.is_empty() {
            let Some(audio) = input.attachments.iter().find(|a| is_audio(a)) else { return Ok(false) };
            const MAX: usize = 25 * 1024 * 1024;
            let url = audio["url"].as_str().ok_or_else(|| anyhow!("Attachment URL missing"))?;
            let bytes = self.wire.download(url, MAX).await?;
            use base64::Engine;
            let saved = self.op(to, json!({"op":"upload","name":session,"filename":audio["filename"].as_str().unwrap_or("voice.ogg"),
                "data_b64":base64::engine::general_purpose::STANDARD.encode(bytes)})).await?;
            let heard = match self.op(to, json!({"op":"transcribe","path":saved["path"]})).await {
                Ok(t) => t["text"].as_str().unwrap_or("").trim().to_string(),
                Err(_) => return Ok(false), // no speech-to-text: the note goes to the session as a file
            };
            if heard.is_empty() { return Ok(false); }
            text = heard;
        }
        if text.is_empty() || !input.attachments.iter().all(is_audio) {
            return Ok(false);
        }
        let mut answer = json!(text);
        if card["multi"] == true {
            let mut picked = selected_labels(&card);
            picked.push(text.clone());
            answer = json!(picked);
        }
        let decision = self.op(to, json!({"op":"answer","id":approval["id"],"answers":{"0":answer}})).await;
        self.sync_prompts(to, session).await?;
        decision?;
        Ok(true)
    }

    pub(super) async fn queued_notice(&self, to: &Address, event: &str, text: &str) -> Result<()> {
        let key = format!("release:{}:{event}", to.destination);
        let card = match self.server.channels.relay(&key)? {
            Some(v) => v,
            None => {
                let action=self.server.channels.action(&to.person,&json!({"kind":"release","destination":to.destination,"session":self.current(to).await?,"event":event,"card":key}))?;
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
        if self.current(to).await? != data["session"] {
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
            self.server.channels.force_conversation(to)?
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

/// The one question of a question approval, when it has exactly one (ask_user always does).
fn single_question(approval: &Value) -> Option<Value> {
    (approval["kind"] == "question").then_some(())?;
    let qs = approval["input"]["questions"].as_array().filter(|qs| qs.len() == 1)?;
    qs.first().filter(|q| q.is_object()).cloned()
}

/// Choice labels that fit on the card: Discord allows 25 buttons, two go to Done/Dismiss.
fn option_labels(q: &Value) -> Vec<String> {
    q["options"].as_array().into_iter().flatten().filter_map(|o| o["label"].as_str().or(o.as_str()))
        .filter(|l| !l.trim().is_empty()).take(23).map(String::from).collect()
}

fn toggle_label(label: &str, on: bool) -> String {
    format!("{} {label}", if on { "☑" } else { "⬜" })
}

fn selected_labels(card: &Value) -> Vec<String> {
    let labels = card["labels"].as_array().cloned().unwrap_or_default();
    card["selected"].as_array().into_iter().flatten().filter_map(Value::as_u64)
        .filter_map(|i| labels.get(i as usize).and_then(Value::as_str).map(String::from)).collect()
}

fn is_audio(a: &Value) -> bool {
    a["content_type"].as_str().is_some_and(|c| c.starts_with("audio/"))
        || crate::stt::is_audio(a["filename"].as_str().unwrap_or(""))
}

/// A question card's body: who asks, the question, and how to answer.
pub(crate) fn question_text(session: &str, q: &Value) -> String {
    let mut lines = vec![format!("❓ Question from {session}")];
    if let Some(h) = q["header"].as_str().filter(|h| !h.is_empty()) {
        lines.push(format!("**{h}**"));
    }
    lines.push(q["question"].as_str().unwrap_or("").chars().take(1200).collect());
    for o in q["options"].as_array().into_iter().flatten() {
        if let (Some(l), Some(d)) = (o["label"].as_str(), o["description"].as_str().filter(|d| !d.is_empty())) {
            lines.push(format!("• {l} — {}", d.chars().take(150).collect::<String>()));
        }
    }
    let has_options = q["options"].as_array().is_some_and(|o| !o.is_empty());
    lines.push(String::new());
    lines.push(match (has_options, q["multiSelect"] == true) {
        (true, true) => "Pick one or more, then tap Done, or reply with your own answer (text or voice).",
        (true, false) => "Tap a choice, or reply with your own answer (text or voice).",
        _ => "Reply with your answer (text or voice).",
    }.into());
    lines.join("\n")
}

/// The line a closed card ends with: the chosen answer, or why it closed.
pub(crate) fn outcome_line(approval: &Value, status: &str) -> String {
    if approval["kind"] != "question" {
        return format!("Request {status}.");
    }
    match status {
        "approved" => {
            let answers: Vec<String> = approval["answer"].as_object().into_iter().flatten()
                .filter_map(|(_, v)| v.as_str().map(String::from)).collect();
            format!("✔ Answered: {}", if answers.is_empty() { "(answered)".into() } else { answers.join("; ") })
        }
        "denied" => "✖ Dismissed.".into(),
        other => format!("Question {other}."),
    }
}
