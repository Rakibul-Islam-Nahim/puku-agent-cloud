//! An in-process S3 for the integration tests: just enough of the API for
//! `BlobStore` and a fake worker to take a snapshot through it -- single and
//! multipart PUT, complete, abort, GET, DELETE, path-style. Signatures are
//! not checked; that `BlobStore` signs is covered by its own unit tests.

#![cfg(test)]

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use axum::extract::{Request, State};
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};

#[derive(Clone, Default)]
pub struct FakeS3 {
    inner: Arc<Mutex<Store>>,
}

#[derive(Default)]
struct Store {
    objects: HashMap<String, Vec<u8>>,
    /// Upload id -> (object key, parts by number).
    uploads: HashMap<String, (String, BTreeMap<u32, Vec<u8>>)>,
    next: u64,
}

impl FakeS3 {
    /// Serve it on a free port. Returns the store and its endpoint URL.
    pub async fn start() -> (FakeS3, String) {
        let s3 = FakeS3::default();
        let app = axum::Router::new().fallback(handle).with_state(s3.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (s3, format!("http://{addr}"))
    }

    /// Every object's key, without the bucket, sorted.
    pub fn keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = self.inner.lock().unwrap().objects.keys().cloned().collect();
        keys.sort();
        keys
    }

    pub fn get(&self, key: &str) -> Option<Vec<u8>> {
        self.inner.lock().unwrap().objects.get(key).cloned()
    }
}

fn xml(body: String) -> Response {
    ([("content-type", "application/xml")], body).into_response()
}

async fn handle(State(s3): State<FakeS3>, req: Request) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let query: Vec<(String, String)> = req
        .uri()
        .query()
        .unwrap_or("")
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (k.to_string(), v.to_string())
        })
        .collect();
    let q = |name: &str| query.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());
    let body = axum::body::to_bytes(req.into_body(), usize::MAX).await.unwrap_or_default();
    // Path style: /<bucket>/<key>.
    let key = path.trim_start_matches('/').split_once('/').map(|(_, k)| k.to_string()).unwrap_or_default();

    let mut st = s3.inner.lock().unwrap();
    match (method, q("uploads"), q("uploadId")) {
        (Method::POST, Some(_), _) => {
            st.next += 1;
            let id = format!("upload-{}", st.next);
            st.uploads.insert(id.clone(), (key.clone(), BTreeMap::new()));
            xml(format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
                 <InitiateMultipartUploadResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
                 <Bucket>puku-cloud</Bucket><Key>{key}</Key><UploadId>{id}</UploadId>\
                 </InitiateMultipartUploadResult>"
            ))
        }
        (Method::PUT, _, Some(id)) => {
            let Some(part) = q("partNumber").and_then(|n| n.parse::<u32>().ok()) else {
                return StatusCode::BAD_REQUEST.into_response();
            };
            let Some((_, parts)) = st.uploads.get_mut(&id) else {
                return StatusCode::NOT_FOUND.into_response();
            };
            parts.insert(part, body.to_vec());
            ([("etag", format!("\"etag-{part}\""))], "").into_response()
        }
        (Method::POST, _, Some(id)) => {
            let Some((k, parts)) = st.uploads.remove(&id) else {
                return (StatusCode::NOT_FOUND, "<Error><Code>NoSuchUpload</Code></Error>").into_response();
            };
            // Every part the completion names must have been uploaded.
            let named = String::from_utf8_lossy(&body).matches("<PartNumber>").count();
            if named != parts.len() {
                return (StatusCode::BAD_REQUEST, "<Error><Code>InvalidPart</Code></Error>").into_response();
            }
            st.objects.insert(k.clone(), parts.into_values().flatten().collect());
            xml(format!("<CompleteMultipartUploadResult><Key>{k}</Key><ETag>\"done\"</ETag></CompleteMultipartUploadResult>"))
        }
        (Method::DELETE, _, Some(id)) => {
            st.uploads.remove(&id);
            StatusCode::NO_CONTENT.into_response()
        }
        (Method::PUT, _, None) => {
            st.objects.insert(key, body.to_vec());
            ([("etag", "\"object\"")], "").into_response()
        }
        (Method::GET, _, None) => match st.objects.get(&key) {
            Some(bytes) => bytes.clone().into_response(),
            None => StatusCode::NOT_FOUND.into_response(),
        },
        (Method::DELETE, _, None) => {
            st.objects.remove(&key);
            StatusCode::NO_CONTENT.into_response()
        }
        _ => StatusCode::METHOD_NOT_ALLOWED.into_response(),
    }
}
