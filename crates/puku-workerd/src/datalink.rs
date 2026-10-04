//! The worker data plane, worker side (see `puku_cloud_proto::data_proto`).
//!
//! Keeps a few idle WebSockets open to controld's `/v1/worker/data`. Each
//! one waits for a `StreamHeader`, dials its own replacement, and then
//! serves that one stream against a machine on this worker: a guest port, a
//! command, a file, a tarball.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use puku_cloud_proto::data_proto::{
    DataHello, DataMsg, FileEntry, StreamHeader, StreamTarget, DATA_CHUNK_BYTES, MAX_EXEC_OUTPUT_BYTES,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use crate::machines::{Machines, Running};
use crate::vm::ExecRequest;

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Idle sockets to keep ready. Two covers a screen stream plus an observe in
/// flight, which is the common burst; controld asks for more when it runs dry.
const IDLE_TARGET: usize = 2;
/// Cloudflare closes a WebSocket idle for 100 s.
const PING_EVERY: Duration = Duration::from_secs(25);

pub struct DataLink {
    url: String,
    worker_name: String,
    token: String,
    machines: Machines,
    idle: AtomicUsize,
}

impl DataLink {
    pub fn new(url: String, worker_name: String, token: String, machines: Machines) -> Arc<Self> {
        Arc::new(DataLink { url, worker_name, token, machines, idle: AtomicUsize::new(0) })
    }

