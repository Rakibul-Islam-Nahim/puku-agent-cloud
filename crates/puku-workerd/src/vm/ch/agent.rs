//! Host side of `guest_proto`: talking to puku-guestd through Cloud
//! Hypervisor's hybrid vsock.
//!
//! Cloud Hypervisor exposes a guest's vsock as a unix socket on the host.
//! Connecting to it and writing `CONNECT <port>\n` opens a stream to that
//! guest port; the VMM answers `OK <n>\n` and then it is a plain byte pipe.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use bytes::Bytes;
use puku_cloud_proto::data_proto::MAX_EXEC_OUTPUT_BYTES;
use puku_cloud_proto::guest_proto::{header, kind, parse_header, GuestReply, GuestRequest, GUEST_AGENT_PORT};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

use crate::vm::{ExecOutput, ExecRequest, StreamOutcome, TIMEOUT_EXIT_CODE};

/// Open a stream to the guest agent.
pub async fn open(vsock: &Path) -> Result<UnixStream> {
    let mut s = UnixStream::connect(vsock)
        .await
        .with_context(|| format!("connecting to {}", vsock.display()))?;
    s.write_all(format!("CONNECT {GUEST_AGENT_PORT}\n").as_bytes()).await?;
    // Byte at a time: anything read past the newline belongs to the agent.
    let mut line = Vec::new();
    loop {
        let b = s.read_u8().await.context("the VMM closed the vsock handshake")?;
        if b == b'\n' {
            break;
        }
        line.push(b);
        if line.len() > 64 {
            bail!("vsock handshake answered with garbage");
        }
    }
    if !line.starts_with(b"OK") {
        bail!("vsock handshake refused: {}", String::from_utf8_lossy(&line));
    }
    Ok(s)
}

async fn write_frame(s: &mut (impl AsyncWriteExt + Unpin), k: u8, payload: &[u8]) -> Result<()> {
    s.write_all(&header(k, payload.len())).await?;
    s.write_all(payload).await?;
    Ok(())
}

async fn read_frame(s: &mut (impl AsyncReadExt + Unpin)) -> Result<Option<(u8, Vec<u8>)>> {
    let mut h = [0u8; 5];
    match s.read_exact(&mut h).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let (k, len) = parse_header(h).map_err(anyhow::Error::msg)?;
    let mut payload = vec![0u8; len];
    s.read_exact(&mut payload).await?;
    Ok(Some((k, payload)))
}

async fn send_request(s: &mut UnixStream, req: &GuestRequest) -> Result<()> {
    write_frame(s, kind::JSON, &serde_json::to_vec(req)?).await
}

fn parse_reply(payload: &[u8]) -> Result<GuestReply> {
    serde_json::from_slice(payload).context("a malformed reply from the guest agent")
}

/// A request with exactly one JSON reply.
pub async fn call(vsock: &Path, req: &GuestRequest) -> Result<GuestReply> {
    let mut s = open(vsock).await?;
    send_request(&mut s, req).await?;
    match read_frame(&mut s).await? {
        Some((kind::JSON, p)) => parse_reply(&p),
        _ => bail!("the guest agent closed the stream without answering"),
    }
}

/// Wait until the agent answers, or give up after `within`.
pub async fn wait_ready(vsock: &Path, within: Duration) -> Result<()> {
    let deadline = tokio::time::Instant::now() + within;
    let mut last = None;
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(2), call(vsock, &GuestRequest::Ping)).await {
            Ok(Ok(GuestReply::Pong { .. })) => return Ok(()),
            Ok(Ok(other)) => last = Some(format!("unexpected reply {other:?}")),
            Ok(Err(e)) => last = Some(format!("{e:#}")),
            Err(_) => last = Some("ping timed out".into()),
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    bail!(
        "the guest agent did not come up within {}s ({})",
        within.as_secs(),
        last.unwrap_or_default()
    )
}

fn exec_request(req: &ExecRequest, stdin: bool) -> GuestRequest {
    let mut argv = vec![req.cmd.clone()];
    argv.extend(req.args.iter().cloned());
    GuestRequest::Exec {
        argv,
        env: req.env.iter().cloned().collect::<BTreeMap<_, _>>(),
        cwd: req.cwd.clone(),
        user: req.user.clone(),
        timeout_ms: req.timeout.map(|t| t.as_millis() as u64),
        stdin,
    }
}

