//! Wire interface for an out-of-process memory retrieval service:
//! newline-delimited JSON requests and responses over a Unix domain socket
//! owned by the operator. The caller sends already-authorized owner IDs; the
//! service never decides access and never returns raw text, only exact raw-byte
//! coordinates the caller verifies before reading a byte. Caller-owned passage
//! scoring is a separate, non-indexing operation.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    time::Duration,
};

/// Wire version. A service answering another version is refused.
pub const VERSION: u32 = 1;

/// Work bounds for one scoped dense request; model-specific token limits still apply.
pub const SCOPED_DENSE_MAX_PASSAGES: usize = 32;
pub const SCOPED_DENSE_MAX_TEXT_BYTES: usize = 2048;

#[cfg(unix)]
pub(crate) fn check_dense_inputs(query: &str, passages: &[String]) -> Result<(), &'static str> {
    let valid = |text: &str| !text.trim().is_empty() && text.len() <= SCOPED_DENSE_MAX_TEXT_BYTES;
    if !valid(query)
        || !(1..=SCOPED_DENSE_MAX_PASSAGES).contains(&passages.len())
        || !passages.iter().all(|text| valid(text))
    {
        return Err("scoped dense request exceeds passage or text bounds, or contains empty text");
    }
    Ok(())
}

/// The service socket inside a data directory.
pub fn socket_path(data_dir: &Path) -> PathBuf {
    data_dir.join("memory-service.sock")
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Dense candidates for `query` among the documents of `owners`.
    Search {
        query: String,
        /// Authorized owner IDs; an empty list selects nothing.
        owners: Vec<String>,
        /// `memory`, `transcripts`, or `None` for both.
        #[serde(default)]
        source: Option<String>,
        #[serde(default)]
        since_ms: Option<i64>,
        #[serde(default)]
        until_ms: Option<i64>,
        limit: usize,
    },
    /// Dense similarity for caller-owned passages. Never indexed or sent to a remote provider.
    DenseScores {
        query: String,
        passages: Vec<String>,
    },
    /// Cross-encoder relevance of each passage to `query`, in input order.
    Rerank {
        query: String,
        passages: Vec<String>,
    },
    Status,
}

/// One dense candidate: an indexed document and the exact raw byte range of
/// the piece that matched.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct DenseHit {
    pub document_id: String,
    pub agent_id: String,
    /// Absolute byte range in the document's segment, with its SHA-256.
    pub offset: u64,
    pub length: u64,
    pub sha256: String,
    pub score: f32,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Status {
    pub embedder: String,
    pub reranker: String,
    /// Documents embedded, and documents the host index holds that are not.
    pub embedded_documents: u64,
    pub pending_documents: u64,
    pub vectors: u64,
    /// The vector space the dense index holds: provider, model, dimension
    /// and preprocessing.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub dense_space: String,
    /// What building this space was estimated to read and cost before it
    /// started, and where it stands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rebuild: Option<Rebuild>,
    /// US$ spent by remote models since the service started.
    #[serde(default)]
    pub spent_usd: f64,
    /// What background embedding is doing: `running`, `idle`, or why it is
    /// paused or retrying.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub indexing: String,
    /// The last error of each background worker that is retrying.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
    /// US$ spent today (UTC) outside a rebuild, and the day's cap: past it,
    /// rerank and new-history embedding wait for the next day.
    #[serde(default)]
    pub today_usd: f64,
    #[serde(default)]
    pub max_daily_usd: f64,
}