    /// Keep the pool topped up for as long as the process lives.
    pub fn spawn(self: &Arc<Self>) {
        let me = self.clone();
        tokio::spawn(async move {
            loop {
                let idle = me.idle.load(Ordering::Relaxed);
                if idle < IDLE_TARGET {
                    me.dial(IDLE_TARGET - idle);
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        });
    }

    /// Dial `n` more sockets now (controld said its pool is empty).
    pub fn dial(self: &Arc<Self>, n: usize) {
        for _ in 0..n {
            let me = self.clone();
            me.idle.fetch_add(1, Ordering::Relaxed);
            tokio::spawn(async move {
                if let Err(e) = me.clone().socket().await {
                    tracing::debug!(error = format!("{e:#}"), "data socket ended");
                }
            });
        }
    }

    async fn socket(self: Arc<Self>) -> Result<()> {
        let mut counted = true;
        let result = self.socket_inner(&mut counted).await;
        if counted {
            self.idle.fetch_sub(1, Ordering::Relaxed);
        }
        result
    }

    async fn socket_inner(self: &Arc<Self>, counted: &mut bool) -> Result<()> {
        let (mut ws, _) = match tokio_tungstenite::connect_async(&self.url).await {
            Ok(ok) => ok,
            Err(e) => {
                // Do not spin on a control plane that is down.
                tokio::time::sleep(Duration::from_secs(3)).await;
                return Err(e.into());
            }
        };
        let hello = DataHello { worker_name: self.worker_name.clone(), auth_token: self.token.clone() };
        ws.send(Message::Text(serde_json::to_string(&hello)?.into())).await?;

        let mut ping = tokio::time::interval(PING_EVERY);
        ping.tick().await;
        let header = loop {
            tokio::select! {
                _ = ping.tick() => ws.send(Message::Ping(Bytes::new())).await?,
                msg = ws.next() => match msg {
                    Some(Ok(Message::Text(t))) => match serde_json::from_str::<StreamHeader>(&t) {
                        Ok(h) => break h,
                        Err(e) => anyhow::bail!("bad stream header: {e}"),
                    },
                    Some(Ok(Message::Close(_))) | None => return Ok(()),
                    Some(Err(e)) => return Err(e.into()),
                    Some(Ok(_)) => {}
                }
            }
        };
        // No longer idle: replace it before serving, so the pool refills
        // while this stream is still busy.
        *counted = false;
        self.idle.fetch_sub(1, Ordering::Relaxed);
        self.dial(1);
        serve(&self.machines, ws, header).await
    }
}

async fn send_msg(ws: &mut Ws, msg: &DataMsg) -> Result<()> {
    ws.send(Message::Text(serde_json::to_string(msg)?.into())).await?;
    Ok(())
}

async fn fail(ws: &mut Ws, status: u16, message: impl Into<String>) -> Result<()> {
    send_msg(ws, &DataMsg::Error { status, message: message.into() }).await?;
    let _ = ws.close(None).await;
    Ok(())
}

/// Serve one stream.
pub async fn serve(machines: &Machines, mut ws: Ws, header: StreamHeader) -> Result<()> {
    let Some(m) = machines.get(header.machine_id) else {
        return fail(&mut ws, 409, format!("machine {} is not running on this worker", header.machine_id)).await;
    };
    match header.target {
        StreamTarget::Port { port } => {
            if !m.spec.expose.contains(&port) {
                return fail(&mut ws, 403, format!("port {port} is not exposed")).await;
            }
            match m.vm.connect_port(port).await {
                Ok(io) => {
                    send_msg(&mut ws, &DataMsg::Ready).await?;
                    pipe(ws, io).await;
                    Ok(())
                }
                Err(e) => fail(&mut ws, 502, format!("{e:#}")).await,
            }
        }
        StreamTarget::Exec { argv, cwd, env, user, timeout_ms, stdin } => {
            send_msg(&mut ws, &DataMsg::Ready).await?;
            let input = if stdin { Some(read_all(&mut ws).await?) } else { None };
            let Some((cmd, args)) = argv.split_first() else {
                return fail(&mut ws, 400, "argv must name a command").await;
            };
            let mut merged: Vec<(String, String)> =
                m.spec.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            merged.retain(|(k, _)| !env.contains_key(k));
            merged.extend(env);
            let req = ExecRequest {
                cmd: cmd.clone(),
                args: args.to_vec(),
                stdin: input,
                user: user.or_else(|| m.default_user()),
                cwd,
                env: merged,
                timeout: Some(Duration::from_millis(timeout_ms)),
            };
            match m.vm.exec(req).await {
                Ok(out) => {
                    let (stdout, t1) = capped(&out.stdout);
                    let (stderr, t2) = capped(&out.stderr);
                    send_msg(
                        &mut ws,
                        &DataMsg::ExecResult {
                            code: out.code,
                            stdout,
                            stderr,
                            timed_out: out.timed_out,
                            truncated: t1 || t2,
                        },
                    )
                    .await
                }
                Err(e) => fail(&mut ws, 502, format!("{e:#}")).await,
            }
        }
        StreamTarget::FileRead { path, max_bytes } => {
            match probe(&m, &path).await? {
                Probe::Missing => return fail(&mut ws, 404, format!("{path} does not exist")).await,
                Probe::Dir => return fail(&mut ws, 400, format!("{path} is a directory")).await,
                Probe::File(size) if max_bytes.is_some_and(|max| size > max) => {
                    return fail(&mut ws, 413, format!("{path} is {size} bytes, over the {} allowed", max_bytes.unwrap()))
                        .await
                }
                Probe::File(_) => {}
            }
            send_msg(&mut ws, &DataMsg::Ready).await?;
            stream_out(&m, ws, sh_args("cat -- \"$1\"", &[&path]), &[0]).await
        }
        StreamTarget::FileWrite { path, mode } => {
            send_msg(&mut ws, &DataMsg::Ready).await?;
            let script = "mkdir -p -- \"$(dirname -- \"$1\")\" && cat > \"$1\" && chmod \"$2\" \"$1\"";
            let mut req = sh_args(script, &[&path, &format!("{mode:o}")]);
            // As the volume's user, like exec: on libkrun an unset user is the
            // image's (root for most), and the parents made here would be
            // root's alone -- unwritable, or unreadable, by the machine itself.
            req.user = m.default_user();
            stream_in(&m, ws, req).await
        }
        StreamTarget::List { path, recursive } => {
            send_msg(&mut ws, &DataMsg::Ready).await?;
            match list(&m, &path, recursive).await? {
                Ok(entries) => send_msg(&mut ws, &DataMsg::Listing { entries }).await,
                Err((status, message)) => fail(&mut ws, status, message).await,
            }
        }
        StreamTarget::ArchiveGet { path, excludes } => {
            match probe(&m, &path).await? {
                Probe::Dir => {}
                Probe::Missing => return fail(&mut ws, 404, format!("{path} does not exist")).await,
                Probe::File(_) => return fail(&mut ws, 400, format!("{path} is not a directory")).await,
            }
            send_msg(&mut ws, &DataMsg::Ready).await?;
            let mut args: Vec<String> = vec!["-czf".into(), "-".into(), "-C".into(), path];
            args.extend(excludes.into_iter().map(|e| format!("--exclude={e}")));
            args.push(".".into());
            // tar exits 1 when a file changed while it was read; the archive
            // is still whole, and a live workspace changes all the time.
            let req = ExecRequest { cmd: "tar".into(), args, user: m.default_user(), ..Default::default() };
            stream_out(&m, ws, req, &[0, 1]).await
        }
        StreamTarget::ArchivePut { path, replace } => {
            send_msg(&mut ws, &DataMsg::Ready).await?;
            let script = "mkdir -p -- \"$1\" && { [ \"$2\" != 1 ] || find \"$1\" -mindepth 1 -delete; } \
                          && tar -xzf - -C \"$1\"";
            let mut req = sh_args(script, &[&path, if replace { "1" } else { "0" }]);
            req.user = m.default_user();
            stream_in(&m, ws, req).await
        }
    }
}

/// `sh -c <script> sh <args...>`: arguments travel as positional parameters
/// and are never re-parsed, whatever characters a path contains.
fn sh_args(script: &str, args: &[&str]) -> ExecRequest {
    let mut a = vec!["-c".to_string(), script.to_string(), "sh".to_string()];
    a.extend(args.iter().map(|s| s.to_string()));
    ExecRequest { cmd: "sh".into(), args: a, ..Default::default() }
}

fn capped(bytes: &[u8]) -> (String, bool) {
    let cut = bytes.len() > MAX_EXEC_OUTPUT_BYTES;
    (String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_EXEC_OUTPUT_BYTES)]).into_owned(), cut)
}