/// Room left under the per-stream cap.
fn append_capped(buf: &mut Vec<u8>, data: &[u8]) {
    let room = MAX_EXEC_OUTPUT_BYTES.saturating_sub(buf.len());
    buf.extend_from_slice(&data[..data.len().min(room)]);
}

/// Run a command to completion.
pub async fn exec(vsock: &Path, req: ExecRequest) -> Result<ExecOutput> {
    let mut s = open(vsock).await?;
    send_request(&mut s, &exec_request(&req, req.stdin.is_some())).await?;
    if let Some(bytes) = &req.stdin {
        for chunk in bytes.chunks(64 * 1024) {
            write_frame(&mut s, kind::STDIN, chunk).await?;
        }
        write_frame(&mut s, kind::STDIN_EOF, &[]).await?;
    }
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    let collect = async {
        loop {
            match read_frame(&mut s).await? {
                Some((kind::STDOUT, p)) => append_capped(&mut stdout, &p),
                Some((kind::STDERR, p)) => append_capped(&mut stderr, &p),
                Some((kind::JSON, p)) => return parse_reply(&p),
                Some(_) => {}
                None => bail!("the guest agent closed the stream mid-command"),
            }
        }
    };
    // The guest enforces the timeout; this is only for a guest that has
    // stopped answering altogether.
    let reply = match req.timeout {
        Some(t) => tokio::time::timeout(t + Duration::from_secs(10), collect)
            .await
            .unwrap_or_else(|_| Ok(GuestReply::Exited { code: TIMEOUT_EXIT_CODE, timed_out: true }))?,
        None => collect.await?,
    };
    match reply {
        GuestReply::Exited { code, timed_out } => Ok(ExecOutput { code, stdout, stderr, timed_out }),
        GuestReply::Error { message } => bail!("{message}"),
        other => bail!("unexpected reply {other:?}"),
    }
}

/// Run a command, streaming stdin in and stdout out.
pub async fn exec_stream(
    vsock: &Path,
    req: ExecRequest,
    stdin: Option<mpsc::Receiver<Bytes>>,
    stdout: mpsc::Sender<Bytes>,
) -> Result<StreamOutcome> {
    let s = open(vsock).await?;
    let (mut rd, mut wr) = s.into_split();
    let first = serde_json::to_vec(&exec_request(&req, stdin.is_some()))?;
    write_frame(&mut wr, kind::JSON, &first).await?;
    let feeder = stdin.map(|mut rx| {
        tokio::spawn(async move {
            while let Some(chunk) = rx.recv().await {
                for piece in chunk.chunks(64 * 1024) {
                    if write_frame(&mut wr, kind::STDIN, piece).await.is_err() {
                        return;
                    }
                }
            }
            let _ = write_frame(&mut wr, kind::STDIN_EOF, &[]).await;
        })
    });
    let mut stderr = Vec::new();
    let result = loop {
        match read_frame(&mut rd).await {
            Ok(Some((kind::STDOUT, p))) => {
                if stdout.send(Bytes::from(p)).await.is_err() {
                    break Err(anyhow::anyhow!("the reader went away"));
                }
            }
            Ok(Some((kind::STDERR, p))) => append_capped(&mut stderr, &p),
            Ok(Some((kind::JSON, p))) => break parse_reply(&p),
            Ok(Some(_)) => {}
            Ok(None) => break Err(anyhow::anyhow!("the guest agent closed the stream mid-command")),
            Err(e) => break Err(e),
        }
    };
    if let Some(f) = feeder {
        f.abort();
    }
    match result? {
        GuestReply::Exited { code, timed_out } => Ok(StreamOutcome { code, stderr, timed_out }),
        GuestReply::Error { message } => bail!("{message}"),
        other => bail!("unexpected reply {other:?}"),
    }
}

/// A raw stream to `127.0.0.1:port` inside the guest.
pub async fn connect_port(vsock: &Path, port: u16) -> Result<UnixStream> {
    let mut s = open(vsock).await?;
    send_request(&mut s, &GuestRequest::Connect { port }).await?;
    match read_frame(&mut s).await? {
        Some((kind::JSON, p)) => match parse_reply(&p)? {
            GuestReply::Ok => Ok(s),
            GuestReply::Error { message } => bail!("{message}"),
            other => bail!("unexpected reply {other:?}"),
        },
        _ => bail!("the guest agent closed the stream"),
    }
}

#[cfg(test)]
mod tests {
    //! Against a fake VMM: a unix socket that does the CONNECT handshake and
    //! then speaks the agent side of the protocol, the way puku-guestd does.
    use super::*;

