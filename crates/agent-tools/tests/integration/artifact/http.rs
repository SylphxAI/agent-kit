use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
};

use super::*;
use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};

#[derive(Clone)]
struct ServerState {
    backend: LocalArtifactBackend,
    requests: Arc<AtomicUsize>,
}

async fn handler(State(state): State<ServerState>, request: Request) -> Response {
    state.requests.fetch_add(1, Ordering::Relaxed);
    if request
        .headers()
        .get("Authorization")
        .and_then(|h| h.to_str().ok())
        != Some("Bearer test-token")
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let (parts, body) = request.into_parts();
    let body = axum::body::to_bytes(body, 7 * MAX_ARTIFACT_BYTES)
        .await
        .unwrap();
    tokio::task::spawn_blocking(move || {
        let path = parts.uri.path();
        let method = parts.method.as_str();
        let query = parts.uri.query().unwrap_or("");
        let collection = "/prefix/v1/artifacts";
        if path == collection && method == "POST" {
            assert_eq!(
                parts.headers["Content-Type"]
                    .to_str()
                    .unwrap()
                    .split(';')
                    .next(),
                Some("application/json")
            );
            let request: CreateArtifact = serde_json::from_slice(&body).unwrap();
            assert_eq!(request.room_id.as_deref(), Some("room_a"));
            let mut meta = state.backend.create(request).unwrap();
            add_preview(&mut meta);
            return (StatusCode::CREATED, Json(meta)).into_response();
        }
        if path == collection && method == "GET" {
            let filter = match query {
                "room_id=room_a&kind=markdown&limit=1" => ArtifactList {
                    room_id: Some("room_a".into()),
                    kind: Some(ArtifactKind::Markdown),
                    limit: Some(1),
                    after: None,
                },
                "room_id=room_b&kind=markdown&limit=1" => ArtifactList {
                    room_id: Some("room_b".into()),
                    kind: Some(ArtifactKind::Markdown),
                    limit: Some(1),
                    after: None,
                },
                "limit=4" => ArtifactList::default(),
                _ => panic!("incorrect query"),
            };
            let mut page = state.backend.list(filter).unwrap();
            page.artifacts.iter_mut().for_each(add_preview);
            return Json(page).into_response();
        }
        let Some(id) = path.strip_prefix("/prefix/v1/artifacts/") else {
            return StatusCode::NOT_FOUND.into_response();
        };
        match (method, id) {
            ("GET", "redirect") => (
                StatusCode::TEMPORARY_REDIRECT,
                [("Location", "/prefix/v1/artifacts/redirect-target")],
            )
                .into_response(),
            ("GET", "redirect-target") => panic!("API client followed redirect"),
            ("GET", "oversized") => {
                let chunk = Bytes::from(vec![b' '; MAX_ARTIFACT_BYTES]);
                let stream = futures::stream::iter(
                    (0..8).map(move |_| Ok::<_, std::io::Error>(chunk.clone())),
                );
                (
                    [("Content-Type", "application/json")],
                    Body::from_stream(stream),
                )
                    .into_response()
            }
            ("GET", _) => {
                let version = match query {
                    "" => None,
                    "version=1" => Some(1),
                    _ => panic!("incorrect version query"),
                };
                let mut record = match state.backend.get(id, version) {
                    Ok(record) => record,
                    Err(ArtifactError::NotFound) => return StatusCode::NOT_FOUND.into_response(),
                    Err(e) => panic!("unexpected storage error: {e}"),
                };
                add_preview(&mut record.metadata);
                match record.metadata.title.as_str() {
                    "Other origin" => {
                        record.metadata.preview_url =
                            Some("https://offline.invalid/preview?signature=opaque".into())
                    }
                    "Wrong version" => record.metadata.version += 1,
                    "Wrong bytes" => record.content = ArtifactContent::Text("bad bytes".into()),
                    _ => (),
                }
                Json(record).into_response()
            }
            ("PATCH", _) => {
                assert_eq!(
                    parts.headers["Content-Type"]
                        .to_str()
                        .unwrap()
                        .split(';')
                        .next(),
                    Some("application/json")
                );
                match state
                    .backend
                    .update(id, serde_json::from_slice(&body).unwrap())
                {
                    Ok(mut result) => {
                        add_preview(&mut result.metadata);
                        Json(result).into_response()
                    }
                    Err(ArtifactError::VersionConflict) => StatusCode::CONFLICT.into_response(),
                    Err(e) => panic!("unexpected storage error: {e}"),
                }
            }
            ("DELETE", _) => {
                assert!(body.is_empty());
                state.backend.delete(id).unwrap();
                StatusCode::NO_CONTENT.into_response()
            }
            _ => StatusCode::METHOD_NOT_ALLOWED.into_response(),
        }
    })
    .await
    .unwrap()
}

fn add_preview(meta: &mut ArtifactMetadata) {
    meta.preview_url = Some(format!(
        "https://example.test/{}/{}?signature=opaque",
        meta.artifact_id, meta.version
    ));
}

