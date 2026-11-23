#![cfg(any(feature = "sqlite", feature = "postgres"))]

use agent_memory::{
    HistorySearchQuery, MemoryScope, MemorySearchRequest, MemoryStore, RawCitation, RawRecordInput,
    StoreError,
};

const ORIGINAL: &[u8] = b"original \xc3\xa9\x00\xff";
const REPLACEMENT: &[u8] = b"mutation \xc3\xa9\x00\xff";
const ORIGINAL_SHA: &str = "9b2053a20f5671cdea9396bb2702f74d2bb74cd5ea814808d87ea2be7f3e91c5";
const SLICE_SHA: &str = "8bad28186fc31b5c8eaf042018b010904cf098038cc21e3462848b88b55a0afa";

fn record(id: &str, owner: &str, bytes: &[u8]) -> RawRecordInput {
    RawRecordInput {
        id: id.into(),
        agent_id: owner.into(),
        source: MemoryScope::Memory,
        timestamp_ms: Some(1),
        direction: "in".into(),
        sender: "original sender".into(),
        destination: "test destination".into(),
        role: Some("user".into()),
        tool_name: None,
        tool_names: Vec::new(),
        raw_bytes: bytes.to_vec(),
    }
}

fn assert_conflict(result: Result<RawCitation, StoreError>) {
    assert!(
        matches!(&result, Err(StoreError::IntegrityMismatch(message))
        if message.contains("immutable record")),
        "{result:?}"
    );
}

async fn assert_retained(store: &impl MemoryStore, citation: &RawCitation) {
    let owner = [citation.agent_id.clone()];
    assert_eq!(
        store
            .read_citation(
                &owner,
                &citation.document_id,
                0,
                citation.length,
                Some(ORIGINAL_SHA)
            )
            .await
            .unwrap()
            .unwrap(),
        (citation.clone(), ORIGINAL.to_vec()),
    );
    let (partial, bytes) = store
        .read_citation(&owner, &citation.document_id, 9, 4, Some(SLICE_SHA))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(bytes, &ORIGINAL[9..]);
    assert_eq!(
        partial,
        RawCitation {
            offset: 9,
            length: 4,
            sha256: SLICE_SHA.into(),
            ..citation.clone()
        }
    );

    let foreign = ["other-owner".to_owned()];
    assert!(
        store
            .read_citation(&foreign, &citation.document_id, 0, citation.length, None)
            .await
            .unwrap()
            .is_none()
    );

    let mut req = MemorySearchRequest {
        owners: owner.to_vec(),
        scope: MemoryScope::Memory,
        query: HistorySearchQuery {
            query: Some("original".into()),
            ..Default::default()
        },
        ..Default::default()
    };
    let hits = store.search_lexical(&req).await.unwrap();
    let hit = hits
        .iter()
        .find(|hit| hit.document_id == citation.document_id)
        .unwrap();
    assert_eq!(hit.metadata.sender, "original sender");
    assert_eq!(hit.metadata.timestamp_ms, Some(1));
    assert_eq!(&hit.citation, citation);
    req.owners = foreign.to_vec();
    assert!(store.search_lexical(&req).await.unwrap().is_empty());
}

async fn assert_parity(
    store: &impl MemoryStore,
    original: &RawRecordInput,
    citation: &RawCitation,
) {
    assert_eq!(citation.document_id, original.id);
    assert_eq!(citation.agent_id, original.agent_id);
    assert_eq!(citation.offset, 0);
    assert_eq!(citation.length, ORIGINAL.len() as u64);
    assert_eq!(citation.sha256, ORIGINAL_SHA);
    assert_eq!(citation.raw_sha256, ORIGINAL_SHA);
    assert_retained(store, citation).await;

    let mut retry = original.clone();
    retry.timestamp_ms = Some(2);
    retry.sender = "retry sender".into();
    assert_eq!(&store.append_record(&retry).await.unwrap(), citation);
    assert_retained(store, citation).await;

    for (owner, bytes) in [
        (original.agent_id.as_str(), REPLACEMENT),
        ("other-owner", ORIGINAL),
        ("other-owner", REPLACEMENT),
    ] {
        assert_conflict(
            store
                .append_record(&record(&original.id, owner, bytes))
                .await,
        );
        assert_retained(store, citation).await;
    }
}

#[cfg(feature = "sqlite")]
fn sqlite_fixture() -> tempfile::TempDir {
    tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap()
}

