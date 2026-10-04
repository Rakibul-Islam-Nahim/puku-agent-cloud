//! The worker data plane, control-plane side (see `puku_cloud_proto::data_proto`).
//!
//! Workers park idle WebSockets here; a request that needs a machine's bytes
//! takes one, names its target, and owns the socket until it is done. The
//! socket is never returned to the pool -- one socket, one stream -- and the
//! worker dials its replacement the moment it sees the header.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use puku_cloud_proto::data_proto::{DataHello, DataMsg, StreamHeader, StreamTarget, DATA_CHUNK_BYTES};
use puku_cloud_proto::worker_proto::Down;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use uuid::Uuid;

use crate::AppState;

/// How long a request waits for a worker to dial a socket when its pool is
/// empty. A worker keeps a couple idle, so this is only the burst case.
const POOL_WAIT: Duration = Duration::from_secs(10);
/// How long a worker has to answer a header with `Ready`/`Error`.
const READY_WAIT: Duration = Duration::from_secs(30);

#[derive(Default)]
pub struct DataPool {
    idle: Mutex<HashMap<Uuid, Vec<WebSocket>>>,
    arrived: tokio::sync::Notify,
}

impl DataPool {
    pub fn new() -> Arc<Self> {
        Arc::new(DataPool::default())
    }

    fn put(&self, worker_id: Uuid, ws: WebSocket) {
        self.idle.lock().unwrap().entry(worker_id).or_default().push(ws);
        self.arrived.notify_waiters();
    }

    fn take(&self, worker_id: Uuid) -> Option<WebSocket> {
        self.idle.lock().unwrap().get_mut(&worker_id).and_then(|v| v.pop())
    }

    /// Drop every idle socket for a worker whose control link went away.
    pub fn forget(&self, worker_id: Uuid) {
        self.idle.lock().unwrap().remove(&worker_id);
    }

    #[cfg(test)]
    pub fn idle_count(&self, worker_id: Uuid) -> usize {
        self.idle.lock().unwrap().get(&worker_id).map_or(0, |v| v.len())
    }
}

/// A failure to reach a machine, carrying the HTTP status it maps to.
#[derive(Debug)]
pub struct StreamError {
    pub status: u16,
    pub message: String,
}

impl StreamError {
    fn new(status: u16, message: impl Into<String>) -> Self {
        StreamError { status, message: message.into() }
    }
}

/// `GET /v1/worker/data`: a worker offering one idle socket.
pub async fn handle_data_socket(state: AppState, mut socket: WebSocket) {
    let hello = match tokio::time::timeout(Duration::from_secs(10), socket.recv()).await {
        Ok(Some(Ok(Message::Text(t)))) => match serde_json::from_str::<DataHello>(&t) {
            Ok(h) => h,
            Err(_) => return,
        },
        _ => return,
    };
    // A data socket only makes sense for a worker whose control link is up:
    // that is where its machines are tracked and where "dial more" goes.
    let Some(worker) = state.workers.all().into_iter().find(|w| w.name == hello.worker_name) else {
        tracing::debug!(worker = %hello.worker_name, "data socket from a worker with no control link");
        return;
    };
    // That link already authenticated the worker's token, so a data socket only
    // has to present the same one. Not a database lookup: a worker keeps a pool of
    // these and replaces each one it uses, and a query per socket exhausted the
    // connection pool -- every socket refused then, and machines went unreachable.
    if !same_bytes(&crate::auth::hash_key(&hello.auth_token), &worker.token_hash) {
        tracing::warn!(worker = %hello.worker_name, "data socket refused: not the token this worker registered with");
        return;
    }
    state.data.put(worker.worker_id, socket);
}