/// The estimate a new vector space reports before any of it is embedded.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Rebuild {
    pub documents: u64,
    /// Documents read to estimate, and the tokens they extrapolate to.
    pub sampled_documents: u64,
    pub estimated_tokens: u64,
    /// `None` when the model's price is not known.
    pub estimated_usd: Option<f64>,
    /// The cap the estimate was compared with.
    pub max_usd: f64,
    /// `running`, `done`, or why it waits.
    pub state: String,
    /// US$ the rebuild has spent so far.
    pub spent_usd: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum Reply {
    Search { hits: Vec<DenseHit>, status: Status },
    DenseScores { similarities: Vec<f32> },
    Rerank { scores: Vec<f32> },
    Status { status: Status },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Envelope<T> {
    v: u32,
    #[serde(flatten)]
    body: T,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
enum Outcome {
    Ok { ok: bool, reply: Box<Reply> },
    Err { ok: bool, error: String },
}

/// Encode one request line.
pub fn encode_request(request: &Request) -> serde_json::Result<String> {
    let mut line = serde_json::to_string(&Envelope {
        v: VERSION,
        body: request,
    })?;
    line.push('\n');
    Ok(line)
}

/// Decode one request line; a wrong version is an error.
pub fn decode_request(line: &str) -> Result<Request, String> {
    let envelope: Envelope<Request> = serde_json::from_str(line).map_err(|e| e.to_string())?;
    if envelope.v != VERSION {
        return Err(format!(
            "memory service speaks v{VERSION}, not v{}",
            envelope.v
        ));
    }
    Ok(envelope.body)
}

pub fn encode_reply(result: &Result<Reply, String>) -> String {
    let outcome = match result {
        Ok(reply) => Outcome::Ok {
            ok: true,
            reply: Box::new(reply.clone()),
        },
        Err(error) => Outcome::Err {
            ok: false,
            error: error.clone(),
        },
    };
    let mut line = serde_json::to_string(&Envelope {
        v: VERSION,
        body: outcome,
    })
    .unwrap_or_else(|e| format!(r#"{{"v":{VERSION},"ok":false,"error":"{e}"}}"#));
    line.push('\n');
    line
}

pub fn decode_reply(line: &str) -> Result<Reply, String> {
    let envelope: Envelope<Outcome> =
        serde_json::from_str(line).map_err(|e| format!("bad memory service reply: {e}"))?;
    if envelope.v != VERSION {
        return Err(format!("memory service answered v{}", envelope.v));
    }
    match envelope.body {
        Outcome::Ok { ok: true, reply } => Ok(*reply),
        Outcome::Err { error, .. } => Err(error),
        Outcome::Ok { .. } => Err("memory service reply without a result".into()),
    }
}

/// A connection to a running memory service over a Unix domain socket.
/// Each call is one request and one reply within `deadline`; a slow or absent
/// service is reported to the caller, which answers from lexical retrieval.
#[cfg(unix)]
#[derive(Clone, Debug)]
pub struct Client {
    socket: PathBuf,
    deadline: Duration,
}

#[cfg(unix)]
impl Client {
    pub fn new(socket: PathBuf, deadline: Duration) -> Self {
        Self { socket, deadline }
    }

    /// A client for the service of `data_dir`, or `None` when no service has
    /// a socket there.
    pub fn for_data_dir(data_dir: &Path, deadline: Duration) -> Option<Self> {
        let socket = socket_path(data_dir);
        socket.exists().then(|| Self::new(socket, deadline))
    }

    pub fn call(&self, request: &Request) -> Result<Reply, String> {
        let stream = UnixStream::connect(&self.socket)
            .map_err(|e| format!("memory service unreachable: {e}"))?;
        stream
            .set_read_timeout(Some(self.deadline))
            .and_then(|()| stream.set_write_timeout(Some(self.deadline)))
            .map_err(|e| e.to_string())?;
        let line = encode_request(request).map_err(|e| e.to_string())?;
        (&stream)
            .write_all(line.as_bytes())
            .map_err(|e| format!("memory service write: {e}"))?;
        let mut reply = String::new();
        BufReader::new(&stream).read_line(&mut reply).map_err(|e| {
            format!(
                "memory service did not answer within {:?}: {e}",
                self.deadline
            )
        })?;
        decode_reply(&reply)
    }

    pub fn search(
        &self,
        query: &str,
        owners: &[String],
        source: Option<&str>,
        since_ms: Option<i64>,
        until_ms: Option<i64>,
        limit: usize,
    ) -> Result<(Vec<DenseHit>, Status), String> {
        match self.call(&Request::Search {
            query: query.into(),
            owners: owners.to_vec(),
            source: source.map(str::to_owned),
            since_ms,
            until_ms,
            limit,
        })? {
            Reply::Search { hits, status } => Ok((hits, status)),
            _ => Err("memory service answered another request".into()),
        }
    }

    /// One similarity per input position. No document IDs are sent to the service.
    pub fn dense_scores(&self, query: &str, passages: &[String]) -> Result<Vec<f32>, String> {
        check_dense_inputs(query, passages)?;
        match self.call(&Request::DenseScores {
            query: query.into(),
            passages: passages.to_vec(),
        })? {
            Reply::DenseScores { similarities }
                if similarities.len() == passages.len()
                    && similarities.iter().all(|score| score.is_finite()) =>
            {
                Ok(similarities)
            }
            Reply::DenseScores { .. } => Err("invalid scoped dense similarities".into()),
            _ => Err("memory service answered another request".into()),
        }
    }

    pub fn rerank(&self, query: &str, passages: &[String]) -> Result<Vec<f32>, String> {
        match self.call(&Request::Rerank {
            query: query.into(),
            passages: passages.to_vec(),
        })? {
            Reply::Rerank { scores } if scores.len() == passages.len() => Ok(scores),
            Reply::Rerank { .. } => Err("memory service scored another number of passages".into()),
            _ => Err("memory service answered another request".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_and_replies_round_trip_and_versions_are_checked() {
        let request = Request::Search {
            query: "點解要再註冊".into(),
            owners: vec!["agt_1".into()],
            source: Some("transcripts".into()),
            since_ms: None,
            until_ms: Some(5),
            limit: 10,
        };
        let line = encode_request(&request).unwrap();
        assert!(
            line.ends_with('\n') && line.contains(r#""v":1"#) && line.contains(r#""op":"search""#)
        );
        assert_eq!(decode_request(line.trim()).unwrap(), request);
        assert!(decode_request(&line.replace(r#""v":1"#, r#""v":2"#)).is_err());

        let reply = Reply::Rerank {
            scores: vec![0.5, -1.0],
        };
        assert_eq!(
            decode_reply(&encode_reply(&Ok(reply.clone()))).unwrap(),
            reply
        );
        let error = decode_reply(&encode_reply(&Err("model missing".into())));
        assert_eq!(error, Err("model missing".into()));
        let status = Reply::Search {
            hits: vec![DenseHit {
                document_id: "transcript:a:b:1".into(),
                agent_id: "agt_1".into(),
                offset: 10,
                length: 20,
                sha256: "ab".into(),
                score: 0.9,
            }],
            status: Status::default(),
        };
        assert_eq!(
            decode_reply(&encode_reply(&Ok(status.clone()))).unwrap(),
            status
        );
    }

    #[cfg(unix)]
    #[cfg(unix)]
    #[test]
    fn scoped_dense_contract_requires_bounded_inputs_and_one_score_per_passage() {
        use std::os::unix::net::UnixListener;

        let query = "automobile";
        let passages = vec!["vehicle maintenance".into(), "seed packets".into()];
        assert!(check_dense_inputs(query, &[]).is_err());
        assert!(
            check_dense_inputs(query, &vec!["valid".into(); SCOPED_DENSE_MAX_PASSAGES + 1])
                .is_err()
        );
        assert!(check_dense_inputs(query, &["x".repeat(SCOPED_DENSE_MAX_TEXT_BYTES + 1)]).is_err());
        assert!(check_dense_inputs(" ", &passages).is_err());
        let expected = Request::DenseScores {
            query: query.into(),
            passages: passages.clone(),
        };
        let dir = crate::test_tempdir();
        let path = socket_path(dir.path());
        let listener = UnixListener::bind(&path).unwrap();
        let worker = std::thread::spawn(move || {
            for similarities in [vec![0.91, -0.1], vec![0.91]] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut line = String::new();
                BufReader::new(&stream).read_line(&mut line).unwrap();
                assert_eq!(decode_request(&line).unwrap(), expected);
                stream
                    .write_all(encode_reply(&Ok(Reply::DenseScores { similarities })).as_bytes())
                    .unwrap();
            }
        });
        let client = Client::new(path, Duration::from_secs(2));
        assert_eq!(client.dense_scores(query, &passages).unwrap(), [0.91, -0.1]);
        assert!(client.dense_scores(query, &passages).is_err());
        worker.join().unwrap();
    }

    #[test]
    fn an_absent_service_is_an_error_not_a_wait() {
        let dir = crate::test_tempdir();
        assert!(Client::for_data_dir(dir.path(), Duration::from_secs(1)).is_none());
        let client = Client::new(dir.path().join("missing.sock"), Duration::from_secs(1));
        let started = std::time::Instant::now();
        assert!(client.rerank("q", &["p".into()]).is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