/// Every binary frame up to `Eof`.
async fn read_all(ws: &mut Ws) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    while let Some(msg) = ws.next().await {
        match msg? {
            Message::Binary(b) => buf.extend_from_slice(&b),
            Message::Text(t) if matches!(serde_json::from_str(&t), Ok(DataMsg::Eof)) => return Ok(buf),
            Message::Close(_) => anyhow::bail!("the control plane closed the stream mid-upload"),
            _ => {}
        }
    }
    anyhow::bail!("the control plane closed the stream mid-upload")
}

enum Probe {
    Missing,
    Dir,
    File(u64),
}

async fn probe(m: &Running, path: &str) -> Result<Probe> {
    let mut req = sh_args(
        "if [ -d \"$1\" ]; then echo dir; elif [ -e \"$1\" ]; then wc -c < \"$1\"; else echo missing; fi",
        &[path],
    );
    req.user = m.default_user();
    let out = m.vm.exec(req).await?;
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(match text.trim() {
        "dir" => Probe::Dir,
        "missing" | "" => Probe::Missing,
        n => Probe::File(n.parse().unwrap_or(0)),
    })
}

/// Directory entries via `find` and `stat`, both of which busybox has --
/// guest images are anything from alpine to debian.
async fn list(m: &Running, path: &str, recursive: bool) -> Result<std::result::Result<Vec<FileEntry>, (u16, String)>> {
    let depth = if recursive { "" } else { "-maxdepth 1" };
    let script = format!(
        "[ -e \"$1\" ] || {{ echo MISSING; exit 0; }}; [ -d \"$1\" ] || {{ echo NOTDIR; exit 0; }}; \
         find \"$1\" -mindepth 1 {depth} ! -type l -exec stat -c '%F|%s|%a|%n' {{}} +"
    );
    let mut req = sh_args(&script, &[path]);
    req.user = m.default_user();
    let out = m.vm.exec(req).await?;
    let text = String::from_utf8_lossy(&out.stdout);
    match text.trim() {
        "MISSING" => return Ok(Err((404, format!("{path} does not exist")))),
        "NOTDIR" => return Ok(Err((400, format!("{path} is not a directory")))),
        _ => {}
    }
    if !out.success() {
        return Ok(Err((500, String::from_utf8_lossy(&out.stderr).trim().to_string())));
    }
    Ok(Ok(parse_listing(&text)))
}

fn parse_listing(text: &str) -> Vec<FileEntry> {
    text.lines()
        .filter_map(|line| {
            let mut p = line.splitn(4, '|');
            let (kind, size, mode, path) = (p.next()?, p.next()?, p.next()?, p.next()?);
            let kind = match kind {
                "directory" => "dir",
                "regular file" | "regular empty file" => "file",
                _ => return None, // sockets, fifos, devices
            };
            let mode = u32::from_str_radix(mode, 8).unwrap_or(0);
            Some(FileEntry {
                path: path.to_string(),
                kind: kind.to_string(),
                size: if kind == "dir" { 0 } else { size.parse().unwrap_or(0) },
                executable: kind == "file" && mode & 0o100 != 0,
            })
        })
        .collect()
}

