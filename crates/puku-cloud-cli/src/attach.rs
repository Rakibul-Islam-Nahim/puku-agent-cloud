//! Attach to a session over the relay WebSocket: replay, live tail,
//! interactive stdin, Ctrl-C interrupt (twice to quit).

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use puku_cloud_proto::client_ws::{ClientMsg, ServerMsg};
use puku_cloud_proto::event::{Event, EventKind};
use tokio::io::AsyncBufReadExt;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

pub async fn attach(
    base: &str,
    api_key: Option<&str>,
    session_id: Uuid,
    after_seq: i64,
    raw_json: bool,
) -> Result<()> {
    let ws_base = base
        .replacen("http://", "ws://", 1)
        .replacen("https://", "wss://", 1);
    let auth = api_key.map(|k| format!("?api_key={k}")).unwrap_or_default();
    let url = format!("{ws_base}/v1/sessions/{session_id}/attach{auth}");
    let (ws, _) = connect_async(&url).await.context("connecting to relay")?;
    let (mut sink, mut stream) = ws.split();

    sink.send(Message::Text(
        serde_json::to_string(&ClientMsg::Hello { after_seq })?.into(),
    ))
    .await?;

    let stdin = tokio::io::BufReader::new(tokio::io::stdin());
    let mut stdin_lines = stdin.lines();
    let mut last_ctrl_c: Option<Instant> = None;

    loop {
        tokio::select! {
            msg = stream.next() => {
                let text = match msg {
                    Some(Ok(Message::Text(t))) => t,
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => continue,
                    Some(Err(e)) => return Err(e.into()),
                };
                match serde_json::from_str::<ServerMsg>(&text)? {
                    ServerMsg::Events { events } => {
                        for ev in events {
                            if raw_json {
                                println!("{}", serde_json::to_string(&ev)?);
                            } else {
                                render(&ev);
                            }
                        }
                    }
                    ServerMsg::Live => {
                        if !raw_json {
                            eprintln!("── live ──");
                        }
                    }
                    ServerMsg::State { state, error } => {
                        if !raw_json {
                            match error {
                                Some(e) => eprintln!("[state: {state:?} — {e}]"),
                                None => eprintln!("[state: {state:?}]"),
                            }
                        }
                        if matches!(
                            state,
                            puku_cloud_proto::session::SessionState::Completed
                                | puku_cloud_proto::session::SessionState::Failed
                                | puku_cloud_proto::session::SessionState::Canceled
                        ) {
                            // Keep draining briefly for the final events.
                        }
                    }
                    ServerMsg::Error { message } => eprintln!("error: {message}"),
                }
            }
            line = stdin_lines.next_line() => {
                if let Ok(Some(line)) = line {
                    if !line.trim().is_empty() {
                        sink.send(Message::Text(
                            serde_json::to_string(&ClientMsg::Input { text: line })?.into(),
                        )).await?;
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                if last_ctrl_c.is_some_and(|t| t.elapsed() < Duration::from_secs(2)) {
                    eprintln!("\ndetached (session keeps running; `puku-cloud attach {session_id}` to return)");
                    return Ok(());
                }
                last_ctrl_c = Some(Instant::now());
                eprintln!("\n[interrupt sent — Ctrl-C again within 2s to detach]");
                sink.send(Message::Text(
                    serde_json::to_string(&ClientMsg::Interrupt)?.into(),
                )).await?;
            }
        }
    }
    Ok(())
}

/// Human rendering of the event stream. Agent events are puku-cli
/// stream-json; everything else is platform frames.
fn render(ev: &Event) {
    let p = &ev.payload;
    let ty = p.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match ev.kind {
        EventKind::Agent => match ty {
            "system" => match p.get("subtype").and_then(|s| s.as_str()) {
                Some("init") => {
                    let model = p.get("model").and_then(|m| m.as_str()).unwrap_or("?");
                    eprintln!("[agent ready — model {model}]");
                }
                Some(sub) => {
                    // Never swallow agent-side problems (api_retry, errors):
                    // a silent session looks healthy when it isn't.
                    if let Some(err) = p.get("error").and_then(|e| e.as_str()) {
                        let status = p.get("error_status").and_then(|s| s.as_i64()).unwrap_or(0);
                        let attempt = p.get("attempt").and_then(|a| a.as_i64()).unwrap_or(0);
                        eprintln!("[{sub}: {err} (status {status}, attempt {attempt})]");
                    }
                }
                None => {}
            },
            "assistant" => {
                for block in content_blocks(p) {
                    match block.get("type").and_then(|t| t.as_str()) {
                        Some("text") => {
                            if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                                println!("{text}");
                            }
                        }
                        Some("tool_use") => {
                            let name = block.get("name").and_then(|n| n.as_str()).unwrap_or("?");
                            let input = block
                                .get("input")
                                .map(summarize_input)
                                .unwrap_or_default();
                            eprintln!("  ⏺ {name} {input}");
                        }
                        _ => {}
                    }
                }
            }
            "user" => {
                // Tool results: show a compact one-liner.
                for block in content_blocks(p) {
                    if block.get("type").and_then(|t| t.as_str()) == Some("tool_result") {
                        let is_err = block
                            .get("is_error")
                            .and_then(|e| e.as_bool())
                            .unwrap_or(false);
                        if is_err {
                            eprintln!("  ⎿ tool error");
                        }
                    }
                }
            }
            "result" => {
                let cost = p.get("total_cost_usd").and_then(|c| c.as_f64()).unwrap_or(0.0);
                let subtype = p.get("subtype").and_then(|s| s.as_str()).unwrap_or("?");
                eprintln!("[result: {subtype} — ${cost:.4}]");
                if let Some(r) = p.get("result").and_then(|r| r.as_str()) {
                    println!("{r}");
                }
            }
            _ => {}
        },
        EventKind::Session => {
            if ty == "session.state" {
                let s = p.get("state").and_then(|s| s.as_str()).unwrap_or("?");
                eprintln!("[{s}]");
            }
        }
        EventKind::Exec => {
            if ty == "exec.exited" {
                let code = p.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
                eprintln!("[agent process exited: {code}]");
            }
        }
        EventKind::User => {
            if let Some(text) = p.get("text").and_then(|t| t.as_str()) {
                eprintln!("> {text}");
            }
        }
    }
}

fn content_blocks(p: &serde_json::Value) -> Vec<serde_json::Value> {
    p.get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default()
}

fn summarize_input(input: &serde_json::Value) -> String {
    for key in ["command", "file_path", "pattern", "prompt", "url"] {
        if let Some(v) = input.get(key).and_then(|v| v.as_str()) {
            let s: String = v.chars().take(80).collect();
            return format!("({s})");
        }
    }
    String::new()
}
