//! End-to-end tests across controld, the worker link and the session state
//! machine. See `harness.rs` for why these exist and how to enable them.

#![cfg(test)]

use puku_cloud_proto::session::SessionState;
use puku_cloud_proto::worker_proto::Up;
use uuid::Uuid;

use crate::harness::{start, start_with, FakeWorker, Harness, Opts};

/// `PUKU_TEST_DATABASE_URL` unset → skip rather than fail, so a laptop with
/// no database still gets a green suite.
macro_rules! harness {
    () => {
        match start().await {
            Some(h) => h,
            None => return,
        }
    };
    ($opts:expr) => {
        match start_with($opts).await {
            Some(h) => h,
            None => return,
        }
    };
}

async fn create_session(h: &Harness, body: serde_json::Value) -> Uuid {
    let (status, row) = h.post("/v1/sessions", body).await;
    assert_eq!(status, 200, "create failed: {row}");
    Uuid::parse_str(row["id"].as_str().unwrap()).unwrap()
}

/// The whole loop: a session is dispatched, the agent blocks on a question,
/// a human answers over REST, and the answer reaches the guest as the
/// `control_response` puku-cli is actually waiting on.
#[tokio::test]
async fn question_is_answered_with_a_control_response() {
    let h = harness!();
    let mut w = FakeWorker::connect(&h, "w1", "test-worker-token").await.unwrap();

    let id = create_session(&h, serde_json::json!({"prompt": "ask me something"})).await;
    let spec = w.next_assignment().await.expect("session should be assigned");
    assert_eq!(spec.session_id, id);

    // Drive to running, then report the frame a real worker sends after
    // detect_question matches a can_use_tool control request.
    for st in [SessionState::Booting, SessionState::Bootstrapping, SessionState::Running] {
        w.send(Up::SessionState { session_id: id, state: st, error: None, puku_session_id: None })
            .await
            .unwrap();
    }
    w.send(Up::PendingQuestion {
        session_id: id,
        question: serde_json::json!({
            "kind": "can_use_tool",
            "request_id": "req-int-1",
            "tool_name": "AskUserQuestion",
            "tool_use_id": "call_int",
            "input": {"questions": [{"question": "Tabs or spaces?", "header": "Indentation"}]},
        }),
    })
    .await
    .unwrap();
    h.await_state(id, &["waiting_input"]).await;

    let (status, _) = h
        .post(
            &format!("/v1/sessions/{id}/answer"),
            serde_json::json!({"question_id": "req-int-1", "answer": "Tabs"}),
        )
        .await;
    assert_eq!(status, 202);

    // The frame that reaches the guest must be a control_response echoing
    // the request id, with the answer keyed by the question's header. A
    // user message here would leave puku-cli blocked forever.
    let line = w.next_input().await.expect("an input frame");
    let v: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(v["type"], "control_response");
    assert_eq!(v["response"]["request_id"], "req-int-1");
    assert_eq!(v["response"]["response"]["behavior"], "allow");
    assert_eq!(
        v["response"]["response"]["updatedInput"]["answers"]["Indentation"],
        "Tabs"
    );

    // Answering unblocks the session and clears the question.
    h.await_state(id, &["running"]).await;
    let (_, s) = h.get(&format!("/v1/sessions/{id}")).await;
    assert!(s["pending_question"].is_null());
}

/// A stale answer must not look like it worked.
#[tokio::test]
async fn answering_the_wrong_question_is_refused() {
    let h = harness!();
    let mut w = FakeWorker::connect(&h, "w-stale", "test-worker-token").await.unwrap();
    let id = create_session(&h, serde_json::json!({"prompt": "x"})).await;
    w.next_assignment().await.unwrap();
    for st in [SessionState::Booting, SessionState::Bootstrapping, SessionState::Running] {
        w.send(Up::SessionState { session_id: id, state: st, error: None, puku_session_id: None })
            .await
            .unwrap();
    }
    w.send(Up::PendingQuestion {
        session_id: id,
        question: serde_json::json!({"kind": "can_use_tool", "request_id": "current", "input": {}}),
    })
    .await
    .unwrap();
    h.await_state(id, &["waiting_input"]).await;

    let (status, body) = h
        .post(
            &format!("/v1/sessions/{id}/answer"),
            serde_json::json!({"question_id": "stale", "answer": "yes"}),
        )
        .await;
    assert_eq!(status, 409, "{body}");
}

/// A revoked worker token must not register, and a token already bound to
/// one host must not be reusable from another.
#[tokio::test]
async fn worker_tokens_are_per_host_and_revocable() {
    let h = harness!();
    let token = crate::workertoken::create(&h.pool, "box-1").await.unwrap();

    // Bind it by registering once.
    let mut w = FakeWorker::connect(&h, "box-1", &token).await.unwrap();
    let id = create_session(&h, serde_json::json!({"prompt": "bind the token"})).await;
    assert!(w.next_assignment().await.is_some(), "bound worker should get work");
    let _ = id;

    // The same token from a different host is refused: a leaked token must
    // not silently fan out across machines.
    let mut other = FakeWorker::connect(&h, "box-2", &token).await.unwrap();
    assert!(other.recv().await.is_none(), "second host must be rejected");

    // And a revoked token stops working entirely.
    sqlx::query("UPDATE worker_tokens SET revoked_at = now() WHERE name = 'box-1'")
        .execute(&h.pool)
        .await
        .unwrap();
    let mut revoked = FakeWorker::connect(&h, "box-1", &token).await.unwrap();
    assert!(revoked.recv().await.is_none(), "revoked token must be rejected");
}

/// An unknown token is refused even when the shared secret is enabled.
#[tokio::test]
async fn unknown_worker_tokens_are_refused() {
    let h = harness!();
    let mut w = FakeWorker::connect(&h, "impostor", "not-the-token").await.unwrap();
    assert!(w.recv().await.is_none());
}

/// The deployment ceiling is the real gate: a caller asking for more
/// privilege than the deployment allows gets the ceiling, in the stored row
/// and in the spec the worker receives.
#[tokio::test]
async fn permission_mode_is_clamped_end_to_end() {
    let h = harness!();
    let mut w = FakeWorker::connect(&h, "w-clamp", "test-worker-token").await.unwrap();
    let id = create_session(
        &h,
        serde_json::json!({
            "prompt": "narrow me",
            "permission_mode": "plan",
            "disallowed_tools": ["Bash"],
        }),
    )
    .await;
    let spec = w.next_assignment().await.unwrap();
    assert_eq!(spec.session_id, id);
    // Narrower than the ceiling, so it is honoured rather than widened.
    assert_eq!(
        spec.permission_mode,
        Some(puku_cloud_proto::session::PermissionMode::Plan)
    );
    assert_eq!(spec.disallowed_tools, vec!["Bash".to_string()]);
    // The runner needs a turn budget or the agent stops after one response.
    assert_eq!(spec.max_turns, Some(50));
}

/// Unattended runs authenticate with the org's stored credential. This is
/// the path that had a reader and no writer, so every scheduled job silently
/// spent the operator's key.
#[tokio::test]
async fn scheduled_runs_use_the_stored_org_credential() {
    let h = harness!();
    let mut w = FakeWorker::connect(&h, "w-cred", "test-worker-token").await.unwrap();

    let (status, _) = h
        .post(
            "/v1/credentials",
            serde_json::json!({"kind": "api_key", "value": "pk_live_INTTEST_SENTINEL"}),
        )
        .await;
    assert_eq!(status, 200);

    // Stored encrypted, never in the clear.
    let (raw,): (Vec<u8>,) = sqlx::query_as("SELECT value_enc FROM org_credentials LIMIT 1")
        .fetch_one(&h.pool)
        .await
        .unwrap();
    assert!(
        !String::from_utf8_lossy(&raw).contains("SENTINEL"),
        "credential stored in plaintext"
    );

    let (status, sched) = h
        .post(
            "/v1/schedules",
            serde_json::json!({
                "prompt": "nightly", "cron": "0 3 * * *", "disallowed_tools": ["Bash"],
            }),
        )
        .await;
    assert_eq!(status, 200, "{sched}");
    let (status, _) = h
        .post(&format!("/v1/schedules/{}/run", sched["id"].as_str().unwrap()), serde_json::json!({}))
        .await;
    assert_eq!(status, 200);

    let spec = w.next_assignment().await.expect("the fired schedule should dispatch");
    // On the bearer field, not puku_api_key: a pk_live_ value authenticates
    // to the puku gateway as a bearer regardless of the `kind` it was filed
    // under. Routed to puku_api_key it goes out as x-api-key and the gateway
    // reports a missing token.
    assert_eq!(
        spec.puku_auth_token.as_deref(),
        Some("pk_live_INTTEST_SENTINEL"),
        "the schedule must run on the stored credential, not the operator's"
    );
    assert_ne!(
        spec.puku_api_key.as_deref(),
        Some("pk_live_INTTEST_SENTINEL"),
        "a puku key on the x-api-key field never reaches the gateway"
    );
    // And the schedule's policy reaches the guest.
    assert_eq!(spec.disallowed_tools, vec!["Bash".to_string()]);
}

/// With no stored credential and no operator fallback, the session fails
/// before a VM boots rather than after the agent 401s.
#[tokio::test]
async fn a_session_with_no_credential_fails_before_booting() {
    let h = harness!(Opts { no_operator_key: true, ..Default::default() });
    let mut w = FakeWorker::connect(&h, "w-nocred", "test-worker-token").await.unwrap();
    let id = create_session(&h, serde_json::json!({"prompt": "no creds anywhere"})).await;
    h.await_state(id, &["failed"]).await;
    let (_, s) = h.get(&format!("/v1/sessions/{id}")).await;
    assert!(
        s["error"].as_str().unwrap_or_default().contains("no model credential"),
        "unhelpful error: {}",
        s["error"]
    );
    // Nothing should have been handed to a worker. The session is already
    // `failed`, so a short window is conclusive.
    w.assert_no_assignment(std::time::Duration::from_secs(1)).await;
}

/// Sessions belong to their owner, not to everyone in the org.
#[tokio::test]
async fn other_users_cannot_see_a_session() {
    // Auth on: with it off every caller is the dev identity *with admin*,
    // which can see everything and would make this assertion meaningless.
    let h = harness!(Opts { auth_required: true, ..Default::default() });
    // A second user in the SAME org.
    let other = Uuid::new_v4();
    sqlx::query("INSERT INTO users (id, org_id, email, role) VALUES ($1,$2,$3,'member')")
        .bind(other)
        .bind(h.org)
        .bind(format!("{other}@test"))
        .execute(&h.pool)
        .await
        .unwrap();
    let id = create_session(&h, serde_json::json!({"prompt": "alice's session"})).await;
    sqlx::query("UPDATE sessions SET user_id = $2 WHERE id = $1")
        .bind(id)
        .bind(other)
        .execute(&h.pool)
        .await
        .unwrap();

    // The harness key belongs to a different user, so the row must be
    // invisible — 404, not 403: existence is not leaked across users.
    let (status, _) = h.get(&format!("/v1/sessions/{id}")).await;
    assert_eq!(status, 404);
}