struct Mock {
    addr: std::net::SocketAddr,
    requests: Arc<AtomicUsize>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Mock {
    fn start() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let state = ServerState {
            backend: LocalArtifactBackend::new(temp.path()).unwrap(),
            requests: Arc::new(AtomicUsize::new(0)),
        };
        let requests = state.requests.clone();
        let (ready, started) = std::sync::mpsc::channel();
        let (stop, end) = tokio::sync::oneshot::channel();
        let thread = thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                    ready.send(listener.local_addr().unwrap()).unwrap();
                    axum::serve(listener, Router::new().fallback(handler).with_state(state))
                        .with_graceful_shutdown(async {
                            let _ = end.await;
                        })
                        .await
                        .unwrap();
                });
            drop(temp);
        });
        Self {
            addr: started.recv().unwrap(),
            requests,
            stop: Some(stop),
            thread: Some(thread),
        }
    }

    fn client(&self, preview: Option<&str>) -> HttpArtifactBackend {
        HttpArtifactBackend::new(
            &format!("http://{}/prefix/", self.addr),
            "test-token",
            preview,
        )
        .unwrap()
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        self.stop.take().unwrap().send(()).ok();
        self.thread.take().unwrap().join().unwrap();
    }
}

#[test]
fn http_lifecycle_sends_expected_routes_bearer_and_cas() {
    let mock = Mock::start();
    let client = mock.client(Some("https://example.test"));
    let first = client.create(note("first", "room_a")).unwrap();
    assert_eq!(first.version, 1);
    assert_eq!(
        first.preview_url.as_deref(),
        Some(
            format!(
                "https://example.test/{}/1?signature=opaque",
                first.artifact_id
            )
            .as_str()
        )
    );
    let id = &first.artifact_id;
    assert_eq!(
        client
            .update(id, change(1, "second"))
            .unwrap()
            .metadata
            .version,
        2
    );
    assert!(matches!(
        client.update(id, change(1, "old")),
        Err(ArtifactError::VersionConflict)
    ));
    assert_eq!(
        client.get(id, None).unwrap().content.decode().unwrap(),
        b"second"
    );
    let third = client.update(id, change(2, "third")).unwrap();
    assert_eq!(third.metadata.version, 3);
    assert_eq!(third.diff.unwrap().added_lines, 1);
    assert_eq!(client.get(id, Some(1)).unwrap().metadata, first);
    assert_eq!(
        client.get(id, None).unwrap().content.decode().unwrap(),
        b"third"
    );
    let filter = |room: &str| ArtifactList {
        room_id: Some(room.into()),
        kind: Some(ArtifactKind::Markdown),
        limit: Some(1),
        after: None,
    };
    assert_eq!(
        client.list(filter("room_a")).unwrap().artifacts,
        [third.metadata]
    );
    assert!(client.list(filter("room_b")).unwrap().artifacts.is_empty());
    client.delete(id).unwrap();
    assert!(matches!(
        client.get(id, Some(1)),
        Err(ArtifactError::NotFound)
    ));
    assert!(
        client
            .list(ArtifactList::default())
            .unwrap()
            .artifacts
            .is_empty()
    );
    assert_eq!(mock.requests.load(Ordering::Relaxed), 12);
}

#[test]
fn http_refuses_bad_requests_before_io_and_untrusted_receipts_after_io() {
    let mock = Mock::start();
    let client = mock.client(Some("https://example.test"));
    assert!(matches!(
        client.get("%2F", None),
        Err(ArtifactError::InvalidInput(_))
    ));
    let oversized = "a".repeat(MAX_ARTIFACT_BYTES + 1);
    assert!(matches!(
        client.create(note(&oversized, "room_a")),
        Err(ArtifactError::SizeLimit)
    ));
    assert_eq!(mock.requests.load(Ordering::Relaxed), 0);
    for title in ["Other origin", "Wrong version", "Wrong bytes"] {
        let meta = client
            .create(CreateArtifact {
                title: title.into(),
                ..note("note", "room_a")
            })
            .unwrap();
        assert!(matches!(
            client.get(&meta.artifact_id, Some(1)),
            Err(ArtifactError::InvalidResponse)
        ));
    }
    let normal = client.create(note("note", "room_a")).unwrap();
    assert!(matches!(
        mock.client(None).get(&normal.artifact_id, None),
        Err(ArtifactError::InvalidResponse)
    ));
    assert!(matches!(
        client.get("oversized", None),
        Err(ArtifactError::InvalidResponse)
    ));
    assert!(matches!(
        client.get("redirect", None),
        Err(ArtifactError::HttpStatus(307))
    ));
    assert_eq!(mock.requests.load(Ordering::Relaxed), 10);
}

#[test]
fn http_config_refuses_cleartext_userinfo_and_same_host_preview() {
    assert!(HttpArtifactBackend::new("http://example.test", "test-token", None).is_err());
    assert!(
        HttpArtifactBackend::new(
            "https://example.test",
            "test-token",
            Some("https://example.test:8443")
        )
        .is_err()
    );
    let with_user = format!("https://{}{}example.test", "user", '@');
    assert!(HttpArtifactBackend::new(&with_user, "test-token", None).is_err());
    assert!(HttpArtifactBackend::new("https://example.test", "test-token\r\n", None).is_err());
}