/// Run a command and stream its stdout to the control plane as binary
/// frames, then `Eof` -- or `Error` if it exits outside `ok_codes`.
async fn stream_out(m: &Running, mut ws: Ws, mut req: ExecRequest, ok_codes: &[i32]) -> Result<()> {
    if req.user.is_none() {
        req.user = m.default_user();
    }
    let (tx, mut rx) = mpsc::channel::<Bytes>(8);
    let run = m.vm.exec_stream(req, None, tx);
    tokio::pin!(run);
    let mut outcome = None;
    loop {
        tokio::select! {
            chunk = rx.recv() => match chunk {
                Some(b) => {
                    for piece in b.chunks(DATA_CHUNK_BYTES) {
                        ws.send(Message::Binary(Bytes::copy_from_slice(piece))).await?;
                    }
                }
                None => break,
            },
            res = &mut run, if outcome.is_none() => outcome = Some(res),
        }
    }
    let outcome = match outcome {
        Some(o) => o,
        None => run.await,
    };
    match outcome {
        Ok(o) if ok_codes.contains(&o.code) => send_msg(&mut ws, &DataMsg::Eof).await,
        Ok(o) => fail(&mut ws, 500, String::from_utf8_lossy(&o.stderr).trim().to_string()).await,
        Err(e) => fail(&mut ws, 502, format!("{e:#}")).await,
    }
}

/// Stream binary frames from the control plane into a command's stdin; then
/// `Done` or `Error`.
async fn stream_in(m: &Running, mut ws: Ws, mut req: ExecRequest) -> Result<()> {
    if req.user.is_none() {
        req.user = m.default_user();
    }
    let (in_tx, in_rx) = mpsc::channel::<Bytes>(8);
    let (out_tx, _out_rx) = mpsc::channel::<Bytes>(8);
    let run = m.vm.exec_stream(req, Some(in_rx), out_tx);
    tokio::pin!(run);
    let mut in_tx = Some(in_tx);
    let outcome = loop {
        tokio::select! {
            res = &mut run => break res,
            msg = ws.next(), if in_tx.is_some() => match msg {
                Some(Ok(Message::Binary(b))) => {
                    if let Some(tx) = &in_tx {
                        let _ = tx.send(b).await;
                    }
                }
                Some(Ok(Message::Text(t))) => {
                    if matches!(serde_json::from_str(&t), Ok(DataMsg::Eof)) {
                        in_tx = None; // closes the command's stdin
                    }
                }
                Some(Ok(_)) => {}
                Some(Err(_)) | None => in_tx = None,
            },
        }
    };
    match outcome {
        Ok(o) if o.code == 0 => send_msg(&mut ws, &DataMsg::Done).await,
        Ok(o) => fail(&mut ws, 500, String::from_utf8_lossy(&o.stderr).trim().to_string()).await,
        Err(e) => fail(&mut ws, 502, format!("{e:#}")).await,
    }
}

/// Splice a data socket and a guest connection until either side is done.
async fn pipe(ws: Ws, io: Box<dyn crate::vm::GuestIo>) {
    let (mut sink, mut stream) = ws.split();
    let (mut rd, mut wr) = tokio::io::split(io);
    let up = async {
        let mut buf = vec![0u8; DATA_CHUNK_BYTES];
        loop {
            match rd.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if sink.send(Message::Binary(Bytes::copy_from_slice(&buf[..n]))).await.is_err() {
                        return;
                    }
                }
            }
        }
        let _ = sink.send(Message::Text(serde_json::to_string(&DataMsg::Eof).unwrap().into())).await;
        let _ = sink.close().await;
    };
    let down = async {
        while let Some(Ok(msg)) = stream.next().await {
            match msg {
                Message::Binary(b) => {
                    if wr.write_all(&b).await.is_err() {
                        break;
                    }
                }
                Message::Text(t) if matches!(serde_json::from_str(&t), Ok(DataMsg::Eof)) => break,
                Message::Close(_) => break,
                _ => {}
            }
        }
        let _ = wr.shutdown().await;
    };
    // Either direction finishing is not the end: a request fully sent still
    // has a response to come. Wait for both.
    tokio::join!(up, down);
}

#[cfg(test)]
mod tests {
    use super::parse_listing;

    #[test]
    fn listings_parse_the_stat_format() {
        let text = "directory|4096|755|/home/u/sub\n\
                    regular file|12|644|/home/u/a.txt\n\
                    regular empty file|0|755|/home/u/run.sh\n\
                    fifo|0|644|/home/u/pipe\n\
                    regular file|3|600|/home/u/odd|name\n";
        let e = parse_listing(text);
        assert_eq!(e.len(), 4, "the fifo is skipped: {e:?}");
        assert_eq!(e[0].kind, "dir");
        assert_eq!(e[0].size, 0);
        assert_eq!((e[1].kind.as_str(), e[1].size, e[1].executable), ("file", 12, false));
        assert!(e[2].executable, "0755 is executable by its owner");
        assert_eq!(e[3].path, "/home/u/odd|name", "a pipe in a name survives");
    }
}