/// A teleported session is dispatched as a resume with its transcript
/// attached, which is what makes the cloud agent remember the local
/// conversation.
#[tokio::test]
async fn imported_sessions_carry_their_transcript() {
    let h = harness!();
    let mut w = FakeWorker::connect(&h, "w-import", "test-worker-token").await.unwrap();
    let local = Uuid::new_v4().to_string();
    let (status, row) = h
        .post(
            "/v1/sessions/import",
            serde_json::json!({
                "puku_session_id": local,
                "transcript": "{\"type\":\"user\",\"text\":\"ORCHID\"}\n",
                "prompt": "what codename did I give you?",
            }),
        )
        .await;
    assert_eq!(status, 200, "{row}");

    let spec = w.next_assignment().await.expect("import should dispatch");
    assert!(spec.resume, "an imported session must resume, not start fresh");
    assert_eq!(spec.puku_session_id.as_deref(), Some(local.as_str()));
    // No object storage in the harness, so it travels inline.
    match spec.import {
        Some(puku_cloud_proto::session::ImportRef::Inline { jsonl }) => {
            assert!(jsonl.contains("ORCHID"))
        }
        other => panic!("expected an inline transcript, got {other:?}"),
    }
}

/// `--from-pr` resolves through the pr_url the PR flow records.
#[tokio::test]
async fn sessions_are_findable_by_pull_request() {
    let h = harness!();
    let id = create_session(&h, serde_json::json!({"prompt": "opens a pr"})).await;
    sqlx::query("UPDATE sessions SET pr_url = $2 WHERE id = $1")
        .bind(id)
        .bind("https://github.com/acme/widgets/pull/128")
        .execute(&h.pool)
        .await
        .unwrap();

    // A bare number is what a person types.
    let (status, rows) = h.get("/v1/sessions?pr=128").await;
    assert_eq!(status, 200);
    assert_eq!(rows[0]["id"].as_str().unwrap(), id.to_string());

    // A full URL works too, and an unrelated PR matches nothing.
    let (_, rows) = h.get("/v1/sessions?pr=https%3A%2F%2Fgithub.com%2Facme%2Fwidgets%2Fpull%2F128").await;
    assert_eq!(rows.as_array().unwrap().len(), 1);
    let (_, rows) = h.get("/v1/sessions?pr=999").await;
    assert!(rows.as_array().unwrap().is_empty());
}

/// The fleet view reports both directions of disagreement between what the
/// platform believes and what the host actually has.
#[tokio::test]
async fn fleet_reports_drift_in_both_directions() {
    let h = harness!();
    let mut w = FakeWorker::connect(&h, "w-fleet", "test-worker-token").await.unwrap();
    let id = create_session(&h, serde_json::json!({"prompt": "runs somewhere"})).await;
    let spec = w.next_assignment().await.unwrap();
    for st in [SessionState::Booting, SessionState::Bootstrapping, SessionState::Running] {
        w.send(Up::SessionState { session_id: id, state: st, error: None, puku_session_id: None })
            .await
            .unwrap();
    }
    h.await_state(id, &["running"]).await;

    // The host reports an unrelated sandbox and NOT this session's: one
    // orphan, one vanished VM.
    w.send(Up::Heartbeat { used_slots: 1, capacity_slots: None, sandboxes: vec!["ses-orphaned000".into()], host: None })
        .await
        .unwrap();
    // Give the heartbeat a moment to land in the registry.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let (status, fleet) = h.get("/v1/fleet").await;
    assert_eq!(status, 200);
    let vanished = fleet["drift"]["vanished"].as_array().unwrap();
    let orphaned = fleet["drift"]["orphaned"].as_array().unwrap();
    assert!(
        vanished.iter().any(|v| v["sandbox"] == spec.sandbox_name),
        "a running session with no sandbox on the host should be flagged: {fleet}"
    );
    assert!(
        orphaned.iter().any(|o| o["sandbox"] == "ses-orphaned000"),
        "a sandbox with no session should be flagged: {fleet}"
    );
}

/// A shared deployment must never spend the operator's money for a caller
/// who stored no credential of their own. The operator key is present here
/// and must still be refused -- removing the key would prove nothing.
#[tokio::test]
async fn a_shared_deployment_refuses_to_fall_back_to_the_operator() {
    let h = harness!(crate::harness::Opts { multi_tenant: true, ..Default::default() });
    let mut w = FakeWorker::connect(&h, "w-mt", "test-worker-token").await.unwrap();

    let (status, s) = h.post("/v1/sessions", serde_json::json!({"prompt": "hello"})).await;
    assert_eq!(status, 200, "{s}");
    let id = s["id"].as_str().unwrap();

    crate::api::dispatch_pending(&h.state).await.ok();
    let assigned =
        tokio::time::timeout(std::time::Duration::from_millis(600), w.next_assignment()).await;
    assert!(
        matches!(assigned, Err(_) | Ok(None)),
        "a session with no credential of its own must not reach a worker"
    );

    let (_, got) = h.get(&format!("/v1/sessions/{id}")).await;
    assert_eq!(got["state"], "failed", "it should fail up front, not after the agent 401s");
    let err = got["error"].as_str().unwrap_or_default();
    assert!(
        err.contains("credential") && err.contains("refresh"),
        "the error must say what is missing and how to fix it, got: {err}"
    );
}

/// The static PAT clones whatever it can read, for whichever caller names a
/// repo. It is operator-wide, so a shared deployment must not attach it.
#[tokio::test]
async fn the_operator_git_token_stays_off_a_shared_deployments_specs() {
    let h = harness!(crate::harness::Opts { multi_tenant: true, ..Default::default() });
    let mut w = FakeWorker::connect(&h, "w-git", "test-worker-token").await.unwrap();
    let (status, _) = h
        .post("/v1/credentials", serde_json::json!({"kind": "bearer", "value": "caller-bearer"}))
        .await;
    assert_eq!(status, 200);

    let (status, s) = h
        .post("/v1/sessions", serde_json::json!({"prompt": "x", "repo": "https://github.com/a/b"}))
        .await;
    assert_eq!(status, 200, "{s}");

    let spec = w.next_assignment().await.expect("a session with a credential dispatches");
    assert_eq!(spec.git_token, None, "the operator PAT must not ride along");
    assert_eq!(spec.puku_api_key, None, "nor the operator model key");
    assert_eq!(spec.puku_auth_token.as_deref(), Some("caller-bearer"));
}

/// A refresh token that cannot be exchanged must fail the session up front,
/// not boot a VM that will 401 on its first model call. The harness points
/// at a dead issuer, which is what a revoked token looks like from here.
#[tokio::test]
async fn an_unusable_refresh_token_fails_before_booting() {
    let h = harness!(crate::harness::Opts { multi_tenant: true, ..Default::default() });
    let mut w = FakeWorker::connect(&h, "w-rt", "test-worker-token").await.unwrap();
    let (status, body) = h
        .post("/v1/credentials", serde_json::json!({"kind": "refresh", "value": "rt-abc"}))
        .await;
    assert_eq!(status, 200, "refresh must be an accepted credential kind: {body}");

    let (status, s) = h.post("/v1/sessions", serde_json::json!({"prompt": "x"})).await;
    assert_eq!(status, 200, "{s}");
    let id = s["id"].as_str().unwrap();

    crate::api::dispatch_pending(&h.state).await.ok();
    let assigned =
        tokio::time::timeout(std::time::Duration::from_millis(600), w.next_assignment()).await;
    assert!(matches!(assigned, Err(_) | Ok(None)), "a dead refresh token must not boot a VM");

    let (_, got) = h.get(&format!("/v1/sessions/{id}")).await;
    assert_eq!(got["state"], "failed");
}

/// Continuing a conversation after the agent finished its turn. Since a
/// successful result completes the session, this is the ordinary path --
/// and it was refused outright with "invalid transition completed ->
/// scheduled" until Completed -> Scheduled became legal.
#[tokio::test]
async fn a_follow_up_message_resumes_a_finished_session() {
    let h = harness!();
    let mut w = FakeWorker::connect(&h, "w-resume", "test-worker-token").await.unwrap();
    let (status, s) = h.post("/v1/sessions", serde_json::json!({"prompt": "first turn"})).await;
    assert_eq!(status, 200, "{s}");
    let id = s["id"].as_str().unwrap().to_string();
    let first = w.next_assignment().await.expect("the first turn dispatches");
    assert_eq!(first.prompt, "first turn");

    let sid = uuid::Uuid::parse_str(&id).unwrap();
    for st in [SessionState::Booting, SessionState::Bootstrapping, SessionState::Running] {
        // Report puku-cli's own session id on the way to running, as a real
        // worker does from the first line of the agent stream. Without it
        // there is nothing to --resume against.
        let psid = (st == SessionState::Running).then(|| "cli-session-1".to_string());
        w.send(Up::SessionState { session_id: sid, state: st, error: None, puku_session_id: psid })
            .await
            .unwrap();
    }
    w.send(Up::SessionState {
        session_id: sid,
        state: SessionState::Completed,
        error: None,
        puku_session_id: None,
    })
    .await
    .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let (_, got) = h.get(&format!("/v1/sessions/{id}")).await;
    assert_eq!(got["state"], "completed");

    let (status, body) =
        h.post(&format!("/v1/sessions/{id}/input"), serde_json::json!({"text": "second turn"}))
            .await;
    assert_eq!(status, 202, "a follow-up must resume, not 409: {body}");

    let second = w.next_assignment().await.expect("the follow-up dispatches");
    assert_eq!(second.prompt, "second turn", "the agent must receive the new turn");
    assert!(second.resume, "and resume rather than start fresh");
}

/// A refused resume must not leave the session holding a prompt it never
/// ran -- the original instruction would simply be gone.
#[tokio::test]
async fn a_refused_resume_leaves_the_prompt_alone() {
    let h = harness!();
    let mut w = FakeWorker::connect(&h, "w-refuse", "test-worker-token").await.unwrap();
    let (status, s) = h.post("/v1/sessions", serde_json::json!({"prompt": "the original"})).await;
    assert_eq!(status, 200, "{s}");
    let id = s["id"].as_str().unwrap().to_string();
    w.next_assignment().await.expect("dispatches");
    let sid = uuid::Uuid::parse_str(&id).unwrap();
    for st in [
        SessionState::Booting,
        SessionState::Bootstrapping,
        SessionState::Running,
        SessionState::Canceled,
    ] {
        w.send(Up::SessionState { session_id: sid, state: st, error: None, puku_session_id: None })
            .await
            .unwrap();
    }
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let (status, _) =
        h.post(&format!("/v1/sessions/{id}/input"), serde_json::json!({"text": "clobber"})).await;
    assert_ne!(status, 202, "a canceled session has no volumes to resume onto");

    let (_, got) = h.get(&format!("/v1/sessions/{id}")).await;
    assert_eq!(got["prompt"], "the original", "a refused resume must not rewrite the prompt");
}

