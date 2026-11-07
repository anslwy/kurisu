//! `claude-cli-acp`: an ACP agent on stdio whose engine is the user's own
//! interactive `claude` CLI (see lib.rs). Subcommands:
//!   claude-cli-acp                 serve ACP v1 on stdio
//!   claude-cli-acp hook <events> [--permission <secs>]   (internal) Claude Code hook sink
//!   claude-cli-acp smoke <cwd>     live check: new -> prompt -> close -> load -> prompt

use std::path::PathBuf;
use std::sync::Arc;

use agent_client_protocol::v1::schema::{
    AgentCapabilities, CancelNotification, ContentBlock, InitializeRequest, InitializeResponse, LoadSessionRequest,
    LoadSessionResponse, NewSessionRequest, PromptRequest, TextContent,
};
use agent_client_protocol::{Agent, Stdio};
use claude_cli_acp::{ClaudeCli, Config};
use tokio::sync::mpsc;

fn state_dir() -> PathBuf {
    std::env::var_os("CLAUDE_CLI_ACP_STATE")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let base = std::env::var_os("XDG_STATE_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state"));
            base.join("claude-cli-acp")
        })
}

fn config() -> Config {
    let me = std::env::current_exe().expect("current_exe").to_string_lossy().into_owned();
    let mut cfg = Config::new(state_dir(), vec![me, "hook".into()]);
    if let Ok(extra) = std::env::var("CLAUDE_CLI_ACP_ARGS") {
        cfg.extra_args = extra.split_whitespace().map(String::from).collect();
    }
    cfg
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("hook") => claude_cli_acp::run_hook_args(&args[2..]),
        Some("smoke") => smoke(PathBuf::from(args.get(2).expect("cwd"))).await,
        _ => serve().await,
    }
}

async fn serve() -> anyhow::Result<()> {
    let cli = Arc::new(ClaudeCli::new(config()));
    let (c1, c2, c3, c4) = (cli.clone(), cli.clone(), cli.clone(), cli.clone());
    Agent
        .v1()
        .name("claude-cli-acp")
        .on_receive_request(
            async move |init: InitializeRequest, responder, _cx| {
                responder.respond(
                    InitializeResponse::new(init.protocol_version)
                        .agent_capabilities(AgentCapabilities::new().load_session(true)),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: NewSessionRequest, responder, cx| {
                let cli = c1.clone();
                cx.spawn(async move {
                    match cli.new_session(req).await {
                        Ok(r) => responder.respond(r),
                        Err(e) => responder.respond_with_error(agent_client_protocol::Error::internal_error().data(e.to_string())),
                    }
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: LoadSessionRequest, responder, cx| {
                let cli = c2.clone();
                cx.spawn(async move {
                    match cli.load_session(req).await {
                        Ok(()) => responder.respond(LoadSessionResponse::new()),
                        Err(e) => responder.respond_with_error(agent_client_protocol::Error::internal_error().data(e.to_string())),
                    }
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: PromptRequest, responder, cx| {
                let cli = c3.clone();
                let notify = cx.clone();
                let asker = cx.clone();
                cx.spawn(async move {
                    let (tx, mut rx) = mpsc::channel(64);
                    let fwd = tokio::spawn(async move {
                        while let Some(n) = rx.recv().await {
                            let _ = notify.send_notification(n);
                        }
                    });
                    // Permission prompts become ACP session/request_permission calls to the client.
                    let (ptx, mut prx) = mpsc::channel::<agent_client_protocol::v1::schema::RequestPermissionRequest>(8);
                    let pcli = cli.clone();
                    let perms = tokio::spawn(async move {
                        while let Some(p) = prx.recv().await {
                            let (asker, pcli) = (asker.clone(), pcli.clone());
                            tokio::spawn(async move {
                                let (sid, rid) = (p.session_id.clone(), p.tool_call.tool_call_id.to_string());
                                let outcome = match asker.send_request(p).block_task().await {
                                    Ok(r) => r.outcome,
                                    Err(_) => agent_client_protocol::v1::schema::RequestPermissionOutcome::Cancelled,
                                };
                                let _ = pcli.respond_permission(&sid, &rid, &outcome, Some("denied by the ACP client")).await;
                            });
                        }
                    });
                    let res = cli.prompt_with_permissions(req, tx, Some(ptx)).await;
                    let _ = fwd.await;
                    let _ = perms.await;
                    match res {
                        Ok(r) => responder.respond(r),
                        Err(e) => responder.respond_with_error(agent_client_protocol::Error::internal_error().data(e.to_string())),
                    }
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |n: CancelNotification, _cx| {
                let _ = c4.cancel(&n.session_id).await;
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_to(Stdio::new())
        .await
        .map_err(|e| anyhow::anyhow!("{e:?}"))
}

async fn ask(cli: &ClaudeCli, id: &agent_client_protocol::v1::schema::SessionId, text: &str) -> anyhow::Result<String> {
    let (tx, mut rx) = mpsc::channel(64);
    let req = PromptRequest::new(id.clone(), vec![ContentBlock::Text(TextContent::new(text))]);
    let resp = cli.prompt(req, tx).await?;
    let mut reply = String::new();
    while let Ok(n) = rx.try_recv() {
        if let agent_client_protocol::v1::schema::SessionUpdate::AgentMessageChunk(c) = n.update {
            if let ContentBlock::Text(t) = c.content {
                reply.push_str(&t.text);
            }
        }
    }
    eprintln!("stop_reason={:?}", resp.stop_reason);
    Ok(reply)
}

async fn smoke(cwd: PathBuf) -> anyhow::Result<()> {
    let cli = ClaudeCli::new(config());
    let t0 = std::time::Instant::now();
    let s = cli.new_session(NewSessionRequest::new(cwd.clone())).await?;
    eprintln!("session {} ready in {:?}", s.session_id, t0.elapsed());
    let r1 = ask(&cli, &s.session_id, "Reply with exactly the word PONG and nothing else.").await?;
    println!("reply1={r1:?}");
    cli.close(&s.session_id).await?;
    eprintln!("killed; resuming");
    cli.load_session(LoadSessionRequest::new(s.session_id.clone(), cwd)).await?;
    let r2 = ask(&cli, &s.session_id, "What single word did you reply with last time? Answer with just that word.").await?;
    println!("reply2={r2:?}");
    cli.close(&s.session_id).await?;
    anyhow::ensure!(r1.trim() == "PONG" && r2.contains("PONG"), "smoke failed");
    println!("SMOKE OK");
    Ok(())
}
