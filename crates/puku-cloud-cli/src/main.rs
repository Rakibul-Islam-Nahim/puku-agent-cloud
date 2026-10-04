mod attach;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde_json::json;
use uuid::Uuid;

/// Client for puku-agent-cloud: run puku-cli sessions in cloud microVMs.
#[derive(Parser)]
#[command(name = "puku-cloud", version)]
struct Cli {
    /// Control-plane base URL.
    #[arg(long, env = "PUKU_CLOUD_URL", default_value = "http://127.0.0.1:7770", global = true)]
    url: String,
    /// API key (pkc_...); required when the control plane runs with auth.
    #[arg(long, env = "PUKU_CLOUD_API_KEY", global = true)]
    api_key: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a session and stream it live.
    Run {
        /// The task for the agent.
        prompt: String,
        #[arg(long)]
        repo: Option<String>,
        #[arg(long)]
        branch: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        max_budget_usd: Option<f64>,
        /// Hypervisor: `libkrun` or `cloud_hypervisor`. Omitted takes the
        /// deployment's default.
        #[arg(long)]
        engine: Option<String>,
        /// Print raw event JSON instead of rendered output.
        #[arg(long)]
        json: bool,
        /// Create the session but don't attach.
        #[arg(long)]
        detach: bool,
    },
    /// List sessions.
    Ls {
        #[arg(long)]
        state: Option<String>,
    },
    /// Attach to a session: replay the transcript, then stream live.
    /// Stdin lines are sent to the agent; Ctrl-C interrupts (twice to quit).
    Attach {
        session_id: Uuid,
        /// Replay from this seq (0 = everything, -1 = live only).
        #[arg(long, default_value_t = 0)]
        after_seq: i64,
        #[arg(long)]
        json: bool,
    },
    /// Answer a pending question.
    Answer {
        session_id: Uuid,
        question_id: String,
        answer: String,
    },
    /// Send a follow-up message.
    Input { session_id: Uuid, text: String },
    /// Interrupt the current turn.
    Interrupt { session_id: Uuid },
    /// Park the session (VM stops, state kept; resume later).
    Stop { session_id: Uuid },
    /// Resume a parked session.
    Resume { session_id: Uuid },
    /// Cancel and discard a session.
    Cancel { session_id: Uuid },
    /// Inspect and control memory for this org.
    Memory {
        #[command(subcommand)]
        cmd: MemoryCmd,
    },
    /// Manage scheduled jobs (cron entries that create sessions).
    Schedule {
        #[command(subcommand)]
        cmd: ScheduleCmd,
    },
}

#[derive(Subcommand)]
enum MemoryCmd {
    /// Show whether memory is on, and today's call volume.
    Status,
    /// Turn memory on for this org.
    ///
    /// Enabling it starts sending distilled session transcripts to the memory
    /// service, and behind that to Cloudflare. Off by default for that reason.
    On,
    /// Turn memory off.
    Off,
    /// Print the preamble a session on this repo would be given.
    ///
    /// The fastest way to judge whether memory is helping: read the text the
    /// agent is actually being told.
    Show {
        #[arg(long)]
        repo: Option<String>,
    },
}

#[derive(Subcommand)]
enum ScheduleCmd {
    /// Create a schedule, e.g. --cron "0 8 * * 1-5" (UTC, 5-field cron).
    Create {
        prompt: String,
        #[arg(long)]
        cron: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        repo: Option<String>,
        #[arg(long)]
        branch: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        max_budget_usd: Option<f64>,
    },
    /// List schedules.
    Ls,
    /// Delete a schedule.
    Rm { schedule_id: Uuid },
    Enable { schedule_id: Uuid },
    Disable { schedule_id: Uuid },
    /// Fire a schedule now (does not shift its cadence).
    Run { schedule_id: Uuid },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut headers = reqwest::header::HeaderMap::new();
    if let Some(key) = &cli.api_key {
        headers.insert(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {key}").parse().expect("valid api key"),
        );
    }
    let http = reqwest::Client::builder().default_headers(headers).build()?;
    let base = cli.url.trim_end_matches('/').to_string();
    let api_key = cli.api_key.clone();