    async fn fake_vmm(reply_to_exec: Vec<(u8, Vec<u8>)>) -> (tempdir::Dir, std::path::PathBuf) {
        let dir = tempdir::Dir::new();
        let path = dir.path().join("vsock.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        tokio::spawn(async move {
            loop {
                let (mut s, _) = listener.accept().await.unwrap();
                let frames = reply_to_exec.clone();
                tokio::spawn(async move {
                    let mut line = Vec::new();
                    loop {
                        let b = s.read_u8().await.unwrap();
                        if b == b'\n' {
                            break;
                        }
                        line.push(b);
                    }
                    assert_eq!(line, format!("CONNECT {GUEST_AGENT_PORT}").into_bytes());
                    s.write_all(b"OK 1073741824\n").await.unwrap();
                    let (_, req) = read_frame(&mut s).await.unwrap().unwrap();
                    let req: GuestRequest = serde_json::from_slice(&req).unwrap();
                    match req {
                        GuestRequest::Ping => {
                            let r = serde_json::to_vec(&GuestReply::Pong { version: "t".into() }).unwrap();
                            write_frame(&mut s, kind::JSON, &r).await.unwrap();
                        }
                        GuestRequest::Exec { stdin, .. } => {
                            let mut input = Vec::new();
                            if stdin {
                                while let Some((k, p)) = read_frame(&mut s).await.unwrap() {
                                    if k == kind::STDIN_EOF {
                                        break;
                                    }
                                    input.extend(p);
                                }
                                write_frame(&mut s, kind::STDOUT, &input).await.unwrap();
                            }
                            for (k, p) in frames {
                                write_frame(&mut s, k, &p).await.unwrap();
                            }
                        }
                        _ => {}
                    }
                });
            }
        });
        (dir, path)
    }

    fn exited(code: i32, timed_out: bool) -> (u8, Vec<u8>) {
        (kind::JSON, serde_json::to_vec(&GuestReply::Exited { code, timed_out }).unwrap())
    }

    #[tokio::test]
    async fn ping_goes_through_the_vsock_handshake() {
        let (_dir, path) = fake_vmm(vec![]).await;
        wait_ready(&path, Duration::from_secs(2)).await.unwrap();
    }

    #[tokio::test]
    async fn exec_collects_both_streams_and_the_code() {
        let (_dir, path) =
            fake_vmm(vec![(kind::STDOUT, b"out".to_vec()), (kind::STDERR, b"err".to_vec()), exited(7, false)]).await;
        let out = exec(&path, ExecRequest::sh("x").with_stdin("in-")).await.unwrap();
        assert_eq!(out.stdout, b"in-out");
        assert_eq!(out.stderr, b"err");
        assert_eq!((out.code, out.timed_out), (7, false));
    }

    #[tokio::test]
    async fn exec_stream_forwards_stdout_chunks() {
        let (_dir, path) = fake_vmm(vec![(kind::STDOUT, b"tar-bytes".to_vec()), exited(0, false)]).await;
        let (in_tx, in_rx) = mpsc::channel(4);
        let (out_tx, mut out_rx) = mpsc::channel(4);
        in_tx.send(Bytes::from_static(b"up-")).await.unwrap();
        drop(in_tx);
        let outcome = exec_stream(&path, ExecRequest::sh("x"), Some(in_rx), out_tx).await.unwrap();
        let mut got = Vec::new();
        while let Some(b) = out_rx.recv().await {
            got.extend_from_slice(&b);
        }
        assert_eq!(got, b"up-tar-bytes");
        assert_eq!(outcome.code, 0);
    }

    #[tokio::test]
    async fn a_refused_handshake_is_an_error() {
        let dir = tempdir::Dir::new();
        let path = dir.path().join("vsock.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let _ = s.read(&mut [0u8; 64]).await;
            let _ = s.write_all(b"ERR no listener\n").await;
        });
        assert!(open(&path).await.is_err());
    }

    /// A scratch directory without pulling in a crate for it.
    mod tempdir {
        pub struct Dir(std::path::PathBuf);
        impl Dir {
            pub fn new() -> Self {
                // Short: unix socket paths are limited to ~104 bytes on macOS.
                let p = std::path::PathBuf::from(format!("/tmp/pk-{}", &uuid::Uuid::new_v4().simple().to_string()[..10]));
                std::fs::create_dir_all(&p).unwrap();
                Dir(p)
            }
            pub fn path(&self) -> &std::path::Path {
                &self.0
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }
}