#[cfg(feature = "sqlite")]
fn segment_snapshot(
    root: &std::path::Path,
) -> std::collections::BTreeMap<std::ffi::OsString, Vec<u8>> {
    std::fs::read_dir(root.join("segments"))
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (entry.file_name(), std::fs::read(entry.path()).unwrap())
        })
        .collect()
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn sqlite_immutable_append() {
    use agent_memory::SqliteStore;

    let dir = sqlite_fixture();
    let store = SqliteStore::open(dir.path()).unwrap();
    let original = record("immutable", "original-owner", ORIGINAL);
    let citation = store.append_record(&original).await.unwrap();
    drop(store);
    let segments = segment_snapshot(dir.path());
    assert!(!segments.is_empty());

    let reopened = SqliteStore::open(dir.path()).unwrap();
    assert_parity(&reopened, &original, &citation).await;
    assert_eq!(segment_snapshot(dir.path()), segments);
    assert_eq!(reopened.stats().await.unwrap().documents, 1);
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn sqlite_first_append_is_serialized_across_handles() {
    use agent_memory::SqliteStore;
    use std::sync::Barrier;

    let dir = sqlite_fixture();
    let left = SqliteStore::open(dir.path()).unwrap();
    let right = SqliteStore::open(dir.path()).unwrap();
    let a = record("concurrent", "owner-a", ORIGINAL);
    let b = record("concurrent", "owner-b", REPLACEMENT);
    let barrier = Barrier::new(2);
    let (result_a, result_b) = std::thread::scope(|scope| {
        let worker = scope.spawn(|| {
            barrier.wait();
            left.append_record_sync(&a)
        });
        barrier.wait();
        let result_b = right.append_record_sync(&b);
        (worker.join().unwrap(), result_b)
    });
    let (winner, citation) = sole_winner(a, b, result_a, result_b);
    // Exactly one framed record and its acknowledgement file; no losing segment.
    assert_eq!(segment_snapshot(dir.path()).len(), 2);
    assert_eq!(left.stats().await.unwrap().documents, 1);
    assert_winner(&left, &winner, citation).await;
}

fn sole_winner(
    a: RawRecordInput,
    b: RawRecordInput,
    result_a: Result<RawCitation, StoreError>,
    result_b: Result<RawCitation, StoreError>,
) -> (RawRecordInput, RawCitation) {
    match (result_a, result_b) {
        (Ok(citation), refused) => {
            assert_conflict(refused);
            (a, citation)
        }
        (refused, Ok(citation)) => {
            assert_conflict(refused);
            (b, citation)
        }
        (a, b) => panic!("expected one committed append, got {a:?} and {b:?}"),
    }
}

async fn assert_winner(store: &impl MemoryStore, winner: &RawRecordInput, citation: RawCitation) {
    let replay = store.append_record(winner).await.unwrap();
    assert_eq!(replay, citation);
    let readback = store
        .read_citation(
            std::slice::from_ref(&winner.agent_id),
            &winner.id,
            0,
            citation.length,
            Some(&citation.sha256),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(readback, (citation, winner.raw_bytes.clone()));
}

#[cfg(feature = "postgres")]
#[tokio::test]
#[ignore = "requires AGENT_MEMORY_TEST_POSTGRES_URL pointing at a disposable PostgreSQL database with pgvector"]
async fn postgres_immutable_append() {
    use agent_memory::PostgresPgvectorStore;

    let url =
        std::env::var("AGENT_MEMORY_TEST_POSTGRES_URL").expect("disposable PostgreSQL fixture URL");
    let store = PostgresPgvectorStore::connect(&url)
        .await
        .expect("connect to fixture");
    store.migrate().await.unwrap();
    let id = uuid::Uuid::now_v7();
    let original = record(&format!("immutable-{id}"), &format!("owner-{id}"), ORIGINAL);
    let citation = store.append_record(&original).await.unwrap();
    // Unchanged row version also detects an UPDATE that writes identical values.
    async fn row_version(store: &PostgresPgvectorStore, id: &str) -> String {
        sqlx::query_scalar("SELECT xmin::text FROM memory_records WHERE id = $1")
            .bind(id)
            .fetch_one(store.pool())
            .await
            .unwrap()
    }
    let version = row_version(&store, &original.id).await;
    let second = PostgresPgvectorStore::connect(&url)
        .await
        .expect("independent fixture connection");
    assert_parity(&second, &original, &citation).await;
    assert_eq!(row_version(&second, &original.id).await, version);

    let a = record(&format!("concurrent-{id}"), "owner-a", ORIGINAL);
    let b = record(&a.id, "owner-b", REPLACEMENT);
    let (result_a, result_b) = tokio::join!(store.append_record(&a), second.append_record(&b));
    let (winner, citation) = sole_winner(a, b, result_a, result_b);
    assert_winner(&second, &winner, citation).await;
}
