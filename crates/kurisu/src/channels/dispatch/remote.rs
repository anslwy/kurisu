//! Owner-scoped remote commands backed by the existing remote broker.
use super::*;

impl Dispatcher {
    fn remote_owner(to: &Address) -> Result<()> {
        if to.person != "owner" {
            bail!("/web, /terminal and /browser are available only to the locally paired owner identity.");
        }
        Ok(())
    }
    fn remote_owner_note(kind: &str) -> String {
        format!(
            "/{kind} is owner-only (/web, /terminal and /browser are available only to the locally paired owner identity)."
        )
    }
    async fn control_card(
        &self,
        to: &Address,
        key: &str,
        text: &str,
        choices: Vec<(String, Value)>,
    ) -> Result<()> {
        let old = self.server.channels.relay(key)?;
        let card = if let Some(v) = old {
            v
        } else {
            let mut buttons = Vec::new();
            for (label, mut data) in choices {
                data["session"] = json!(self.current(to).await?);
                data["destination"] = json!(to.destination);
                data["card"] = json!(key);
                buttons.push((label, self.server.channels.action(&to.person, &data)?));
            }
            let v = json!({"to":to,"text":text,"buttons":buttons,"status":"pending"});
            self.server.channels.save_relay(key, &v)?;
            v
        };
        if card["to"] != serde_json::to_value(to)? {
            bail!("Remote card belongs to another conversation");
        }
        let ids = self
            .publish(to, key, card["text"].as_str().unwrap())
            .await?;
        let buttons = if card["status"] == "pending" {
            serde_json::from_value(card["buttons"].clone())?
        } else {
            vec![]
        };
        self.wire
            .controls(
                to,
                ids.last().unwrap(),
                card["text"].as_str().unwrap(),
                buttons,
            )
            .await
    }
    pub(crate) async fn finish_control_card(&self, key: &str, outcome: &str) -> Result<()> {
        let Some(mut card) = self.server.channels.relay(key)? else {
            return Ok(());
        };
        let to: Address = serde_json::from_value(card["to"].clone())?;
        let text = format!("{}\n\n{outcome}", card["text"].as_str().unwrap());
        let ids = self.publish(&to, key, &text).await?;
        self.wire
            .controls(&to, ids.last().unwrap(), &text, vec![])
            .await?;
        card["status"] = json!("resolved");
        self.server.channels.save_relay(key, &card)?;
        Ok(())
    }
    async fn remote_link(
        &self,
        to: &Address,
        key: &str,
        kind: &str,
        dir: Option<&str>,
    ) -> Result<()> {
        Self::remote_owner(to)?;
        let mut request = json!({"op":"remote_link","kind":kind,"channel_target":to});
        if let Some(dir) = dir {
            request["dir"] = json!(dir);
        }
        let link = self.op(to, request).await?;
        self.publish(to,key,&format!("One-time /{kind} link (expires in 5 minutes). Opening it shows a code and asks for approval in this DM:\n{}",link["url"].as_str().unwrap_or(""))).await?;
        Ok(())
    }
    pub(super) async fn remote_command(
        &self,
        to: &Address,
        key: &str,
        kind: &str,
        arg: &str,
    ) -> Result<()> {
        if to.person != "owner" {
            self.publish(to, key, &Self::remote_owner_note(kind))
                .await?;
            return Ok(());
        }
        let raw = arg.trim();
        if raw.eq_ignore_ascii_case("close") {
            let n=self.op(to,json!({"op":"remote_close","kind":kind,"why":format!("/{kind} close from Discord")})).await?["closed"].as_u64().unwrap_or(0);
            self.publish(to, key, &format!("Closed {n} /{kind} sessions and links."))
                .await?;
            return Ok(());
        }
        if raw.eq_ignore_ascii_case("off") || raw.eq_ignore_ascii_case("disable") {
            self.op(to, json!({"op":"remote_disable","kind":kind})).await?;
            self.publish(
                to,
                key,
                &format!("/{kind} is off; sessions and links are closed."),
            )
            .await?;
            return Ok(());
        }
        let dir = if kind == "terminal" && !raw.is_empty() {
            let path = std::path::Path::new(raw);
            if !path.is_absolute() || !path.is_dir() {
                bail!("Terminal path must be an existing absolute directory.");
            }
            Some(raw.to_string())
        } else {
            None
        };
        let offer = self.op(to, json!({"op":"remote_offer","kind":kind})).await?;
        if offer["enabled"] != true {
            let where_ = offer["where"].as_str().ok_or_else(|| {
                anyhow!(
                    "{}",
                    offer["error"]
                        .as_str()
                        .unwrap_or("No reachable address for remote access")
                )
            })?;
            return self.control_card(to,&format!("enable:{key}"),&format!("Turn on /{kind} at {where_}? This enables access to this host after a separate connection approval."),vec![("Enable".into(),json!({"kind":"remote_enable","remote_kind":kind,"dir":dir})),("Cancel".into(),json!({"kind":"remote_cancel"}))]).await;
        }
        if kind == "terminal" && dir.is_none() {
            let sid = self.current(to).await?;
            let session = self.op(to, json!({"op":"status","name":sid})).await?;
            return self
                .control_card(
                    to,
                    &format!("directory:{key}"),
                    "Choose the terminal directory, or send /terminal /absolute/path.",
                    vec![
                        (
                            "Active session folder".into(),
                            json!({"kind":"remote_directory","dir":session["cwd"]}),
                        ),
                        (
                            "Kurisu home".into(),
                            json!({"kind":"remote_directory","dir":self.server.home()}),
                        ),
                    ],
                )
                .await;
        }
        self.remote_link(to, key, kind, dir.as_deref()).await
    }
    pub(super) async fn remote_action(&self, to: &Address, key: &str, data: &Value) -> Result<()> {
        if to.person != "owner" {
            let kind = data["remote_kind"].as_str().unwrap_or("terminal");
            self.publish(to, key, &Self::remote_owner_note(kind))
                .await?;
            return Ok(());
        }
        if self.current(to).await? != data["session"] {
            bail!("This remote card belongs to another selected session.");
        }
        let card = data["card"]
            .as_str()
            .ok_or_else(|| anyhow!("Missing remote card"))?;
        let saved = self
            .server
            .channels
            .relay(card)?
            .ok_or_else(|| anyhow!("Remote card expired"))?;
        if saved["to"] != serde_json::to_value(to)? || saved["status"] != "pending" {
            bail!("Remote card expired or already resolved");
        }
        match data["kind"].as_str() {
            Some("remote_enable") => {
                let kind = data["remote_kind"].as_str().unwrap();
                self.op(to, json!({"op":"remote_enable","kind":kind})).await?;
                self.finish_control_card(card, "Enabled.").await?;
                self.remote_command(to, key, kind, data["dir"].as_str().unwrap_or(""))
                    .await?;
            }
            Some("remote_directory") => {
                self.remote_link(to, key, "terminal", data["dir"].as_str())
                    .await?;
                self.finish_control_card(card, "Directory selected.")
                    .await?;
            }
            Some("remote_decide") => {
                if data["expires"]
                    .as_i64()
                    .is_some_and(|t| t <= super::super::now())
                {
                    bail!("Connection request expired");
                }
                self.op(to,json!({"op":"remote_decide","attempt":data["attempt"],"approve":data["approve"]})).await?;
                self.finish_control_card(
                    card,
                    if data["approve"] == true {
                        "Approved."
                    } else {
                        "Denied; the link is dead."
                    },
                )
                .await?;
            }
            Some("remote_cancel") => {
                self.finish_control_card(card, "Cancelled.").await?;
            }
            _ => bail!("Unknown remote action"),
        }
        Ok(())
    }
    pub(crate) async fn remote_card(
        &self,
        to: &Address,
        id: &str,
        kind: &str,
        ip: &str,
        ua: &str,
        code: &str,
        expires: i64,
    ) -> Result<()> {
        Self::remote_owner(to)?;
        self.control_card(to,&format!("remote:{id}"),&format!("Approve /{kind} connection?\nCode: {code}\nFrom: {ip}\nDevice: {ua}\nApprove only if this matches the code in your browser."),vec![("Approve".into(),json!({"kind":"remote_decide","attempt":id,"approve":true,"expires":expires})),("Deny".into(),json!({"kind":"remote_decide","attempt":id,"approve":false,"expires":expires}))]).await
    }
}