/// Constant-time equality, so a refusal takes as long wherever the bytes differ.
fn same_bytes(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Open a stream to `target` on a machine hosted by `worker_id`. Returns the
/// socket once the worker has said `Ready`; the caller then speaks the
/// target's framing on it.
pub async fn open(
    state: &AppState,
    worker_id: Uuid,
    machine_id: Uuid,
    target: StreamTarget,
) -> Result<WebSocket, StreamError> {
    let header = StreamHeader { stream_id: Uuid::new_v4(), machine_id, target };
    let text = serde_json::to_string(&header).expect("header serializes");
    // A pooled socket may have died on the worker side without us noticing;
    // the Ready handshake is where that shows. Try a few before giving up.
    for _ in 0..3 {
        let mut ws = match take_or_wait(state, worker_id).await {
            Some(ws) => ws,
            None => {
                return Err(StreamError::new(
                    503,
                    "the worker hosting this machine opened no data connection in time",
                ))
            }
        };
        if ws.send(Message::Text(text.clone().into())).await.is_err() {
            continue;
        }
        match tokio::time::timeout(READY_WAIT, next_msg(&mut ws)).await {
            Ok(Some(DataMsg::Ready)) => return Ok(ws),
            Ok(Some(DataMsg::Error { status, message })) => return Err(StreamError::new(status, message)),
            Ok(Some(other)) => {
                return Err(StreamError::new(502, format!("worker answered a header with {other:?}")))
            }
            Ok(None) => continue, // dead socket: take another
            Err(_) => return Err(StreamError::new(504, "the worker did not answer in time")),
        }
    }
    Err(StreamError::new(502, "could not open a data stream to the worker"))
}

async fn take_or_wait(state: &AppState, worker_id: Uuid) -> Option<WebSocket> {
    if let Some(ws) = state.data.take(worker_id) {
        return Some(ws);
    }
    let handle = state.workers.get(worker_id)?;
    handle.send(Down::OpenDataSockets { count: 2 });
    let deadline = tokio::time::Instant::now() + POOL_WAIT;
    loop {
        // Register interest before checking, so an arrival between the
        // check and the wait is not missed.
        let notified = state.data.arrived.notified();
        if let Some(ws) = state.data.take(worker_id) {
            return Some(ws);
        }
        if tokio::time::timeout_at(deadline, notified).await.is_err() {
            return state.data.take(worker_id);
        }
    }
}

/// Next framing message, skipping binary frames and pings. `None` when the
/// socket closed.
pub async fn next_msg(ws: &mut WebSocket) -> Option<DataMsg> {
    loop {
        match ws.recv().await? {
            Ok(Message::Text(t)) => return serde_json::from_str(&t).ok(),
            Ok(Message::Close(_)) | Err(_) => return None,
            Ok(_) => continue,
        }
    }
}

/// Send a request body as binary frames, then `Eof`.
pub async fn send_body(ws: &mut WebSocket, body: axum::body::Body) -> Result<(), StreamError> {
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| StreamError::new(400, format!("reading the request body: {e}")))?;
        for piece in chunk.chunks(DATA_CHUNK_BYTES) {
            ws.send(Message::Binary(Bytes::copy_from_slice(piece)))
                .await
                .map_err(|_| StreamError::new(502, "the worker closed the stream"))?;
        }
    }
    send_msg(ws, &DataMsg::Eof).await
}

pub async fn send_msg(ws: &mut WebSocket, msg: &DataMsg) -> Result<(), StreamError> {
    ws.send(Message::Text(serde_json::to_string(msg).expect("serializes").into()))
        .await
        .map_err(|_| StreamError::new(502, "the worker closed the stream"))
}

/// The bytes a worker streams back, as a response body. Ends at `Eof`; an
/// `Error` or an early close ends it with an error, so a client sees a
/// truncated transfer as a failure rather than a short file.
pub fn body_from(ws: WebSocket) -> axum::body::Body {
    let stream = futures::stream::unfold(Some(ws), |ws| async move {
        let mut ws = ws?;
        loop {
            match ws.recv().await {
                Some(Ok(Message::Binary(b))) => return Some((Ok(b), Some(ws))),
                Some(Ok(Message::Text(t))) => match serde_json::from_str::<DataMsg>(&t) {
                    Ok(DataMsg::Eof) => return None,
                    Ok(DataMsg::Error { message, .. }) => {
                        return Some((Err(std::io::Error::other(message)), None))
                    }
                    _ => continue,
                },
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => {
                    return Some((Err(std::io::Error::other("stream ended early")), None))
                }
                Some(Ok(_)) => continue,
            }
        }
    });
    axum::body::Body::from_stream(stream)
}

/// Turn a port stream into an ordinary byte pipe for an HTTP client to run
/// over. Binary frames become bytes; closing either side closes the other.
pub fn into_duplex(ws: WebSocket) -> tokio::io::DuplexStream {
    let (ours, theirs) = tokio::io::duplex(DATA_CHUNK_BYTES);
    let (mut rd, mut wr) = tokio::io::split(ours);
    let (mut sink, mut stream) = ws.split();
    tokio::spawn(async move {
        // worker -> client
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
    });
    tokio::spawn(async move {
        // client -> worker
        let mut buf = vec![0u8; DATA_CHUNK_BYTES];
        loop {
            match rd.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if sink.send(Message::Binary(Bytes::copy_from_slice(&buf[..n]))).await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = sink
            .send(Message::Text(serde_json::to_string(&DataMsg::Eof).unwrap().into()))
            .await;
        let _ = sink.close().await;
    });
    theirs
}