    match cli.cmd {
        Cmd::Run { prompt, repo, branch, model, max_budget_usd, engine, json, detach } => {
            let res = http
                .post(format!("{base}/v1/sessions"))
                .json(&json!({
                    "prompt": prompt,
                    "repo": repo,
                    "branch": branch,
                    "model": model,
                    "max_budget_usd": max_budget_usd,
                    "engine": engine,
                }))
                .send()
                .await?;
            // A refused engine comes back as a 400 whose body names what the
            // deployment offers; `error_for_status` would throw that away.
            if res.status().is_client_error() {
                let status = res.status();
                let body: serde_json::Value = res.json().await.unwrap_or_default();
                let msg = body["error"]["message"].as_str().unwrap_or("request refused");
                anyhow::bail!("{status}: {msg}");
            }
            let resp: serde_json::Value = res.error_for_status()?.json().await?;
            let id: Uuid = serde_json::from_value(resp["id"].clone()).context("no session id")?;
            eprintln!("session {id} created");
            if !detach {
                attach::attach(&base, api_key.as_deref(), id, 0, json).await?;
            }
        }
        Cmd::Ls { state } => {
            let mut req = http.get(format!("{base}/v1/sessions"));
            if let Some(s) = &state {
                req = req.query(&[("state", s)]);
            }
            let rows: Vec<serde_json::Value> =
                req.send().await?.error_for_status()?.json().await?;
            println!("{:<38} {:<14} {:>8}  TITLE", "ID", "STATE", "COST");
            for r in rows {
                let prompt = r["prompt"].as_str().unwrap_or("").chars().take(60).collect::<String>();
                println!(
                    "{:<38} {:<14} {:>8.4}  {}",
                    r["id"].as_str().unwrap_or(""),
                    r["state"].as_str().unwrap_or(""),
                    r["cost_usd"].as_f64().unwrap_or(0.0),
                    prompt
                );
            }
        }
        Cmd::Attach { session_id, after_seq, json } => {
            attach::attach(&base, api_key.as_deref(), session_id, after_seq, json).await?;
        }
        Cmd::Answer { session_id, question_id, answer } => {
            http.post(format!("{base}/v1/sessions/{session_id}/answer"))
                .json(&json!({"question_id": question_id, "answer": answer}))
                .send()
                .await?
                .error_for_status()?;
            eprintln!("answered");
        }
        Cmd::Input { session_id, text } => {
            http.post(format!("{base}/v1/sessions/{session_id}/input"))
                .json(&json!({"text": text}))
                .send()
                .await?
                .error_for_status()?;
            eprintln!("sent");
        }
        Cmd::Interrupt { session_id } => {
            http.post(format!("{base}/v1/sessions/{session_id}/interrupt"))
                .send()
                .await?
                .error_for_status()?;
            eprintln!("interrupt sent");
        }
        Cmd::Stop { session_id } => {
            http.post(format!("{base}/v1/sessions/{session_id}/stop"))
                .send()
                .await?
                .error_for_status()?;
            eprintln!("parking session");
        }
        Cmd::Resume { session_id } => {
            http.post(format!("{base}/v1/sessions/{session_id}/resume"))
                .send()
                .await?
                .error_for_status()?;
            eprintln!("resuming");
        }
        Cmd::Cancel { session_id } => {
            http.delete(format!("{base}/v1/sessions/{session_id}"))
                .send()
                .await?
                .error_for_status()?;
            eprintln!("canceled");
        }
        Cmd::Memory { cmd } => match cmd {
            MemoryCmd::Status => {
                let v: serde_json::Value = http
                    .get(format!("{base}/v1/memory"))
                    .send()
                    .await?
                    .error_for_status()?
                    .json()
                    .await?;
                if !v["available"].as_bool().unwrap_or(false) {
                    eprintln!("memory: not configured on this deployment");
                } else {
                    let on = v["enabled"].as_bool().unwrap_or(false);
                    eprintln!("memory: {}", if on { "on" } else { "off" });
                    if let Some(u) = v.get("usage_today").filter(|u| !u.is_null()) {
                        eprintln!(
                            "today: {} preamble fetches ({} failed, {}ms avg) · {} ingests · {} preamble bytes",
                            u["preamble_fetches"], u["preamble_failures"], u["preamble_ms_avg"],
                            u["ingests"], u["preamble_bytes"]
                        );
                    }
                }
            }
            MemoryCmd::On | MemoryCmd::Off => {
                let enabled = matches!(cmd, MemoryCmd::On);
                let v: serde_json::Value = http
                    .post(format!("{base}/v1/memory"))
                    .json(&json!({ "enabled": enabled }))
                    .send()
                    .await?
                    .error_for_status()?
                    .json()
                    .await?;
                eprintln!(
                    "memory: {}",
                    if v["enabled"].as_bool().unwrap_or(false) { "on" } else { "off" }
                );
            }
            MemoryCmd::Show { repo } => {
                let mut url = format!("{base}/v1/memory/profile");
                if let Some(r) = repo.as_deref().filter(|r| !r.is_empty()) {
                    url.push_str(&format!("?repo={}", urlencode(r)));
                }
                let v: serde_json::Value =
                    http.get(url).send().await?.error_for_status()?.json().await?;
                match v["preamble"].as_str() {
                    Some(p) if !p.is_empty() => println!("{p}"),
                    _ => eprintln!(
                        "nothing to inject yet — no memories a session on this repo would be told"
                    ),
                }
            }
        },
        Cmd::Schedule { cmd } => match cmd {
            ScheduleCmd::Create { prompt, cron, name, repo, branch, model, max_budget_usd } => {
                let row: serde_json::Value = http
                    .post(format!("{base}/v1/schedules"))
                    .json(&json!({
                        "prompt": prompt, "cron": cron, "name": name, "repo": repo,
                        "branch": branch, "model": model, "max_budget_usd": max_budget_usd,
                    }))
                    .send()
                    .await?
                    .error_for_status()?
                    .json()
                    .await?;
                eprintln!(
                    "schedule {} created, next run {}",
                    row["id"].as_str().unwrap_or("?"),
                    row["next_run_at"].as_str().unwrap_or("?")
                );
            }
            ScheduleCmd::Ls => {
                let rows: Vec<serde_json::Value> = http
                    .get(format!("{base}/v1/schedules"))
                    .send()
                    .await?
                    .error_for_status()?
                    .json()
                    .await?;
                println!(
                    "{:<38} {:<3} {:<16} {:<22} PROMPT",
                    "ID", "ON", "CRON", "NEXT RUN"
                );
                for r in rows {
                    println!(
                        "{:<38} {:<3} {:<16} {:<22} {}",
                        r["id"].as_str().unwrap_or(""),
                        if r["enabled"].as_bool().unwrap_or(false) { "on" } else { "off" },
                        r["cron"].as_str().unwrap_or(""),
                        r["next_run_at"].as_str().unwrap_or("").chars().take(19).collect::<String>(),
                        r["prompt"].as_str().unwrap_or("").chars().take(50).collect::<String>(),
                    );
                }
            }
            ScheduleCmd::Rm { schedule_id } => {
                http.delete(format!("{base}/v1/schedules/{schedule_id}"))
                    .send()
                    .await?
                    .error_for_status()?;
                eprintln!("deleted");
            }
            ScheduleCmd::Enable { schedule_id } => {
                http.post(format!("{base}/v1/schedules/{schedule_id}/enable"))
                    .send()
                    .await?
                    .error_for_status()?;
                eprintln!("enabled");
            }
            ScheduleCmd::Disable { schedule_id } => {
                http.post(format!("{base}/v1/schedules/{schedule_id}/disable"))
                    .send()
                    .await?
                    .error_for_status()?;
                eprintln!("disabled");
            }
            ScheduleCmd::Run { schedule_id } => {
                let r: serde_json::Value = http
                    .post(format!("{base}/v1/schedules/{schedule_id}/run"))
                    .send()
                    .await?
                    .error_for_status()?
                    .json()
                    .await?;
                eprintln!("fired: session {}", r["session_id"].as_str().unwrap_or("?"));
            }
        },
    }
    Ok(())
}


/// Percent-encode a query value. A repo URL carries `:` and `/`, which would
/// otherwise split the query string.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
