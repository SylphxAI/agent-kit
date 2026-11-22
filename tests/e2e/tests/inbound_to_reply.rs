use std::fs::{self, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::sync::Arc;

use agent_channels::reply_target::{ReplyTarget, resolve_reply_target};
use agent_channels::speech_gate::{
    OutboundSpeechMode, SuppressReason, TerminalDeliveryDecision, TerminalDeliveryInput,
    decide_terminal_text_delivery,
};
use agent_channels::telegram::{BotIdentity, TelegramInboundMeta};
use agent_ingress::{
    BlockMatch, BlockScope, ChatType, InMemoryIngressBlockStore, IngressBlockRule,
    IngressBlockStore, IngressDecision, SenderIdentity, SenderRelationship, ValidatedBlockRule,
};
use agent_memory::{
    EmbeddingChunk, HistorySearchQuery, MemoryScope, MemorySearchRequest, MemoryStore, RawSegment,
    SqliteStore, StoreError,
};
use agent_tools::code_mode::protocol::ERR_METHOD_NOT_FOUND;
use agent_tools::code_mode::{ToolCallRequest, ToolRegistry};
use agent_tools::{LocalWorkspace, skills};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const AGENT: &str = "example-agent";
const CHANNEL: &str = "telegram:example-account";
const NOW_MS: i64 = 1_000;
const CHAT: i64 = -1001;
const TOPIC: i64 = 42;
const TEXT: &str = "@example_bot inspect the garden archive";
const SKILL: &str = "---
name: garden
description: Read the garden archive.
---
# Garden
";

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

struct Wake {
    context_only: bool,
    target: ReplyTarget,
    awareness: String,
}

fn message(message_id: i64, sender_id: i64) -> Value {
    json!({
        "message_id": message_id,
        "message_thread_id": TOPIC,
        "is_topic_message": true,
        "chat": {"id": CHAT, "type": "supergroup", "is_forum": true},
        "from": {"id": sender_id, "is_bot": false, "first_name": "Owner"},
        "text": TEXT,
        "entities": [{"type": "mention", "offset": 0, "length": 12}]
    })
}

// Tiny host fixture: the event envelope and relationship have already been
// verified by the host. The sender-editable first name does not confer authority.
async fn receive(
    raw_event: &[u8],
    blocks: &InMemoryIngressBlockStore,
    memory: &SqliteStore,
    wakes: &mut Vec<Wake>,
) -> TestResult<IngressDecision> {
    let event: Value = serde_json::from_slice(raw_event)?;
    let bot = BotIdentity::new(Some("example_bot"), None);
    let meta = TelegramInboundMeta::from_message(&event, &bot).expect("valid forum fixture");
    assert!(meta.addressed);
    let sender = SenderIdentity {
        channel: CHANNEL.into(),
        sender_id: meta.sender_id.expect("fixture sender").to_string(),
        chat_id: Some(meta.chat_id.to_string()),
        chat_type: ChatType::Group,
        username: None,
        display_name: event["from"]["first_name"].as_str().map(str::to_owned),
        relationship: SenderRelationship::Stranger,
    };
    let decision = blocks.is_blocked(&sender, NOW_MS).await?;
    if decision != IngressDecision::Allow {
        return Ok(decision);
    }
    let target = resolve_reply_target(Some(&meta.reply_metadata()), None).expect("forum route");
    memory
        .append_record(&agent_memory::RawRecordInput {
            id: format!("message-{}", meta.message_id),
            agent_id: AGENT.into(),
            source: MemoryScope::Transcripts,
            timestamp_ms: Some(NOW_MS),
            direction: "in".into(),
            sender: sender.sender_id.clone(),
            destination: AGENT.into(),
            role: Some("user".into()),
            tool_name: None,
            tool_names: vec![],
            raw_bytes: raw_event.to_vec(),
        })
        .await?;
    wakes.push(Wake {
        context_only: meta.context_only,
        target,
        awareness: sender.format_awareness_header(),
    });
    Ok(decision)
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[tokio::test]
async fn inbound_forum_message_to_verified_memory_skill_and_reply() -> TestResult {
    let temp = tempfile::tempdir()?;
    let memory = SqliteStore::open(temp.path().join("memory"))?;
    let blocks = InMemoryIngressBlockStore::new();
    let blocked = BlockMatch {
        channel: CHANNEL.into(),
        scope: BlockScope::Sender {
            sender_id: "9".into(),
        },
    };
    blocks
        .block(ValidatedBlockRule::new(
            IngressBlockRule {
                rule_id: "blocked-sender".into(),
                target: blocked.clone(),
                reason: None,
                blocked_by: "example-admin".into(),
                created_at_ms: NOW_MS - 1,
                expires_at_ms: None,
            },
            &blocked,
            SenderRelationship::Stranger,
        )?)
        .await?;

    let mut wakes = Vec::new();
    assert_eq!(
        receive(
            &serde_json::to_vec(&message(1, 9))?,
            &blocks,
            &memory,
            &mut wakes
        )
        .await?,
        IngressDecision::Drop {
            rule_id: "blocked-sender".into(),
            reason: None
        }
    );
    assert!(wakes.is_empty());
    assert_eq!(memory.stats().await?.documents, 0);

    let expected = serde_json::to_vec_pretty(&message(2, 7))?;
    assert_eq!(
        receive(&expected, &blocks, &memory, &mut wakes).await?,
        IngressDecision::Allow
    );
    let [wake] = wakes.as_slice() else {
        panic!("exactly one wake expected")
    };
    let header: Value =
        serde_json::from_str(wake.awareness.strip_prefix("sender_awareness: ").unwrap())?;
    assert_eq!(
        header,
        json!({"relationship": "stranger", "channel": CHANNEL, "sender_id": "7", "chat_id": "-1001", "chat_type": "group"})
    );
    assert_eq!(wake.target, ReplyTarget::telegram(CHAT, Some(TOPIC)));

    // Fixed vectors exercise dense retrieval and fusion without a model or network.
    let digest = sha256(&expected);
    memory
        .upsert_embeddings(
            "message-2",
            "fixture",
            &[EmbeddingChunk {
                piece_index: 0,
                offset: 0,
                length: expected.len() as u64,
                sha256: digest.clone(),
                text_sha256: sha256(TEXT.as_bytes()),
                vector: vec![1.0, 0.0],
            }],
        )
        .await?;
    let search = MemorySearchRequest {
        owners: vec![AGENT.into()],
        scope: MemoryScope::Transcripts,
        query: HistorySearchQuery {
            query: Some("garden archive".into()),
            ..Default::default()
        },
        query_vector: Some(vec![1.0, 0.0]),
        vector_space: Some("fixture".into()),
    };
    let hits = memory.search_hybrid(&search).await?;
    let [hit] = hits.as_slice() else {
        panic!("exactly one hybrid hit expected: {hits:?}")
    };
    assert_eq!(hit.document_id, "message-2");
    assert!(hit.lexical_score.is_some() && hit.dense_score.is_some());
    assert_eq!(hit.raw_slice, expected);
    let cite = &hit.citation;
    let verified = memory
        .read_citation(
            &search.owners,
            &cite.document_id,
            cite.offset,
            cite.length,
            Some(&cite.sha256),
        )
        .await?;
    assert_eq!(cite.sha256, digest);
    let (verified_cite, verified_bytes) = verified.expect("authorized citation");
    assert_eq!(&verified_cite, cite);
    assert_eq!(verified_bytes, expected);
    let recalled_event: Value = serde_json::from_slice(&verified_bytes)?;
    assert_eq!(recalled_event["text"], TEXT);

    // Mount only the workspace namespace; the host selects a local skill, not an
    // arbitrary tool call from the inbound message.
    let workspace = temp.path().join("workspace");
    let skill_root = workspace.join("skills");
    fs::create_dir_all(skill_root.join("garden"))?;
    fs::write(skill_root.join("garden/SKILL.md"), SKILL)?;
    let catalog = skills::catalog(&[skill_root]);
    let skill = catalog
        .iter()
        .find(|entry| {
            entry.slug == "garden"
                && recalled_event["text"] == "@example_bot inspect the garden archive"
        })
        .expect("host-approved skill matches recalled request");
    let path = skill.skill_file.strip_prefix(&workspace)?;
    let dispatcher = ToolRegistry::new()
        .with_workspace(Arc::new(LocalWorkspace::new(workspace.clone())))
        .into_dispatcher();
    let params = json!({"path": path});
    assert_eq!(
        ToolCallRequest::from_compound("invalid", ".read", params.clone())
            .unwrap_err()
            .code,
        ERR_METHOD_NOT_FOUND
    );
    let refused = dispatcher
        .dispatch_one(
            ToolCallRequest::from_compound("unknown", "unmounted.read", params.clone()).unwrap(),
        )
        .await;
    assert!(!refused.ok && refused.result.is_none());
    assert_eq!(refused.error.unwrap().code, ERR_METHOD_NOT_FOUND);
    let response = dispatcher
        .dispatch_one(ToolCallRequest::from_compound("skill", "workspace.read", params).unwrap())
        .await;
    assert!(response.ok, "{:?}", response.error);
    assert_eq!(response.result.unwrap()["content"], SKILL);

    let mut speech = TerminalDeliveryInput {
        mode: OutboundSpeechMode::DirectTerminalText,
        context_only: wake.context_only,
        already_sent_via_tool: false,
        text: &skill.description,
    };
    assert_eq!(
        decide_terminal_text_delivery(&speech),
        TerminalDeliveryDecision::Deliver
    );
    let payload = wake
        .target
        .telegram_send_message_payload(speech.text, Some(2))?;
    assert_eq!(
        payload,
        json!({"chat_id": CHAT, "message_thread_id": TOPIC, "text": "Read the garden archive.", "reply_parameters": {"message_id": 2}})
    );
    speech.already_sent_via_tool = true;
    assert_eq!(
        decide_terminal_text_delivery(&speech),
        TerminalDeliveryDecision::Suppress(SuppressReason::AlreadySentViaTool)
    );

    // Change a payload byte in the disposable segment, not in the citation or index.
    let segment_dir = memory.root().join("segments");
    let segments = fs::read_dir(segment_dir)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()?;
    let path = segments
        .iter()
        .find(|p| p.extension().is_some_and(|ext| ext == "raw"))
        .expect("raw segment written");
    let raw = RawSegment::open(path)?.records()?;
    let [coordinate] = raw.as_slice() else {
        panic!("exactly one acknowledged record expected")
    };
    assert_eq!(coordinate.sha256, digest);
    let mut file = OpenOptions::new().write(true).open(path)?;
    file.seek(SeekFrom::Start(coordinate.offset))?;
    file.write_all(b"[")?;
    file.sync_all()?;
    drop(file);
    let tampered = memory
        .read_citation(
            &search.owners,
            &cite.document_id,
            cite.offset,
            cite.length,
            Some(&cite.sha256),
        )
        .await;
    assert!(
        matches!(tampered, Err(StoreError::IntegrityMismatch(_))),
        "{tampered:?}"
    );
    Ok(())
}