/// Wait until controld has persisted puku-cli's own session id.
///
/// `build_spec` derives `resume` from this column, and the worker reports it
/// on a frame of its own — so a follow-up posted before that write lands
/// dispatches as a fresh run rather than a resume.
async fn await_puku_session_id(h: &Harness, id: Uuid) {
    for _ in 0..300 {
        let (_, row) = h.get(&format!("/v1/sessions/{id}")).await;
        if row["puku_session_id"].as_str().is_some() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("session {id} never recorded a puku_session_id");
}

/// Wait for a session's recorded cost to reach `want`.
///
/// `send` returns when the frame is on the socket, not when controld has
/// applied it, and usage frames drive no state change to await on.
async fn await_cost(h: &Harness, id: Uuid, want: f64) -> f64 {
    // Generous on purpose. The whole suite shares one Postgres and runs
    // concurrently, so a resume that takes 400ms alone can take several
    // seconds under load -- a tight budget here fails the suite for a reason
    // that has nothing to do with billing.
    let mut last = f64::NAN;
    for _ in 0..300 {
        let (_, row) = h.get(&format!("/v1/sessions/{id}")).await;
        last = row["cost_usd"].as_f64().unwrap_or(f64::NAN);
        if (last - want).abs() < 1e-6 {
            return last;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    last
}

/// A resumed session must bill for both runs, not just the last one.
///
/// puku-cli restates its cumulative total on every `result`, and a resume
/// boots a fresh sandbox whose counter starts over. Before the baseline
/// existed, the second run's total simply overwrote the first's — a real
/// document session recorded $1.510 after spending $1.722 + $1.510.
#[tokio::test]
async fn a_resumed_session_bills_for_both_runs() {
    let h = harness!();
    let mut w = FakeWorker::connect(&h, "w1", "test-worker-token").await.unwrap();

    let id = create_session(&h, serde_json::json!({"prompt": "first turn"})).await;
    w.next_assignment().await.expect("assigned");
    for st in [SessionState::Booting, SessionState::Bootstrapping, SessionState::Running] {
        w.send(Up::SessionState { session_id: id, state: st, error: None, puku_session_id: None })
            .await
            .unwrap();
    }
    // A resume needs puku-cli's own session id, which a real worker reports
    // once it knows it.
    w.send(Up::SessionState {
        session_id: id,
        state: SessionState::Running,
        error: None,
        puku_session_id: Some("cli-sess-1".into()),
    })
    .await
    .unwrap();
    // The follow-up below only dispatches as a resume once this has landed.
    await_puku_session_id(&h, id).await;

    w.send(Up::SessionUsage {
        session_id: id,
        cost_usd: 1.50,
        tokens_in: 1000,
        tokens_out: 200,
        cache_read_tokens: 10,
        cache_write_tokens: 5,
    })
    .await
    .unwrap();
    w.send(Up::SessionState {
        session_id: id,
        state: SessionState::Completed,
        error: None,
        puku_session_id: None,
    })
    .await
    .unwrap();
    h.await_state(id, &["completed"]).await;

    assert_eq!(await_cost(&h, id, 1.50).await, 1.50, "first run");

    // Continue the conversation. This is the ordinary follow-up path now
    // that a successful result completes a session.
    let (status, _) = h
        .post(&format!("/v1/sessions/{id}/input"), serde_json::json!({"text": "second turn"}))
        .await;
    assert!((200..300).contains(&status), "resume rejected: {status}");

    let spec = w.next_assignment().await.expect("reassigned on resume");
    assert!(spec.resume, "a follow-up turn should dispatch as a resume");
    for st in [SessionState::Booting, SessionState::Bootstrapping, SessionState::Running] {
        w.send(Up::SessionState { session_id: id, state: st, error: None, puku_session_id: None })
            .await
            .unwrap();
    }

    // The fresh sandbox counts from zero and reports LESS than the first
    // run — which is exactly the shape that used to erase the earlier spend.
    w.send(Up::SessionUsage {
        session_id: id,
        cost_usd: 0.25,
        tokens_in: 300,
        tokens_out: 50,
        cache_read_tokens: 3,
        cache_write_tokens: 1,
    })
    .await
    .unwrap();

    let cost = await_cost(&h, id, 1.75).await;
    assert!(
        (cost - 1.75).abs() < 1e-6,
        "expected 1.50 + 0.25 = 1.75, got {cost} — a resume is erasing the earlier spend"
    );
    let (_, row) = h.get(&format!("/v1/sessions/{id}")).await;
    assert_eq!(row["tokens_in"].as_i64().unwrap(), 1300);
    assert_eq!(row["tokens_out"].as_i64().unwrap(), 250);

    // And a second report from the SAME sandbox restates the cumulative; it
    // must not stack on top of itself.
    w.send(Up::SessionUsage {
        session_id: id,
        cost_usd: 0.40,
        tokens_in: 400,
        tokens_out: 60,
        cache_read_tokens: 4,
        cache_write_tokens: 2,
    })
    .await
    .unwrap();
    let cost = await_cost(&h, id, 1.90).await;
    assert!(
        (cost - 1.90).abs() < 1e-6,
        "expected 1.50 + 0.40 = 1.90, got {cost} — reports within one sandbox are double-counting"
    );
}

/// The push-time re-mint must be gated too, not just the spec.
///
/// `build_spec` withheld the operator PAT at dispatch, and then
/// `Up::RequestGitToken` handed it over an hour later — the worker asks for a
/// fresh token before pushing, because the clone token has expired by then.
/// The spec path had a test; this one did not, and that is where it drifted.
#[tokio::test]
async fn the_operator_git_token_is_refused_at_push_time_too() {
    let h = harness!(crate::harness::Opts { multi_tenant: true, ..Default::default() });
    let mut w = FakeWorker::connect(&h, "w-git-push", "test-worker-token").await.unwrap();
    let (status, _) = h
        .post("/v1/credentials", serde_json::json!({"kind": "bearer", "value": "caller-bearer"}))
        .await;
    assert_eq!(status, 200);

    let id = create_session(
        &h,
        serde_json::json!({"prompt": "x", "repo": "https://github.com/a/b"}),
    )
    .await;
    let spec = w.next_assignment().await.expect("assigned");
    assert_eq!(spec.git_token, None, "not on the spec");

    // What the worker sends from gitpush.rs when it is ready to push.
    w.send(Up::RequestGitToken { session_id: id }).await.unwrap();

    let token = w.next_git_token().await.expect("controld must answer, even to refuse");
    assert_eq!(
        token, None,
        "a shared deployment handed out the operator PAT at push time"
    );
}

// ------------------------------------------------------------------ engines

async fn events_of(h: &Harness, id: Uuid) -> Vec<serde_json::Value> {
    let (_, evs) = h.get(&format!("/v1/sessions/{id}/events")).await;
    evs.as_array().cloned().unwrap_or_default()
}

/// The rule the engine field rests on. A worker from before engines ignores
/// the field and would boot libkrun for a session that asked for Cloud
/// Hypervisor, so it must never be handed one.
#[tokio::test]
async fn an_older_worker_is_never_handed_a_cloud_hypervisor_session() {
    let h = harness!();
    let mut w = FakeWorker::connect(&h, "w-old", "test-worker-token").await.unwrap();
    let (status, row) = h
        .post("/v1/sessions", serde_json::json!({"prompt": "x", "engine": "cloud_hypervisor"}))
        .await;
    assert_eq!(status, 200, "{row}");
    assert_eq!(row["engine"], "cloud_hypervisor");
    let id = Uuid::parse_str(row["id"].as_str().unwrap()).unwrap();

    w.assert_no_assignment(std::time::Duration::from_secs(1)).await;
    let (_, s) = h.get(&format!("/v1/sessions/{id}")).await;
    assert_eq!(s["state"], "created", "it waits for a worker that runs its engine");
    assert!(
        events_of(&h, id).await.iter().any(|e| e["payload"]["type"] == "session.waiting_for_worker"),
        "the transcript must say why nothing is happening"
    );
}

/// The dispatcher used to `return` on the first session it could not place.
/// With engines that meant one queued Cloud Hypervisor session blocked every
/// libkrun session behind it.
#[tokio::test]
async fn an_unplaceable_session_does_not_block_the_queue() {
    let h = harness!();
    let mut w = FakeWorker::connect(&h, "w-queue", "test-worker-token").await.unwrap();
    let blocked = create_session(&h, serde_json::json!({"prompt": "a", "engine": "cloud_hypervisor"})).await;
    let runnable = create_session(&h, serde_json::json!({"prompt": "b"})).await;

    let spec = w.next_assignment().await.expect("the libkrun session behind it dispatches");
    assert_eq!(spec.session_id, runnable);
    assert_eq!(spec.engine, puku_cloud_proto::Engine::Libkrun);
    w.assert_no_assignment(std::time::Duration::from_secs(1)).await;
    let (_, s) = h.get(&format!("/v1/sessions/{blocked}")).await;
    assert_eq!(s["state"], "created");
}

/// A worker that advertises Cloud Hypervisor gets Cloud Hypervisor work, and
/// the spec says so.
#[tokio::test]
async fn a_cloud_hypervisor_worker_receives_cloud_hypervisor_sessions() {
    let h = harness!();
    let mut w = FakeWorker::connect_with_engines(
        &h,
        "w-ch",
        "test-worker-token",
        vec![puku_cloud_proto::Engine::Libkrun, puku_cloud_proto::Engine::CloudHypervisor],
    )
    .await
    .unwrap();
    let id = create_session(&h, serde_json::json!({"prompt": "x", "engine": "cloud_hypervisor"})).await;
    let spec = w.next_assignment().await.expect("assigned");
    assert_eq!(spec.session_id, id);
    assert_eq!(spec.engine, puku_cloud_proto::Engine::CloudHypervisor);

    // The fleet records what the worker said it runs.
    let (status, workers) = h.get("/v1/workers").await;
    assert_eq!(status, 200, "{workers}");
    let row = workers.as_array().unwrap().iter().find(|w| w["name"] == "w-ch").unwrap().clone();
    assert!(row["engines"].as_array().unwrap().contains(&serde_json::json!("cloud_hypervisor")));
}

#[tokio::test]
async fn an_unknown_engine_is_refused_at_the_edge() {
    let h = harness!();
    let (status, body) =
        h.post("/v1/sessions", serde_json::json!({"prompt": "x", "engine": "firecracker"})).await;
    assert_eq!(status, 400, "{body}");
    let msg = body["error"]["message"].as_str().unwrap_or_default();
    assert!(msg.contains("libkrun"), "the refusal must say what is offered: {msg}");
}

/// Omitting the field is exactly the old request, and gets the old engine.
#[tokio::test]
async fn a_request_that_names_no_engine_gets_the_default() {
    let h = harness!();
    let mut w = FakeWorker::connect(&h, "w-default", "test-worker-token").await.unwrap();
    let id = create_session(&h, serde_json::json!({"prompt": "x"})).await;
    let spec = w.next_assignment().await.unwrap();
    assert_eq!(spec.session_id, id);
    assert_eq!(spec.engine, puku_cloud_proto::Engine::Libkrun);
}

#[tokio::test]
async fn schedules_carry_their_engine_to_every_run() {
    let h = harness!();
    let (status, sched) = h
        .post(
            "/v1/schedules",
            serde_json::json!({"prompt": "nightly", "cron": "0 3 * * *", "engine": "cloud_hypervisor"}),
        )
        .await;
    assert_eq!(status, 200, "{sched}");
    assert_eq!(sched["engine"], "cloud_hypervisor");
    let (status, fired) = h
        .post(&format!("/v1/schedules/{}/run", sched["id"].as_str().unwrap()), serde_json::json!({}))
        .await;
    assert_eq!(status, 200, "{fired}");
    let (_, s) = h.get(&format!("/v1/sessions/{}", fired["session_id"].as_str().unwrap())).await;
    assert_eq!(s["engine"], "cloud_hypervisor");

    let (status, _) = h
        .post("/v1/schedules", serde_json::json!({"prompt": "x", "cron": "0 3 * * *", "engine": "nope"}))
        .await;
    assert_eq!(status, 400);
}

/// Drive a freshly assigned session to `completed` with a puku session id,
/// so a follow-up dispatches as a resume.
async fn complete_first_turn(h: &Harness, w: &mut FakeWorker, id: Uuid) {
    for st in [SessionState::Booting, SessionState::Bootstrapping, SessionState::Running] {
        let psid = (st == SessionState::Running).then(|| format!("cli-{id}"));
        w.send(Up::SessionState { session_id: id, state: st, error: None, puku_session_id: psid })
            .await
            .unwrap();
    }
    w.send(Up::SessionState { session_id: id, state: SessionState::Completed, error: None, puku_session_id: None })
        .await
        .unwrap();
    h.await_state(id, &["completed"]).await;
    for _ in 0..300 {
        let (_, row) = h.get(&format!("/v1/sessions/{id}")).await;
        if row["puku_session_id"].is_string() && row["volume_worker_id"].is_string() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("session {id} never recorded its puku session id and volume worker");
}

/// The workspace is on the disk of the worker that ran the first turn. A
/// resume that lands on a less-loaded worker elsewhere boots `--resume`
/// against an empty disk, so the idle worker must not get it.
#[tokio::test]
async fn a_resume_goes_back_to_the_worker_holding_the_volumes() {
    let h = harness!();
    let mut home = FakeWorker::connect(&h, "w-home", "test-worker-token").await.unwrap();
    let id = create_session(&h, serde_json::json!({"prompt": "first turn"})).await;
    home.next_assignment().await.expect("assigned");
    complete_first_turn(&h, &mut home, id).await;

    let mut idle = FakeWorker::connect(&h, "w-idle", "test-worker-token").await.unwrap();
    // The home worker is now the busier of the two: least-loaded placement
    // alone would pick the idle one.
    home.send(Up::Heartbeat { used_slots: 3, capacity_slots: None, sandboxes: vec![], host: None })
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let (status, body) =
        h.post(&format!("/v1/sessions/{id}/input"), serde_json::json!({"text": "second turn"})).await;
    assert_eq!(status, 202, "{body}");
    let spec = home.next_assignment().await.expect("the resume goes home");
    assert!(spec.resume);
    idle.assert_no_assignment(std::time::Duration::from_secs(1)).await;
}

/// ...and when that worker is not coming back, the session says so instead
/// of waiting in `scheduled` for ever.
#[tokio::test]
async fn a_resume_whose_volume_host_is_long_gone_fails_with_a_reason() {
    let h = harness!();
    let mut home = FakeWorker::connect(&h, "w-gone", "test-worker-token").await.unwrap();
    let id = create_session(&h, serde_json::json!({"prompt": "first turn"})).await;
    home.next_assignment().await.expect("assigned");
    complete_first_turn(&h, &mut home, id).await;

    drop(home);
    for _ in 0..200 {
        if h.state.workers.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert!(h.state.workers.is_empty(), "the dropped worker should have disconnected");
    sqlx::query("UPDATE workers SET last_heartbeat_at = now() - interval '1 hour' WHERE name = 'w-gone'")
        .execute(&h.pool)
        .await
        .unwrap();

    let (status, _) =
        h.post(&format!("/v1/sessions/{id}/input"), serde_json::json!({"text": "again"})).await;
    assert_eq!(status, 202);
    h.await_state(id, &["failed"]).await;
    let (_, s) = h.get(&format!("/v1/sessions/{id}")).await;
    let err = s["error"].as_str().unwrap_or_default();
    assert!(err.contains("w-gone") && err.contains("volumes"), "unhelpful error: {err}");
}

/// Attaching is answering, typing into and interrupting a session. It used
/// to check the org only, so a colleague could drive someone else's agent.
#[tokio::test]
async fn another_user_cannot_attach_to_a_session() {
    use puku_cloud_proto::client_ws::{ClientMsg, ServerMsg};

    let h = harness!(Opts { auth_required: true, ..Default::default() });
    let other = Uuid::new_v4();
    sqlx::query("INSERT INTO users (id, org_id, email, role) VALUES ($1,$2,$3,'member')")
        .bind(other)
        .bind(h.org)
        .bind(format!("{other}@test"))
        .execute(&h.pool)
        .await
        .unwrap();
    let id = create_session(&h, serde_json::json!({"prompt": "alice's session"})).await;
    sqlx::query("UPDATE sessions SET user_id = $2 WHERE id = $1")
        .bind(id)
        .bind(other)
        .execute(&h.pool)
        .await
        .unwrap();

    let url = format!(
        "{}/v1/sessions/{id}/attach?api_key={}",
        h.base.replace("http://", "ws://"),
        h.key.as_ref().unwrap()
    );
    let (ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    let (mut tx, mut rx) = futures::StreamExt::split(ws);
    futures::SinkExt::send(
        &mut tx,
        tokio_tungstenite::tungstenite::Message::Text(
            serde_json::to_string(&ClientMsg::Hello { after_seq: 0 }).unwrap().into(),
        ),
    )
    .await
    .unwrap();
    let first = tokio::time::timeout(std::time::Duration::from_secs(5), futures::StreamExt::next(&mut rx))
        .await
        .expect("a reply")
        .expect("an open socket")
        .unwrap();
    let text = first.into_text().unwrap();
    match serde_json::from_str::<ServerMsg>(&text).unwrap() {
        ServerMsg::Error { message } => assert!(message.contains("not found"), "{message}"),
        other => panic!("another user's session must not replay: {other:?}"),
    }
}

// ----------------------------------------------------------------- machines

use crate::harness::FakeDataSocket;
use puku_cloud_proto::data_proto::{DataMsg, StreamTarget};
use puku_cloud_proto::machine::MachineState as MS;
use puku_cloud_proto::worker_proto::{Down, HostReport, StagedImage};

/// Boot a machine through the API and a fake worker; returns its spec.
async fn running_machine(h: &Harness, w: &mut FakeWorker, body: serde_json::Value) -> puku_cloud_proto::machine::MachineSpec {
    let (status, created) = h.post("/v1/machines", body).await;
    assert_eq!(status, 200, "{created}");
    let spec = w.next_machine_assignment().await.expect("the machine is assigned");
    w.machine_state(&spec, MS::Booting, false).await;
    w.machine_state(&spec, MS::Running, false).await;
    // 15s, like `Harness::await_state`: the whole suite shares one Postgres,
    // and 5s was close enough to the floor to flake under a full run.
    for _ in 0..300 {
        let (_, m) = h.get(&format!("/v1/machines/{}", spec.machine_id)).await;
        if m["state"] == "running" {
            return spec;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("machine never reached running");
}

async fn machine_state_is(h: &Harness, id: Uuid, want: &str) {
    for _ in 0..300 {
        let (_, m) = h.get(&format!("/v1/machines/{id}")).await;
        if m["state"] == want {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let (_, m) = h.get(&format!("/v1/machines/{id}")).await;
    panic!("machine {id} never reached {want}: {m}");
}

/// The spec a worker boots from carries the caller's request -- including
/// the secret env, decrypted, which the API never echoes back.
#[tokio::test]
async fn a_machine_is_assigned_with_its_spec_and_secrets() {
    let h = harness!();
    let mut w = FakeWorker::connect_machine_worker(&h, "w-m1").await.unwrap();
    let spec = running_machine(
        &h,
        &mut w,
        serde_json::json!({
            "external_id": "home-1", "engine": "cloud_hypervisor", "memory_mib": 4096,
            "expose": [7070, 6080], "env": {"DISPLAY": ":1"},
            "secret_env": {"CONTROL_TOKEN": "s3cret"},
            "volume": {"path": "/home/pukubot", "uid": 1000},
            "entrypoint": {"argv": ["/usr/local/bin/pukubot-computer"], "user": "1000"},
        }),
    )
    .await;
    assert_eq!(spec.engine, puku_cloud_proto::Engine::CloudHypervisor);
    assert_eq!(spec.expose, vec![7070, 6080]);
    assert_eq!(spec.env["DISPLAY"], ":1");
    assert_eq!(spec.env["CONTROL_TOKEN"], "s3cret", "secrets reach the worker");
    assert_eq!(spec.generation, 1);
    assert!(spec.name.starts_with("mch-"));

    let (_, m) = h.get(&format!("/v1/machines/{}", spec.machine_id)).await;
    assert!(!m.to_string().contains("s3cret"), "secrets never come back out: {m}");
    assert_eq!(m["external_id"], "home-1");
}

/// Ensure-running is what makes `provision` cheap and idempotent: the same
/// external_id returns the same machine, and after a stop it comes back on
/// the worker holding its volume with `resumed: true`.
#[tokio::test]
async fn an_external_id_is_ensure_running_and_resumes_on_its_volume() {
    let h = harness!();
    let mut w = FakeWorker::connect_machine_worker(&h, "w-m2").await.unwrap();
    let body = serde_json::json!({"external_id": "home-2", "volume": {"path": "/home/u"}});
    let first = running_machine(&h, &mut w, body.clone()).await;

    // Again while running: same machine, nothing new assigned.
    let (status, again) = h.post("/v1/machines", body.clone()).await;
    assert_eq!(status, 200, "{again}");
    assert_eq!(again["machine"]["id"], first.machine_id.to_string());
    assert_eq!(again["resumed"], false, "the first boot found no volume");

    // Stop: the worker is told, confirms, and the row says stopped.
    let (status, _) = h.post(&format!("/v1/machines/{}/stop", first.machine_id), serde_json::json!({})).await;
    assert_eq!(status, 202);
    let gen = w
        .next_matching(|d| match d {
            Down::StopMachine { machine_id, generation, .. } if machine_id == first.machine_id => Some(generation),
            _ => None,
        })
        .await
        .expect("the worker is told to stop");
    assert_eq!(gen, 1);
    w.machine_state(&first, MS::Stopped, false).await;
    machine_state_is(&h, first.machine_id, "stopped").await;

    // Ensure running again: a new boot, on the same worker, finding its volume.
    let h2 = h.base.clone();
    let wait = tokio::spawn(async move {
        reqwest::Client::new()
            .post(format!("{h2}/v1/machines"))
            .json(&serde_json::json!({"external_id": "home-2", "volume": {"path": "/home/u"}, "wait_s": 10}))
            .send()
            .await
            .unwrap()
            .json::<serde_json::Value>()
            .await
            .unwrap()
    });
    let second = w.next_machine_assignment().await.expect("reassigned");
    assert_eq!(second.machine_id, first.machine_id);
    assert_eq!(second.generation, 2, "every start is a new generation");
    w.machine_state(&second, MS::Running, true).await;
    let resp = wait.await.unwrap();
    assert_eq!(resp["machine"]["state"], "running", "{resp}");
    assert_eq!(resp["resumed"], true, "the volume was there: the bot's files survived");
}

/// A late frame from an older boot must not change the newer one.
#[tokio::test]
async fn a_report_from_an_older_boot_is_ignored() {
    let h = harness!();
    let mut w = FakeWorker::connect_machine_worker(&h, "w-m3").await.unwrap();
    let first = running_machine(&h, &mut w, serde_json::json!({"external_id": "gen"})).await;
    h.post(&format!("/v1/machines/{}/stop", first.machine_id), serde_json::json!({})).await;
    w.machine_state(&first, MS::Stopped, false).await;
    machine_state_is(&h, first.machine_id, "stopped").await;
    h.post(&format!("/v1/machines/{}/start", first.machine_id), serde_json::json!({})).await;
    let second = w.next_machine_assignment().await.unwrap();
    w.machine_state(&second, MS::Running, true).await;
    machine_state_is(&h, first.machine_id, "running").await;

    // The previous boot's worker-side "failed" arrives late.
    w.send(Up::MachineState {
        machine_id: first.machine_id,
        generation: first.generation,
        state: MS::Failed,
        error: Some("old boot died".into()),
        volume_existed: false,
        reason: None,
    })
    .await
    .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let (_, m) = h.get(&format!("/v1/machines/{}", first.machine_id)).await;
    assert_eq!(m["state"], "running", "{m}");
}

/// Machines only go to workers that said they run them, and a fleet with
/// none refuses the machine at once rather than queueing it for ever.
#[tokio::test]
async fn a_worker_without_the_machines_feature_is_never_given_one() {
    let h = harness!();
    let mut w = FakeWorker::connect(&h, "w-plain", "test-worker-token").await.unwrap();
    for _ in 0..200 {
        if !h.state.workers.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    let (status, body) = h.post("/v1/machines", serde_json::json!({"external_id": "ff-plain"})).await;
    assert_eq!(status, 422, "{body}");
    assert_eq!(body["error"]["reason"], "machines_unsupported");
    let got = tokio::time::timeout(std::time::Duration::from_secs(1), w.next_machine_assignment()).await;
    assert!(matches!(got, Err(_) | Ok(None)), "a plain worker must not get a machine");
}

/// A data socket rides on its worker's control link: it must present the token
/// that link registered with, and a worker with no control link gets none parked.
#[tokio::test]
async fn a_data_socket_must_present_its_workers_token() {
    let h = harness!();
    let _w = FakeWorker::connect_machine_worker(&h, "w-tok").await.unwrap();

    let mut wrong = FakeDataSocket::offer_with_token(&h, "w-tok", "not-the-token").await.unwrap();
    assert!(wrong.closed_within(5).await, "a socket with another token is refused");

    let mut stranger = FakeDataSocket::offer(&h, "w-nobody").await.unwrap();
    assert!(stranger.closed_within(5).await, "a socket for a worker with no control link is refused");

    let mut right = FakeDataSocket::offer(&h, "w-tok").await.unwrap();
    assert!(!right.closed_within(1).await, "the registered token's socket is parked");
}

/// Exec rides the data plane: controld opens a stream, the worker answers.
/// When no idle socket is parked, controld asks the worker for one.
#[tokio::test]
async fn exec_runs_over_a_data_socket_the_worker_is_asked_to_open() {
    let h = harness!();
    let mut w = FakeWorker::connect_machine_worker(&h, "w-exec").await.unwrap();
    let spec = running_machine(&h, &mut w, serde_json::json!({"expose": []})).await;
    let id = spec.machine_id;

    let base = h.base.clone();
    let call = tokio::spawn(async move {
        reqwest::Client::new()
            .post(format!("{base}/v1/machines/{id}/exec"))
            .json(&serde_json::json!({"argv": ["echo", "hi"], "stdin": "input", "timeout_ms": 5000}))
            .send()
            .await
            .unwrap()
    });
    // The pool is empty, so controld asks for sockets on the control link.
    let asked = w
        .next_matching(|d| match d {
            Down::OpenDataSockets { count } => Some(count),
            _ => None,
        })
        .await
        .expect("controld asks for data sockets");
    assert!(asked >= 1);

    let mut ds = FakeDataSocket::offer(&h, "w-exec").await.unwrap();
    let header = ds.header().await;
    assert_eq!(header.machine_id, id);
    match &header.target {
        StreamTarget::Exec { argv, stdin, timeout_ms, .. } => {
            assert_eq!(argv, &vec!["echo".to_string(), "hi".to_string()]);
            assert!(*stdin);
            assert_eq!(*timeout_ms, 5000);
        }
        other => panic!("expected exec, got {other:?}"),
    }
    ds.msg(&DataMsg::Ready).await;
    assert_eq!(ds.read_to_eof().await, b"input");
    ds.msg(&DataMsg::ExecResult {
        code: 0,
        stdout: "hi\n".into(),
        stderr: String::new(),
        timed_out: false,
        truncated: false,
    })
    .await;

    let resp = call.await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["code"], 0);
    assert_eq!(body["stdout"], "hi\n");
}

#[tokio::test]
async fn files_stream_through_the_data_plane_both_ways() {
    let h = harness!();
    let mut w = FakeWorker::connect_machine_worker(&h, "w-files").await.unwrap();
    let spec = running_machine(&h, &mut w, serde_json::json!({})).await;
    let id = spec.machine_id;

    // Read.
    let mut ds = FakeDataSocket::offer(&h, "w-files").await.unwrap();
    let base = h.base.clone();
    let read = tokio::spawn(async move {
        let r = reqwest::get(format!("{base}/v1/machines/{id}/files?path=/home/u/a.txt&max_bytes=100"))
            .await
            .unwrap();
        (r.status().as_u16(), r.bytes().await.unwrap().to_vec())
    });
    let header = ds.header().await;
    assert_eq!(header.target, StreamTarget::FileRead { path: "/home/u/a.txt".into(), max_bytes: Some(100) });
    ds.msg(&DataMsg::Ready).await;
    ds.bytes(b"hello ").await;
    ds.bytes(b"world").await;
    ds.msg(&DataMsg::Eof).await;
    let (status, body) = read.await.unwrap();
    assert_eq!(status, 200);
    assert_eq!(body, b"hello world");

    // A missing file is the worker's clean 404, not a broken stream.
    let mut ds = FakeDataSocket::offer(&h, "w-files").await.unwrap();
    let base = h.base.clone();
    let missing = tokio::spawn(async move {
        reqwest::get(format!("{base}/v1/machines/{id}/files?path=/nope")).await.unwrap().status().as_u16()
    });
    ds.header().await;
    ds.msg(&DataMsg::Error { status: 404, message: "/nope does not exist".into() }).await;
    assert_eq!(missing.await.unwrap(), 404);

    // Write.
    let mut ds = FakeDataSocket::offer(&h, "w-files").await.unwrap();
    let base = h.base.clone();
    let write = tokio::spawn(async move {
        reqwest::Client::new()
            .put(format!("{base}/v1/machines/{id}/files?path=/home/u/b.sh&mode=0755"))
            .body("#!/bin/sh\n")
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    });
    let header = ds.header().await;
    assert_eq!(header.target, StreamTarget::FileWrite { path: "/home/u/b.sh".into(), mode: 0o755 });
    ds.msg(&DataMsg::Ready).await;
    assert_eq!(ds.read_to_eof().await, b"#!/bin/sh\n");
    ds.msg(&DataMsg::Done).await;
    assert_eq!(write.await.unwrap(), 204);

    // Traversal is refused before anything reaches the worker.
    let (status, _) = h.get(&format!("/v1/machines/{id}/files?path=/home/../etc/shadow")).await;
    assert_eq!(status, 400);
}

/// A capability link reaches exactly one port, carries no bearer, and the
/// proxied request never sees the caller's credentials.
#[tokio::test]
async fn links_proxy_one_port_and_strip_credentials() {
    let h = harness!(Opts { auth_required: true, ..Default::default() });
    let mut w = FakeWorker::connect_machine_worker(&h, "w-link").await.unwrap();
    let spec = running_machine(&h, &mut w, serde_json::json!({"expose": [6080]})).await;
    let id = spec.machine_id;

    let (status, _) = h.post(&format!("/v1/machines/{id}/links"), serde_json::json!({"port": 6081})).await;
    assert_eq!(status, 403, "an unexposed port cannot be linked");
    let (status, link) = h
        .post(
            &format!("/v1/machines/{id}/links"),
            serde_json::json!({"port": 6080, "path": "/embed.html", "query": "view_only=1"}),
        )
        .await;
    assert_eq!(status, 200, "{link}");
    let url = link["url"].as_str().unwrap();
    assert!(url.starts_with("http://links.test/v1/links/") && url.ends_with("/embed.html?view_only=1"), "{url}");
    let local = url.replace("http://links.test", &h.base);

    let mut ds = FakeDataSocket::offer(&h, "w-link").await.unwrap();
    let fetch = tokio::spawn(async move {
        // No bearer at all: the capability is the credential.
        let r = reqwest::Client::new()
            .get(&local)
            .header("cookie", "session=dashboard")
            .send()
            .await
            .unwrap();
        (r.status().as_u16(), r.headers().get("set-cookie").is_some(), r.text().await.unwrap())
    });
    let header = ds.header().await;
    assert_eq!(header.target, StreamTarget::Port { port: 6080 });
    ds.msg(&DataMsg::Ready).await;
    let head = String::from_utf8(ds.read_until(b"\r\n\r\n").await).unwrap();
    assert!(head.starts_with("GET /embed.html?view_only=1 HTTP/1.1"), "{head}");
    assert!(!head.to_ascii_lowercase().contains("cookie:"), "cookies must not reach the guest: {head}");
    ds.bytes(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nSet-Cookie: evil=1\r\n\r\nnoVNC").await;
    ds.msg(&DataMsg::Eof).await;
    let (status, set_cookie, body) = fetch.await.unwrap();
    assert_eq!(status, 200);
    assert_eq!(body, "noVNC");
    assert!(!set_cookie, "a guest cannot set cookies on the control plane's origin");

    // A forged capability is a flat 404.
    let forged = url.replace("http://links.test", &h.base).replace(".6080.", ".6081.");
    let status = reqwest::get(&forged).await.unwrap().status().as_u16();
    assert_eq!(status, 404);
}

/// `…/ports/6080/` and `…/links/{cap}/` -- the directory form noVNC's own
/// URLs use -- reach the guest's root, not a 404. Found on the KVM run.
#[tokio::test]
async fn the_trailing_slash_forms_reach_the_guest_root() {
    let h = harness!();
    let mut w = FakeWorker::connect_machine_worker(&h, "w-slash").await.unwrap();
    let spec = running_machine(&h, &mut w, serde_json::json!({"expose": [6080]})).await;
    let id = spec.machine_id;
    let (_, link) = h.post(&format!("/v1/machines/{id}/links"), serde_json::json!({"port": 6080})).await;
    let link = link["url"].as_str().unwrap().replace("http://links.test", &h.base);
    assert!(link.ends_with('/'), "{link}");

    for url in [format!("{}/v1/machines/{id}/ports/6080/", h.base), link] {
        let mut ds = FakeDataSocket::offer(&h, "w-slash").await.unwrap();
        let fetch = tokio::spawn(async move { reqwest::get(url).await.unwrap().status().as_u16() });
        ds.header().await;
        ds.msg(&DataMsg::Ready).await;
        let head = String::from_utf8(ds.read_until(b"\r\n\r\n").await).unwrap();
        assert!(head.starts_with("GET / HTTP/1.1"), "{head}");
        ds.bytes(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n").await;
        ds.msg(&DataMsg::Eof).await;
        assert_eq!(fetch.await.unwrap(), 200);
    }
}

/// The authenticated port proxy strips the caller's bearer before the
/// request reaches whatever runs in the guest.
#[tokio::test]
async fn the_port_proxy_never_forwards_the_bearer() {
    let h = harness!(Opts { auth_required: true, ..Default::default() });
    let mut w = FakeWorker::connect_machine_worker(&h, "w-port").await.unwrap();
    let spec = running_machine(&h, &mut w, serde_json::json!({"expose": [7070]})).await;
    let id = spec.machine_id;
    let mut ds = FakeDataSocket::offer(&h, "w-port").await.unwrap();
    let (base, key) = (h.base.clone(), h.key.clone().unwrap());
    let call = tokio::spawn(async move {
        reqwest::Client::new()
            .post(format!("{base}/v1/machines/{id}/ports/7070/v1/desktop?api_key={key}&x=1"))
            .bearer_auth(&key)
            .header("x-guest-authorization", "Bearer guest-control-token")
            .body("{}")
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    });
    ds.header().await;
    ds.msg(&DataMsg::Ready).await;
    let head = String::from_utf8(ds.read_until(b"\r\n\r\n").await).unwrap();
    assert!(head.starts_with("POST /v1/desktop?x=1 HTTP/1.1"), "{head}");
    assert!(!head.contains("pkc_"), "the api key leaked into the guest: {head}");
    // The guest's own bearer arrives as its Authorization, and is the only one.
    let lower = head.to_ascii_lowercase();
    assert_eq!(lower.matches("authorization:").count(), 1, "{head}");
    assert!(lower.contains("authorization: bearer guest-control-token"), "{head}");
    assert!(!lower.contains("x-guest-authorization"), "{head}");
    ds.bytes(b"HTTP/1.1 204 No Content\r\n\r\n").await;
    ds.msg(&DataMsg::Eof).await;
    assert_eq!(call.await.unwrap(), 204);
}

#[tokio::test]
async fn destroying_a_machine_tells_its_worker_and_frees_the_external_id() {
    let h = harness!();
    let mut w = FakeWorker::connect_machine_worker(&h, "w-del").await.unwrap();
    let spec = running_machine(&h, &mut w, serde_json::json!({"external_id": "gone"})).await;
    let res = reqwest::Client::new()
        .delete(format!("{}/v1/machines/{}", h.base, spec.machine_id))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 204);
    let told = w
        .next_matching(|d| match d {
            Down::DestroyMachine { machine_id, .. } => Some(machine_id),
            _ => None,
        })
        .await;
    assert_eq!(told, Some(spec.machine_id));
    machine_state_is(&h, spec.machine_id, "destroyed").await;

    // The key is free again: a new machine, not the destroyed one.
    let (status, again) = h.post("/v1/machines", serde_json::json!({"external_id": "gone"})).await;
    assert_eq!(status, 200, "{again}");
    assert_ne!(again["machine"]["id"], spec.machine_id.to_string());
}

// ------------------------------------------------------- fail-fast placement

/// `POST /v1/machines`, keeping the headers `Harness::post` drops.
async fn create_raw(h: &Harness, body: serde_json::Value) -> (u16, Option<String>, serde_json::Value) {
    let res = reqwest::Client::new()
        .post(format!("{}/v1/machines", h.base))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = res.status().as_u16();
    let retry = res.headers().get("retry-after").and_then(|v| v.to_str().ok()).map(str::to_string);
    (status, retry, res.json().await.unwrap_or(serde_json::Value::Null))
}

/// Nothing to run it on: refused at once, whatever `wait_s` says, and
/// nothing is left behind to hold a quota slot or the external_id.
#[tokio::test]
async fn a_machine_nothing_can_run_is_refused_at_once_and_not_created() {
    let h = harness!();
    let started = std::time::Instant::now();
    let (status, retry, body) = create_raw(&h, serde_json::json!({"external_id": "ff-none", "wait_s": 120})).await;
    assert_eq!(status, 503, "{body}");
    assert_eq!(body["error"]["reason"], "no_workers", "{body}");
    assert!(body["error"]["message"].as_str().is_some_and(|m| !m.is_empty()), "{body}");
    assert_eq!(body["error"]["retry_after_s"], 30);
    assert_eq!(retry.as_deref(), Some("30"));
    assert!(started.elapsed() < std::time::Duration::from_secs(5), "wait_s must not be waited out");
    let (_, list) = h.get("/v1/machines?external_id=ff-none").await;
    assert_eq!(list.as_array().map(Vec::len), Some(0), "{list}");
}

/// The old behaviour, on request: queued until a worker can take it, and
/// saying why it waits.
#[tokio::test]
async fn queue_true_keeps_a_machine_waiting_for_a_worker() {
    let h = harness!();
    let (status, body) = h.post("/v1/machines", serde_json::json!({"external_id": "ff-queue", "queue": true})).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["machine"]["state"], "scheduled");
    assert_eq!(body["machine"]["reason"], "no_workers");
    let mut w = FakeWorker::connect_machine_worker(&h, "w-late").await.unwrap();
    let spec = w.next_machine_assignment().await.expect("assigned once a worker appears");
    assert_eq!(spec.machine_id.to_string(), body["machine"]["id"].as_str().unwrap());
}

/// Bigger than any host could ever hold is a 422, not a wait.
#[tokio::test]
async fn a_machine_larger_than_any_worker_is_refused_as_too_large() {
    let h = harness!();
    let host = HostReport { max_slots: Some(1), cores: Some(4), ..Default::default() };
    let mut w = FakeWorker::connect_machine_worker_with(&h, "w-small", 1, Some(host)).await.unwrap();
    let (status, body) = h.post("/v1/machines", serde_json::json!({"memory_mib": 4096})).await;
    assert_eq!(status, 422, "{body}");
    assert_eq!(body["error"]["reason"], "too_large");
    assert_eq!(body["error"]["detail"]["max_slots"], 1);
    let got = tokio::time::timeout(std::time::Duration::from_millis(500), w.next_machine_assignment()).await;
    assert!(matches!(got, Err(_) | Ok(None)), "nothing was assigned");
}

/// Full right now is a 503 with a Retry-After, saying how close it came.
#[tokio::test]
async fn a_full_fleet_is_refused_with_a_retry_after() {
    let h = harness!();
    let mut w = FakeWorker::connect_machine_worker_with(&h, "w-full", 2, None).await.unwrap();
    let (status, first) = h.post("/v1/machines", serde_json::json!({"memory_mib": 4096})).await;
    assert_eq!(status, 200, "the first takes both slots: {first}");
    w.next_machine_assignment().await.expect("assigned");
    let (status, retry, body) = create_raw(&h, serde_json::json!({"memory_mib": 2048})).await;
    assert_eq!(status, 503, "{body}");
    assert_eq!(body["error"]["reason"], "capacity_full");
    assert_eq!(body["error"]["detail"]["best_free_slots"], 0);
    assert_eq!(retry.as_deref(), Some("15"));
}

/// The worker a boot goes to fails it: the caller hears so at once, with
/// the worker's reason, instead of after `wait_s`.
#[tokio::test]
async fn a_failed_boot_returns_at_once_with_the_workers_reason() {
    let h = harness!();
    let mut w = FakeWorker::connect_machine_worker(&h, "w-fail").await.unwrap();
    let base = h.base.clone();
    let started = std::time::Instant::now();
    let call = tokio::spawn(async move {
        let res = reqwest::Client::new()
            .post(format!("{base}/v1/machines"))
            .json(&serde_json::json!({"wait_s": 120}))
            .send()
            .await
            .unwrap();
        (res.status().as_u16(), res.json::<serde_json::Value>().await.unwrap())
    });
    let spec = w.next_machine_assignment().await.expect("assigned");
    w.machine_state(&spec, MS::Booting, false).await;
    w.machine_failed(&spec, "image_not_staged", "image x is not staged for cloud_hypervisor").await;
    let (status, body) = call.await.unwrap();
    assert!(started.elapsed() < std::time::Duration::from_secs(20), "{:?}", started.elapsed());
    assert_eq!(status, 502, "{body}");
    assert_eq!(body["error"]["reason"], "boot_failed");
    assert_eq!(body["error"]["detail"]["worker_reason"], "image_not_staged");
    assert_eq!(body["machine"]["state"], "failed");
    assert_eq!(body["machine"]["reason"], "image_not_staged");
}

/// A Cloud Hypervisor machine only goes where its disk is staged, and is
/// refused before anything is assigned when nowhere has it.
#[tokio::test]
async fn a_cloud_hypervisor_machine_needs_its_image_staged_somewhere() {
    let h = harness!();
    let bare = HostReport { images: Some(vec![]), ..Default::default() };
    let mut w1 = FakeWorker::connect_machine_worker_with(&h, "w-bare", 8, Some(bare)).await.unwrap();
    let body = serde_json::json!({"engine": "cloud_hypervisor", "image": "pukubot-computer:latest"});
    let (status, refused) = h.post("/v1/machines", body.clone()).await;
    assert_eq!(status, 422, "{refused}");
    assert_eq!(refused["error"]["reason"], "image_not_staged");
    assert!(
        refused["error"]["message"].as_str().unwrap().contains("build-ch-rootfs.sh pukubot-computer:latest"),
        "{refused}"
    );

    let staged = HostReport {
        images: Some(vec![StagedImage {
            engine: puku_cloud_proto::Engine::CloudHypervisor,
            key: puku_cloud_proto::machine::image_key("pukubot-computer:latest"),
            image: Some("pukubot-computer:latest".into()),
            digest: None,
            size_mib: None,
        }]),
        ..Default::default()
    };
    let mut w2 = FakeWorker::connect_machine_worker_with(&h, "w-staged", 8, Some(staged)).await.unwrap();
    let (status, created) = h.post("/v1/machines", body).await;
    assert_eq!(status, 200, "{created}");
    assert!(w2.next_machine_assignment().await.is_some(), "the worker with the disk gets it");
    let got = tokio::time::timeout(std::time::Duration::from_millis(300), w1.next_machine_assignment()).await;
    assert!(matches!(got, Err(_) | Ok(None)), "the worker without it never does");
}

/// The volume is on one worker. While that worker is away a start is
/// refused, saying how long it has left, instead of hanging or booting an
/// empty volume on the idle worker next to it.
#[tokio::test]
async fn starting_a_machine_whose_volume_host_is_offline_says_so() {
    let h = harness!();
    let mut w = FakeWorker::connect_machine_worker(&h, "w-vol").await.unwrap();
    let spec = running_machine(&h, &mut w, serde_json::json!({"volume": {"path": "/home/u"}})).await;
    h.post(&format!("/v1/machines/{}/stop", spec.machine_id), serde_json::json!({})).await;
    w.machine_state(&spec, MS::Stopped, false).await;
    machine_state_is(&h, spec.machine_id, "stopped").await;
    drop(w);
    for _ in 0..200 {
        if h.state.workers.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    let _other = FakeWorker::connect_machine_worker(&h, "w-other").await.unwrap();
    let (status, body) =
        h.post(&format!("/v1/machines/{}/start", spec.machine_id), serde_json::json!({"wait_s": 30})).await;
    assert_eq!(status, 503, "{body}");
    assert_eq!(body["error"]["reason"], "volume_host_offline");
    assert!(body["error"]["detail"]["grace_left_s"].as_i64().unwrap() > 0, "{body}");
    assert_eq!(body["machine"]["state"], "stopped", "left as it was: {body}");
    assert_eq!(body["machine"]["reason"], "volume_host_offline");
}

// ---------------------------------------------------------------- snapshots

use puku_cloud_proto::snapshot::{SnapshotLayer, SnapshotOrder, SnapshotTrigger};

/// A deployment that takes snapshots, onto an in-process S3.
async fn snap_harness() -> Option<Harness> {
    crate::harness::start_with(crate::harness::Opts { snapshots: true, ..Default::default() }).await
}

async fn await_snapshot(h: &Harness, machine: Uuid, sid: Uuid, want: &str) -> serde_json::Value {
    for _ in 0..300 {
        let (_, s) = h.get(&format!("/v1/machines/{machine}/snapshots/{sid}")).await;
        if s["state"] == want {
            return s;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let (_, s) = h.get(&format!("/v1/machines/{machine}/snapshots/{sid}")).await;
    panic!("snapshot {sid} never reached {want}: {s}");
}

/// Stop a running machine and take the snapshot the stop carries, as the
/// worker would.
async fn stop_with_snapshot(
    h: &Harness,
    w: &mut FakeWorker,
    spec: &puku_cloud_proto::machine::MachineSpec,
) -> SnapshotOrder {
    let (status, stopped) = h.post(&format!("/v1/machines/{}/stop", spec.machine_id), serde_json::json!({})).await;
    assert_eq!(status, 202, "{stopped}");
    let order = w
        .next_matching(|d| match d {
            Down::StopMachine { snapshot: Some(o), .. } => Some(o),
            _ => None,
        })
        .await
        .expect("the stop carries a snapshot order");
    w.machine_state(spec, MS::Stopped, false).await;
    w.upload_snapshot(&order, b"sealed layer bytes").await;
    await_snapshot(h, spec.machine_id, order.snapshot_id, "ready").await;
    order
}

fn with_snapshots() -> serde_json::Value {
    serde_json::json!({"volume": {"path": "/home/u"}, "snapshots": {"on_stop": true}})
}

/// The policy's snapshot rides on the stop; the worker uploads through
/// presigned URLs; controld assembles the object and makes it the latest.
#[tokio::test]
async fn a_stop_takes_the_snapshot_the_policy_asks_for() {
    let Some(h) = snap_harness().await else { return };
    let mut w = FakeWorker::connect_snapshot_worker(&h, "w-snap").await.unwrap();
    let spec = running_machine(&h, &mut w, with_snapshots()).await;
    let (status, stopped) = h.post(&format!("/v1/machines/{}/stop", spec.machine_id), serde_json::json!({})).await;
    assert_eq!(status, 202);
    assert_eq!(stopped["snapshot"]["state"], "pending", "{stopped}");
    let order = w
        .next_matching(|d| match d {
            Down::StopMachine { snapshot: Some(o), .. } => Some(o),
            _ => None,
        })
        .await
        .expect("the stop carries a snapshot order");
    assert_eq!(order.trigger, SnapshotTrigger::Stop);
    assert_eq!(order.layers.len(), 1, "a volume and no persistent root");
    assert!(order.key_hex.len() == 64 && order.previous.is_none());

    // Another worker cannot upload into it.
    let mut intruder = FakeWorker::connect_snapshot_worker(&h, "w-intruder").await.unwrap();
    intruder
        .send(Up::RequestSnapshotUrls {
            snapshot_id: order.snapshot_id,
            layer: SnapshotLayer::Volume,
            first_part: 1,
            count: 1,
        })
        .await
        .unwrap();
    let refused = intruder
        .next_matching(|d| match d {
            Down::SnapshotUrls { error, urls, .. } => Some((error, urls)),
            _ => None,
        })
        .await
        .unwrap();
    assert!(refused.1.is_empty() && refused.0.unwrap().contains("not ordered from this worker"));

    w.machine_state(&spec, MS::Stopped, false).await;
    w.upload_snapshot(&order, b"sealed layer bytes").await;
    let snap = await_snapshot(&h, spec.machine_id, order.snapshot_id, "ready").await;
    assert_eq!(snap["consistency"], "clean");
    assert_eq!(snap["layers"][0]["layer"], "volume");
    assert!(!snap.to_string().contains(&order.key_hex), "the data key never comes back out");
    let stored = h.s3.as_ref().unwrap().get(&order.layers[0].key).expect("the object was assembled");
    assert_eq!(stored, b"sealed layer bytes");
    let (_, m) = h.get(&format!("/v1/machines/{}", spec.machine_id)).await;
    assert_eq!(m["latest_snapshot_id"], order.snapshot_id.to_string());
    let (_, list) = h.get(&format!("/v1/machines/{}/snapshots", spec.machine_id)).await;
    assert_eq!(list.as_array().map(Vec::len), Some(1));
}

/// A worker from before snapshots never gets an order it would drop.
#[tokio::test]
async fn a_worker_without_snapshots_is_never_sent_an_order() {
    let Some(h) = snap_harness().await else { return };
    let mut w = FakeWorker::connect_machine_worker(&h, "w-old").await.unwrap();
    let spec = running_machine(&h, &mut w, with_snapshots()).await;
    h.post(&format!("/v1/machines/{}/stop", spec.machine_id), serde_json::json!({})).await;
    let carried = w
        .next_matching(|d| match d {
            Down::StopMachine { snapshot, .. } => Some(snapshot),
            _ => None,
        })
        .await
        .unwrap();
    assert!(carried.is_none());
    w.machine_state(&spec, MS::Stopped, false).await;
    machine_state_is(&h, spec.machine_id, "stopped").await;
    let (status, body) = h.post(&format!("/v1/machines/{}/snapshots", spec.machine_id), serde_json::json!({})).await;
    assert_eq!(status, 422, "{body}");
    assert_eq!(body["error"]["reason"], "snapshots_unsupported");
}

/// The worker holding the volume is gone. A start is refused -- saying a
/// snapshot could bring it back -- until the caller asks to relocate; then
/// another worker restores it, and only that worker may read the snapshot.
#[tokio::test]
async fn a_lost_worker_s_machine_is_relocated_from_its_snapshot() {
    let Some(h) = snap_harness().await else { return };
    let mut a = FakeWorker::connect_snapshot_worker(&h, "w-lost").await.unwrap();
    let spec = running_machine(&h, &mut a, with_snapshots()).await;
    let order = stop_with_snapshot(&h, &mut a, &spec).await;
    drop(a);
    for _ in 0..200 {
        if h.state.workers.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    let mut b = FakeWorker::connect_snapshot_worker(&h, "w-new-home").await.unwrap();
    let start = format!("/v1/machines/{}/start", spec.machine_id);

    let (status, body) = h.post(&start, serde_json::json!({})).await;
    assert_eq!(status, 503, "{body}");
    assert_eq!(body["error"]["reason"], "volume_host_offline");
    assert_eq!(body["error"]["detail"]["snapshot_available"], true, "{body}");

    let (status, body) = h.post(&start, serde_json::json!({"relocate": true})).await;
    assert_eq!(status, 200, "{body}");
    let restored = b.next_machine_assignment().await.expect("the new worker gets the machine");
    assert_eq!(restored.machine_id, spec.machine_id);
    assert_eq!(restored.generation, spec.generation + 1, "the restore is the next boot; a stop is not one");
    let restore = restored.restore.clone().expect("with a restore order");
    assert_eq!(restore.snapshot_id, order.snapshot_id);
    assert_eq!(restore.layers[0].key, order.layers[0].key);
    assert_eq!(restore.layers[0].sha256.as_deref(), Some("ab".repeat(32).as_str()));
    assert_eq!(restore.key_hex, order.key_hex, "the same data key opens it");

    // Only the worker restoring it may read it.
    let mut other = FakeWorker::connect_snapshot_worker(&h, "w-bystander").await.unwrap();
    other
        .send(Up::RequestSnapshotGet { snapshot_id: order.snapshot_id, layer: SnapshotLayer::Volume })
        .await
        .unwrap();
    let refused = other
        .next_matching(|d| match d {
            Down::SnapshotGetUrl { url, .. } => Some(url),
            _ => None,
        })
        .await
        .unwrap();
    assert!(refused.is_none(), "a bystander is refused");

    b.machine_state(&restored, MS::Restoring, false).await;
    machine_state_is(&h, spec.machine_id, "restoring").await;
    b.send(Up::RequestSnapshotGet { snapshot_id: order.snapshot_id, layer: SnapshotLayer::Volume })
        .await
        .unwrap();
    let url = b
        .next_matching(|d| match d {
            Down::SnapshotGetUrl { url, .. } => Some(url),
            _ => None,
        })
        .await
        .unwrap()
        .expect("the restoring worker gets a URL");
    let bytes = reqwest::get(&url).await.unwrap().bytes().await.unwrap();
    assert_eq!(&bytes[..], b"sealed layer bytes");
    b.machine_state(&restored, MS::Running, true).await;
    machine_state_is(&h, spec.machine_id, "running").await;
    let (_, m) = h.get(&format!("/v1/machines/{}", spec.machine_id)).await;
    assert_eq!(m["restored_from"], order.snapshot_id.to_string(), "{m}");
}

/// A purge deletes the snapshots with the machine, objects included.
#[tokio::test]
async fn destroying_with_purge_deletes_the_snapshots_objects() {
    let Some(h) = snap_harness().await else { return };
    let mut w = FakeWorker::connect_snapshot_worker(&h, "w-purge").await.unwrap();
    let spec = running_machine(&h, &mut w, with_snapshots()).await;
    let order = stop_with_snapshot(&h, &mut w, &spec).await;
    let s3 = h.s3.clone().unwrap();
    assert_eq!(s3.keys(), vec![order.layers[0].key.clone()]);

    let res = reqwest::Client::new()
        .delete(format!("{}/v1/machines/{}?purge=true", h.base, spec.machine_id))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 204);
    let cfg = h.state.cfg.snapshots.clone().unwrap();
    crate::snapshots::sweep(&h.state, &cfg).await.unwrap();
    assert!(s3.keys().is_empty(), "the objects are gone: {:?}", s3.keys());
    let (status, _) = h.get(&format!("/v1/machines/{}/snapshots/{}", spec.machine_id, order.snapshot_id)).await;
    assert_eq!(status, 404);
}

/// Retention keeps a machine's newest `keep` and deletes the rest.
#[tokio::test]
async fn retention_keeps_the_newest_snapshots() {
    let Some(h) = snap_harness().await else { return };
    let mut w = FakeWorker::connect_snapshot_worker(&h, "w-keep").await.unwrap();
    let body = serde_json::json!({"volume": {"path": "/home/u"}, "snapshots": {"keep": 1}});
    let spec = running_machine(&h, &mut w, body).await;
    let mut orders = Vec::new();
    for _ in 0..2 {
        let (status, snap) =
            h.post(&format!("/v1/machines/{}/snapshots", spec.machine_id), serde_json::json!({})).await;
        assert_eq!(status, 202, "{snap}");
        let order = w
            .next_matching(|d| match d {
                Down::SnapshotMachine { order } => Some(order),
                _ => None,
            })
            .await
            .expect("the worker is told to snapshot");
        assert_eq!(order.trigger, SnapshotTrigger::Manual);
        w.upload_snapshot(&order, order.snapshot_id.as_bytes()).await;
        await_snapshot(&h, spec.machine_id, order.snapshot_id, "ready").await;
        orders.push(order);
    }
    let cfg = h.state.cfg.snapshots.clone().unwrap();
    crate::snapshots::sweep(&h.state, &cfg).await.unwrap();
    let (status, _) = h.get(&format!("/v1/machines/{}/snapshots/{}", spec.machine_id, orders[0].snapshot_id)).await;
    assert_eq!(status, 404, "the older one is deleted");
    await_snapshot(&h, spec.machine_id, orders[1].snapshot_id, "ready").await;
    assert_eq!(h.s3.as_ref().unwrap().keys(), vec![orders[1].layers[0].key.clone()]);
}

/// Reconnect reconciliation: a VM the worker no longer has is marked stopped
/// with its volume intact, and an assignment that died with the link is
/// requeued.
#[tokio::test]
async fn a_reconnecting_worker_reconciles_its_machines() {
    let h = harness!();
    let mut w = FakeWorker::connect_machine_worker(&h, "w-recon").await.unwrap();
    let spec = running_machine(&h, &mut w, serde_json::json!({})).await;
    drop(w);
    for _ in 0..200 {
        if h.state.workers.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    // Back, with nothing running (the box rebooted).
    let _w = FakeWorker::connect_machine_worker(&h, "w-recon").await.unwrap();
    machine_state_is(&h, spec.machine_id, "stopped").await;
    let (_, m) = h.get(&format!("/v1/machines/{}", spec.machine_id)).await;
    assert!(m["error"].as_str().unwrap_or_default().contains("lost"), "{m}");
}

// --- Host liveness leases --------------------------------------------------

type LeaseRow = (i64, String, chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>, bool);

/// (generation, state, last_renewed_at, expires_at, expired) for a worker's lease.
async fn lease_of(h: &Harness, worker: &str) -> Option<LeaseRow> {
    sqlx::query_as(
        "SELECT l.generation, l.state, l.last_renewed_at, l.expires_at, l.expires_at <= now() \
         FROM leases l JOIN workers w ON w.id = l.host_id WHERE w.name = $1",
    )
    .bind(worker)
    .fetch_optional(&h.pool)
    .await
    .unwrap()
}

async fn until_workers_gone(h: &Harness) {
    for _ in 0..200 {
        if h.state.workers.is_empty() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("worker never left the registry");
}

#[tokio::test]
async fn a_lease_worker_is_given_a_lease_that_its_frames_renew() {
    let h = harness!();
    let mut w = FakeWorker::connect_lease_worker(&h, "w-lease").await.unwrap();
    let (gen, state, first, _, _) = lease_of(&h, "w-lease").await.expect("lease taken over on register");
    assert_eq!((gen, state.as_str()), (1, "held"));
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    w.send(Up::LeaseRenew).await.unwrap();
    for _ in 0..100 {
        let (g, _, renewed, _, _) = lease_of(&h, "w-lease").await.unwrap();
        if renewed > first {
            assert_eq!(g, 1, "renewal keeps the generation");
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("the renew frame never reached the lease row");
}

#[tokio::test]
async fn a_dropped_link_expires_the_lease_at_once_and_a_reconnect_takes_it_over() {
    let h = harness!();
    let w = FakeWorker::connect_lease_worker(&h, "w-drop").await.unwrap();
    drop(w);
    until_workers_gone(&h).await;
    // Expired now, not after the 3 s TTL: the next sweep suspects it.
    let (_, state, _, _, expired) = lease_of(&h, "w-drop").await.unwrap();
    assert_eq!(state, "held");
    assert!(expired, "a dropped link must expire the lease immediately");

    let _w = FakeWorker::connect_lease_worker(&h, "w-drop").await.unwrap();
    let (gen, state, _, _, expired) = lease_of(&h, "w-drop").await.unwrap();
    assert_eq!((gen, state.as_str(), expired), (2, "held", false), "a new generation per registration");
}

#[tokio::test]
async fn a_worker_without_the_lease_feature_has_no_lease() {
    let h = harness!();
    let _w = FakeWorker::connect_machine_worker(&h, "w-old").await.unwrap();
    assert!(lease_of(&h, "w-old").await.is_none(), "nobody renews it, so nobody may declare it dead");
}

/// The sweeper's whole path against Postgres: suspected, then dead, with
/// both timestamps persisted, and a dead host shut out until it re-registers.
#[tokio::test]
async fn the_sweeper_suspects_then_declares_dead_and_persists_both() {
    use puku_leases::{LeaseError, LeaseService, LeaseServiceImpl, LeaseSweeper};
    let h = harness!();
    let store = std::sync::Arc::new(crate::leases::PgLeaseStore { pool: h.pool.clone() });
    let svc = std::sync::Arc::new(LeaseServiceImpl::new(store.clone(), "controld-a"));
    let bmc = std::sync::Arc::new(puku_leases::bmc_probe::BmcProbeStub);
    let sweeper = LeaseSweeper::new(store.clone(), svc.clone(), bmc);
    let mut hosts = Vec::new();
    for _ in 0..4 {
        let id = Uuid::new_v4();
        svc.takeover(id, "controld-a").await.unwrap();
        hosts.push(id);
    }
    let dead = hosts[0];
    let lease = svc.lookup(dead).await.unwrap().unwrap();

    sqlx::query("UPDATE leases SET expires_at = now() - interval '4 seconds' WHERE host_id = $1")
        .bind(dead)
        .execute(&h.pool)
        .await
        .unwrap();
    let r = sweeper.sweep_once().await.unwrap();
    assert_eq!(r.newly_suspected, vec![dead]);
    assert!(r.newly_dead.is_empty());
    let (state, suspected): (String, Option<chrono::DateTime<chrono::Utc>>) =
        sqlx::query_as("SELECT state, suspected_at FROM leases WHERE host_id = $1")
            .bind(dead)
            .fetch_one(&h.pool)
            .await
            .unwrap();
    assert_eq!(state, "suspected");
    assert!(suspected.is_some(), "suspected_at is persisted");

    sqlx::query("UPDATE leases SET suspected_at = now() - interval '20 seconds' WHERE host_id = $1")
        .bind(dead)
        .execute(&h.pool)
        .await
        .unwrap();
    let r = sweeper.sweep_once().await.unwrap();
    assert_eq!(r.newly_dead, vec![dead]);
    let (state, confirmed): (String, Option<chrono::DateTime<chrono::Utc>>) =
        sqlx::query_as("SELECT state, confirmed_dead_at FROM leases WHERE host_id = $1")
            .bind(dead)
            .fetch_one(&h.pool)
            .await
            .unwrap();
    assert_eq!(state, "released");
    assert!(confirmed.is_some(), "confirmed_dead_at is persisted");

    assert!(matches!(svc.renew(&lease).await, Err(LeaseError::Dead(_))), "a dead host cannot just renew");
    let back = svc.takeover(dead, "controld-a").await.unwrap();
    assert_eq!(back.generation, lease.generation + 1);
    let (state, suspected, confirmed): (String, Option<chrono::DateTime<chrono::Utc>>, Option<chrono::DateTime<chrono::Utc>>) =
        sqlx::query_as("SELECT state, suspected_at, confirmed_dead_at FROM leases WHERE host_id = $1")
            .bind(dead)
            .fetch_one(&h.pool)
            .await
            .unwrap();
    assert_eq!((state.as_str(), suspected, confirmed), ("held", None, None));
}

/// Two controld instances: only one sweeps, and when it goes away the
/// other takes over.
#[tokio::test]
async fn only_one_instance_holds_the_sweeper_lock() {
    let h = harness!();
    let mut a = crate::leases::SweepLock::new();
    let mut b = crate::leases::SweepLock::new();
    assert!(a.hold(&h.pool).await, "first instance takes the lock");
    assert!(a.hold(&h.pool).await, "and keeps it");
    assert!(!b.hold(&h.pool).await, "second instance does not sweep");
    drop(a); // the instance dies; its session closes
    for _ in 0..100 {
        if b.hold(&h.pool).await {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("the lock never passed to the surviving instance");
}
