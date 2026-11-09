//! Antigravity owns authentication and conversations; Kurisu speaks only ACP.
use agent_client_protocol::v1::schema::SessionId;
use anyhow::{bail, Result};
use serde_json::json;
use crate::acp::AcpDriver;

pub const MODELS: &[&str] = &[
    "gemini-3.8-flash", "gemini-3.8-flash-high", "gemini-3.8-flash-medium",
    "gemini-3.8-flash-low", "gemini-4-argon", "gemini-4-argon-high",
    "gemini-4-argon-medium", "gemini-4-argon-low", "claude-sonnet-4-6",
    "claude-opus-4-6-thinking", "gpt-oss-120b-medium",
];
pub fn bin() -> String {
    std::env::var("KURISU_AGY_BIN").unwrap_or_else(|_| "agy".into())
}

/// Families whose `-high|-medium|-low` suffix is a thought_level, with the
/// level the CLI itself uses when no level is sent.
const FAMILIES: &[(&str, &str)] = &[("gemini-3.8-flash", "high"), ("gemini-4-argon", "medium")];

/// Suffixes in CLI model names map to ACP's separate thought_level option.
/// An explicit Kurisu effort overrides the suffix; unspecified effort falls
/// back to the family's CLI default (high for 3.8 Flash, medium for 4 Argon).
pub fn model_options<'a>(model: &'a str, effort: Option<&'a str>) -> Result<(&'a str, &'a str)> {
    let model = if model == "default" { "gemini-3.8-flash" } else { model };
    if !MODELS.contains(&model) {
        bail!("unknown agy model {model:?}; valid ids: {}", MODELS.join(", "));
    }
    let (id, default): (&'a str, &'a str) = match FAMILIES.iter()
        .find(|(base, _)| model == *base || model.starts_with(&format!("{base}-"))) {
        Some((base, level)) => (base, level),
        None => (model, "high"),
    };
    let suffix = model.strip_prefix(id).and_then(|r| r.strip_prefix('-'))
        .filter(|level| !level.is_empty()).unwrap_or(default);
    let thought = effort.filter(|e| *e != "default").unwrap_or(suffix);
    if !["high", "medium", "low"].contains(&thought) {
        bail!("agy thought_level must be high|medium|low");
    }
    Ok((id, thought))
}

