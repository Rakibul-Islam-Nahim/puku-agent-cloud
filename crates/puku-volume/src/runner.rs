//! How the RBD backend runs `rbd` and `ceph`.
//!
//! Every Ceph operation is a CLI call. Routing them through one trait lets
//! the unit tests assert the exact command lines (and script failures)
//! without a cluster, while production uses [`SystemRunner`].

use std::collections::VecDeque;
use std::sync::Mutex;

use async_trait::async_trait;

use crate::error::VolumeError;

/// What a finished command produced.
#[derive(Debug, Clone, Default)]
pub struct CmdOutput {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

impl CmdOutput {
    pub fn ok(stdout: impl Into<String>) -> Self {
        Self { status: 0, stdout: stdout.into(), stderr: String::new() }
    }
    pub fn fail(status: i32, stderr: impl Into<String>) -> Self {
        Self { status, stdout: String::new(), stderr: stderr.into() }
    }
    pub fn success(&self) -> bool {
        self.status == 0
    }
}

#[async_trait]
pub trait CommandRunner: Send + Sync {
    /// Run `program args...` to completion. `Err` only when the program
    /// could not be started; a non-zero exit is an `Ok(CmdOutput)` so the
    /// caller can decide which failures are benign (e.g. "already exists").
    async fn run(&self, program: &str, args: &[String]) -> Result<CmdOutput, VolumeError>;
}

/// Runs real processes.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemRunner;

#[async_trait]
impl CommandRunner for SystemRunner {
    async fn run(&self, program: &str, args: &[String]) -> Result<CmdOutput, VolumeError> {
        let out = tokio::process::Command::new(program)
            .args(args)
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|e| VolumeError::BackendUnavailable(format!("{program}: {e}")))?;
        Ok(CmdOutput {
            status: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
}

/// Test runner: records every call and answers from a script, in order.
/// A call with no scripted answer left succeeds with empty output.
#[derive(Debug, Default)]
pub struct ScriptedRunner {
    pub calls: Mutex<Vec<String>>,
    answers: Mutex<VecDeque<CmdOutput>>,
}

impl ScriptedRunner {
    pub fn new(answers: Vec<CmdOutput>) -> Self {
        Self { calls: Mutex::new(Vec::new()), answers: Mutex::new(answers.into()) }
    }

    /// Every call so far as `program arg arg ...`.
    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("runner poisoned").clone()
    }
}

#[async_trait]
impl CommandRunner for ScriptedRunner {
    async fn run(&self, program: &str, args: &[String]) -> Result<CmdOutput, VolumeError> {
        let line = std::iter::once(program.to_string()).chain(args.iter().cloned()).collect::<Vec<_>>().join(" ");
        self.calls.lock().expect("runner poisoned").push(line);
        Ok(self.answers.lock().expect("runner poisoned").pop_front().unwrap_or_default())
    }
}
