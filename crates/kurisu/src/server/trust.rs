//! Person authorization for Discord operations and paired sessions' MCP callers.
use super::*;
use crate::channels::state::Denied;

impl Server {
    pub(super) async fn caller_person(&self, caller: &str) -> Result<Option<String>> {
        let mut id = self.store.lock().await.resolve_req(caller)?;
        let mut children: Vec<String> = Vec::new();
        for _ in 0..32 {
            if self.channels.ambiguous_session(&id)? {
                return Err(anyhow!(Denied));
            }
            if let Some(person) = self.channels.session_owner(&id)? {
                for child in children {
                    self.channels.claim_session(&person, &child)?;
                }
                return Ok(Some(person));
            }
            children.push(id.clone());
            let parent = self
                .store
                .lock()
                .await
                .get(&id)?
                .and_then(|s| s.created_by_session);
            match parent {
                Some(parent) => id = parent,
                None => return Ok(None),
            }
        }
        Err(anyhow!(Denied))
    }
    fn sanitize_person_row(&self, person: &str, row: &mut Value) -> Result<()> {
        match row {
            Value::Object(fields) => {
                if let Some(Value::Array(children)) = fields.get_mut("children") {
                    children.retain(|child| {
                        child["id"].as_str().is_some_and(|id| {
                            self.channels.session_owner(id).ok().flatten().as_deref()
                                == Some(person)
                        })
                    });
                }
                let parent = fields.get("created_by_session").and_then(Value::as_str);
                if parent.is_some_and(|id| {
                    self.channels.session_owner(id).ok().flatten().as_deref() != Some(person)
                }) {
                    fields.insert("created_by_session".into(), Value::Null);
                    fields.insert("created_by_name".into(), Value::Null);
                }
                for key in ["replaces", "replaced_by"] {
                    if fields.get(key).and_then(Value::as_str).is_some_and(|id| {
                        self.channels.session_owner(id).ok().flatten().as_deref() != Some(person)
                    }) {
                        fields.insert(key.into(), Value::Null);
                    }
                }
                for value in fields.values_mut() {
                    self.sanitize_person_row(person, value)?;
                }
            }
            Value::Array(rows) => {
                for row in rows {
                    self.sanitize_person_row(person, row)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    pub(super) async fn person_request(
        self: &Arc<Self>,
        person: String,
        mut req: Value,
    ) -> Result<Value> {
        let operation = req["op"]
            .as_str()
            .ok_or_else(|| anyhow!(Denied))?
            .to_string();
        if let Some(caller) = req["caller"].as_str() {
            if self.caller_person(caller).await?.as_deref() != Some(person.as_str()) {
                return Err(anyhow!(Denied));
            }
        }
        let named = matches!(
            operation.as_str(),
            "status"
                | "history"
                | "history_page"
                | "handoff"
                | "send"
                | "stop"
                | "interrupt"
                | "set_description"
                | "set_note"
                | "update"
                | "reharness"
                | "compact"
                | "upload"
                | "approvals"
                | "usage"
                | "audit"
                | "changes"
        );
        let mut target = None;
        if named {
            let key = req["name"].as_str().ok_or_else(|| anyhow!(Denied))?;
            let id = self
                .store
                .lock()
                .await
                .resolve_req(key)
                .map_err(|_| anyhow!(Denied))?;
            self.channels.assert_owner(&person, &id)?;
            req["name"] = json!(id);
            target = Some(id);
        }
        match operation.as_str() {
            "peers" | "list_full" | "list" | "models" | "clis" | "version" => {}
            "create" => {
                req["role"] = json!("worker");
                let base = self
                    .paths
                    .home
                    .join("workspaces/discord-people")
                    .join(&person);
                std::fs::create_dir_all(&base)?;
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700))?;
                // A display name never selects an existing workspace, including an archived person's folder.
                req["cwd"] = json!(base.join(uuid::Uuid::new_v4().simple().to_string()));
                if person != "owner" && req["permission_mode"] == "bypassPermissions" {
                    return Err(anyhow!(Denied));
                }
            }
            "approve" | "deny" | "answer" => {
                let key = req["id"].as_str().ok_or_else(|| anyhow!(Denied))?;
                let approval = self
                    .store
                    .lock()
                    .await
                    .approval_resolve(key)
                    .map_err(|_| anyhow!(Denied))?;
                self.channels.assert_owner(&person, &approval.session)?;
                if req["always"] == true || req["session_wide"] == true {
                    return Err(anyhow!(Denied));
                }
                req["id"] = json!(approval.id);
            }
            "send_file" | "transcribe" => {
                let caller = req["caller"].as_str().ok_or_else(|| anyhow!(Denied))?;
                let id = self.store.lock().await.resolve_req(caller)?;
                let cwd = self
                    .store
                    .lock()
                    .await
                    .get(&id)?
                    .ok_or_else(|| anyhow!(Denied))?
                    .cwd;
                let base = std::fs::canonicalize(cwd).map_err(|_| anyhow!(Denied))?;
                let path = PathBuf::from(req["path"].as_str().ok_or_else(|| anyhow!(Denied))?);
                let path = if path.is_absolute() {
                    path
                } else {
                    base.join(path)
                };
                let path = std::fs::canonicalize(path).map_err(|_| anyhow!(Denied))?;
                if !path.starts_with(base) {
                    return Err(anyhow!(Denied));
                }
            }
            "usage_report" if person == "owner" => {}
            op if op.starts_with("remote_") && person == "owner" && req.get("caller").is_none() => {
            }
            _ if named => {}
            _ => return Err(anyhow!(Denied)),
        }
        if operation == "update"
            && req["fields"].as_object().is_some_and(|fields| {
                fields.keys().any(|k| {
                    !matches!(
                        k.as_str(),
                        "harness" | "model" | "effort" | "description" | "note"
                    )
                })
            })
        {
            return Err(anyhow!(Denied));
        }
        if operation == "handoff" {
            let mut id = target.clone();
            let mut seen = HashSet::new();
            while let Some(current) = id {
                if !seen.insert(current.clone()) || seen.len() > 20 {
                    return Err(anyhow!(Denied));
                }
                self.channels.assert_owner(&person, &current)?;
                id = self
                    .store
                    .lock()
                    .await
                    .get(&current)?
                    .and_then(|s| s.replaces);
            }
        }
        let mut response = self.handle_unchecked(req).await?;
        if operation == "create" {
            let id = response["id"].as_str().ok_or_else(|| anyhow!(Denied))?;
            self.channels.claim_session(&person, id)?;
        }
        if matches!(operation.as_str(), "peers" | "list_full" | "list") {
            if let Some(rows) = response.as_array_mut() {
                rows.retain(|row| {
                    row["id"].as_str().is_some_and(|id| {
                        self.channels.session_owner(id).ok().flatten().as_deref()
                            == Some(person.as_str())
                    })
                });
            }
        }
        if operation == "usage" {
            response["rate_limits"] = Value::Null;
        }
        self.sanitize_person_row(&person, &mut response)?;
        Ok(response)
    }
}
