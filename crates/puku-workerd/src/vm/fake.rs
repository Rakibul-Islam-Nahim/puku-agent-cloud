//! An in-memory engine for tests: records what it was asked to do and boots
//! nothing.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use async_trait::async_trait;
use puku_cloud_proto::Engine;

use super::{ExecOutput, ExecRequest, GuestIo, StreamOutcome, Vm, VmBackend, VmSpec};

#[derive(Default)]
struct Shared {
    created: Mutex<Vec<VmSpec>>,
    execs: Mutex<Vec<(String, ExecRequest)>>,
    live: Mutex<Vec<String>>,
    removes_to_fail: AtomicU32,
    remove_attempts: AtomicU32,
}

pub struct FakeBackend {
    engine: Engine,
    shared: Arc<Shared>,
}

impl FakeBackend {
    pub fn new(engine: Engine) -> Self {
        FakeBackend { engine, shared: Arc::default() }
    }

    /// Fail the next `n` removes, as msb does while a stopped VM is still
    /// being released.
    pub fn fail_removes(&self, n: u32) {
        self.shared.removes_to_fail.store(n, Ordering::SeqCst);
    }

    pub fn remove_attempts(&self) -> u32 {
        self.shared.remove_attempts.load(Ordering::SeqCst)
    }

    pub fn created(&self) -> Vec<VmSpec> {
        self.shared.created.lock().unwrap().clone()
    }

    pub fn execs(&self) -> Vec<(String, ExecRequest)> {
        self.shared.execs.lock().unwrap().clone()
    }
}

struct FakeVm {
    name: String,
    shared: Arc<Shared>,
}

#[async_trait]
impl Vm for FakeVm {
    fn name(&self) -> &str {
        &self.name
    }

    async fn exec(&self, req: ExecRequest) -> Result<ExecOutput> {
        self.shared.execs.lock().unwrap().push((self.name.clone(), req));
        Ok(ExecOutput::default())
    }

    /// Echoes stdin to stdout, so a streaming round trip is observable.
    async fn exec_stream(
        &self,
        req: ExecRequest,
        stdin: Option<tokio::sync::mpsc::Receiver<bytes::Bytes>>,
        stdout: tokio::sync::mpsc::Sender<bytes::Bytes>,
    ) -> Result<StreamOutcome> {
        self.shared.execs.lock().unwrap().push((self.name.clone(), req));
        if let Some(mut rx) = stdin {
            while let Some(b) = rx.recv().await {
                let _ = stdout.send(b).await;
            }
        }
        Ok(StreamOutcome::default())
    }

    /// An echo server stands in for the guest's port.
    async fn connect_port(&self, _port: u16) -> Result<Box<dyn GuestIo>> {
        let (ours, theirs) = tokio::io::duplex(4096);
        tokio::spawn(async move {
            let (mut r, mut w) = tokio::io::split(theirs);
            let _ = tokio::io::copy(&mut r, &mut w).await;
        });
        Ok(Box::new(ours))
    }

    async fn stop(&self) -> Result<()> {
        self.shared.live.lock().unwrap().retain(|n| n != &self.name);
        Ok(())
    }
}

#[async_trait]
impl VmBackend for FakeBackend {
    fn engine(&self) -> Engine {
        self.engine
    }

    fn version(&self) -> String {
        "fake".into()
    }

    async fn create(&self, spec: &VmSpec) -> Result<Box<dyn Vm>> {
        self.shared.created.lock().unwrap().push(spec.clone());
        self.shared.live.lock().unwrap().push(spec.name.clone());
        Ok(Box::new(FakeVm { name: spec.name.clone(), shared: self.shared.clone() }))
    }

    async fn attach(&self, name: &str) -> Result<Box<dyn Vm>> {
        if !self.shared.live.lock().unwrap().iter().any(|n| n == name) {
            anyhow::bail!("sandbox gone after restart: {name}");
        }
        Ok(Box::new(FakeVm { name: name.to_string(), shared: self.shared.clone() }))
    }

    async fn remove(&self, name: &str) -> Result<()> {
        self.shared.remove_attempts.fetch_add(1, Ordering::SeqCst);
        let left = self.shared.removes_to_fail.load(Ordering::SeqCst);
        if left > 0 {
            self.shared.removes_to_fail.store(left - 1, Ordering::SeqCst);
            anyhow::bail!("sandbox {name} is still being released");
        }
        self.shared.live.lock().unwrap().retain(|n| n != name);
        Ok(())
    }

    async fn list(&self) -> Result<Vec<String>> {
        Ok(self.shared.live.lock().unwrap().clone())
    }

    async fn prepull(&self, _image: &str) -> Result<()> {
        Ok(())
    }
}