pub async fn configure(driver: &AcpDriver, id: &SessionId, model: &str, effort: Option<&str>, mode: Option<&str>) -> Result<()> {
    let (model, thought) = model_options(model, effort)?;
    driver.rpc("session/set_config_option", json!({"sessionId":id,"configId":"model","value":model})).await?;
    driver.rpc("session/set_config_option", json!({"sessionId":id,"configId":"thought_level","value":thought})).await?;
    let mode = match mode.unwrap_or("default") {
        "default" | "auto" => "default",
        "acceptEdits" => "accept-edits",
        "plan" => "plan",
        other => bail!("agy permission mode {other:?} unsupported; use default|acceptEdits|plan"),
    };
    driver.rpc("session/set_mode", json!({"sessionId":id,"modeId":mode})).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::{agent_text, Driver};
    use std::{path::PathBuf, sync::atomic::Ordering, time::Duration};
    use tokio::sync::mpsc;

    async fn fixture(attempts: usize) -> AcpDriver {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/fixtures/agy-acp.py");
        AcpDriver::spawn_for("agy", path.to_string_lossy().into(), vec![("AGY_FIXTURE_AUTH_ATTEMPTS".into(), attempts.to_string())]).await.unwrap()
    }

    #[test]
    fn models_map_suffix_and_explicit_effort() {
        assert_eq!(model_options("default", None).unwrap(), ("gemini-3.8-flash", "high"));
        assert_eq!(model_options("gemini-3.8-flash-low", None).unwrap(), ("gemini-3.8-flash", "low"));
        assert_eq!(model_options("gemini-3.8-flash-medium", Some("high")).unwrap(), ("gemini-3.8-flash", "high"));
        for id in MODELS { assert!(model_options(id, None).is_ok(), "{id} rejected"); }
        let error = model_options("bogus", None).unwrap_err().to_string();
        assert!(error.contains("valid ids:") && error.contains("claude-sonnet-4-6"));
        assert!(model_options("default", Some("max")).is_err());
    }

    #[test]
    fn argon_models_strip_suffix_and_default_to_medium() {
        assert_eq!(model_options("gemini-4-argon", None).unwrap(), ("gemini-4-argon", "medium"));
        assert_eq!(model_options("gemini-4-argon-low", None).unwrap(), ("gemini-4-argon", "low"));
        assert_eq!(model_options("gemini-4-argon-high", None).unwrap(), ("gemini-4-argon", "high"));
        assert_eq!(model_options("gemini-4-argon-medium", None).unwrap(), ("gemini-4-argon", "medium"));
        assert_eq!(model_options("gemini-4-argon-low", Some("high")).unwrap(), ("gemini-4-argon", "high"));
        assert!(model_options("gemini-4-argon-ultra", None).is_err());
    }

    #[tokio::test]
    async fn warmup_retries_then_succeeds_and_keeps_stdin_open() {
        let driver = fixture(2).await;
        let response = driver.session(None, std::env::current_dir().unwrap(), json!([])).await.unwrap();
        assert_eq!(response["attempts"], 3);
        assert_eq!(response["sessionId"], "agy-conversation-123");
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(driver.alive.load(Ordering::Acquire));
        let id = SessionId::new("agy-conversation-123");
        driver.configure(&id, "default", None, Some("acceptEdits")).await.unwrap();
        let (tx, mut rx) = mpsc::channel(32);
        let result = driver.prompt(&id, "ping", tx).await.unwrap();
        assert!(matches!(result.stop_reason, agent_client_protocol::v1::schema::StopReason::EndTurn));
        let mut updates = Vec::new();
        while let Some(update) = rx.recv().await { updates.push(update); }
        assert_eq!(agent_text(&updates), "PONG");
        assert_eq!(updates.len(), 4);
        assert_eq!(driver.usage.lock().await["agy-conversation-123"]["input_tokens"], 10);
        driver.shutdown().await;
    }

    #[tokio::test]
    async fn permanent_auth_failure_is_bounded_and_actionable() {
        let driver = fixture(usize::MAX).await;
        let start = tokio::time::Instant::now();
        let error = driver.session_with_warmup("session/new", json!({"cwd":std::env::current_dir().unwrap(),"mcpServers":[]}), Duration::from_millis(600)).await.unwrap_err();
        assert!(error.to_string().contains("run `agy acp --login`"));
        assert!(start.elapsed() < Duration::from_secs(2));
        driver.shutdown().await;
    }

    #[tokio::test]
    async fn resume_uses_persisted_conversation_id() {
        let driver = fixture(0).await;
        let id = driver.new_session(std::env::current_dir().unwrap()).await.unwrap();
        driver.shutdown().await;
        let resumed = fixture(0).await;
        resumed.load_session(&id, std::env::current_dir().unwrap()).await.unwrap();
        let (tx, mut rx) = mpsc::channel(32);
        resumed.prompt(&id, "continue", tx).await.unwrap();
        assert!(rx.recv().await.is_some());
        resumed.shutdown().await;
    }

    #[tokio::test]
    async fn missing_binary_has_install_command() {
        let error = AcpDriver::spawn_for("agy", "/nonexistent/kurisu-agy".into(), vec![]).await.err().unwrap();
        assert!(error.to_string().contains("curl -fsSL https://antigravity.google/cli/install.sh | bash"));
    }

    #[tokio::test]
    async fn permissions_map_kurisu_option_to_agent_option() {
        use agent_client_protocol::v1::schema::{RequestPermissionOutcome, SelectedPermissionOutcome, PermissionOptionId};
        let driver = fixture(0).await;
        let id = driver.new_session(std::env::current_dir().unwrap()).await.unwrap();
        let (updates, _rx) = mpsc::channel(32);
        let (permissions, mut requests) = mpsc::channel(4);
        let prompt = driver.prompt_with_permissions(&id, "permission", updates, Some(permissions));
        let approve = async {
            let request = requests.recv().await.unwrap();
            let outcome = RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(PermissionOptionId::new(claude_cli_acp::ALLOW_ONCE)));
            driver.respond_permission(&request.tool_call.tool_call_id.to_string(), &outcome).await.unwrap();
        };
        let (result, ()) = tokio::join!(prompt, approve);
        result.unwrap();
        driver.shutdown().await;
    }

    /// Safe live handshake: never reads credentials or consumes a prompt quota.
    #[tokio::test]
    async fn real_agy_acp_session_and_config() {
        let _env = crate::server::OPENCODE_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        if crate::hostcheck::which(&bin()).is_none() {
            eprintln!("SKIP: agy absent");
            return;
        }
        let driver = AcpDriver::spawn_for("agy", bin(), vec![]).await.unwrap();
        let session = driver.session(None, std::env::current_dir().unwrap(), json!([])).await;
        let value = match session {
            Ok(value) => value,
            Err(error) if error.to_string().starts_with("agy authentication required;") => {
                eprintln!("SKIP: agy not logged in: {error}");
                driver.shutdown().await;
                return;
            }
            Err(error) => panic!("agy ACP failed: {error:#}"),
        };
        let id = SessionId::new(value["sessionId"].as_str().unwrap().to_owned());
        driver.configure(&id, "default", None, Some("default")).await.unwrap();
        driver.close(&id).await.unwrap();
        driver.shutdown().await;
    }
}
