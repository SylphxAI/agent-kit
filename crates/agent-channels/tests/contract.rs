//! End-to-end: inbound Telegram message -> stored metadata -> reply route ->
//! send payload, and the speech gate for the same turn.

use agent_channels::reply_target::{ReplyTarget, resolve_reply_target};
use agent_channels::speech_gate::{
    OutboundSpeechMode, SuppressReason, TerminalDeliveryDecision, TerminalDeliveryInput,
    context_only_from_metadata, decide_terminal_text_delivery,
};
use agent_channels::telegram::{BotIdentity, TelegramInboundMeta};
use serde_json::{Value, json};

fn bot() -> BotIdentity {
    BotIdentity::new(Some("example_bot"), Some(999))
}

fn route_and_gate(
    message: &Value,
    mode: OutboundSpeechMode,
    text: &str,
) -> (Value, TerminalDeliveryDecision) {
    let meta = TelegramInboundMeta::from_message(message, &bot()).expect("parsable");
    // Hosts persist metadata as JSON; round-trip it as a queue would.
    let stored: Value = serde_json::from_str(&meta.reply_metadata().to_string()).unwrap();
    let target = resolve_reply_target(Some(&stored), None).expect("route");
    let body = target
        .telegram_send_message_payload(text, Some(meta.message_id))
        .unwrap();
    let decision = decide_terminal_text_delivery(&TerminalDeliveryInput {
        mode,
        context_only: context_only_from_metadata(&stored),
        already_sent_via_tool: false,
        text,
    });
    (body, decision)
}

#[test]
fn addressed_forum_topic_message_replies_in_its_topic() {
    let message = json!({
        "message_id": 10,
        "message_thread_id": 54007,
        "is_topic_message": true,
        "chat": {"id": -1001, "type": "supergroup", "is_forum": true},
        "from": {"id": 5, "is_bot": false},
        "text": "@example_bot what is the status?",
    });
    let (body, decision) = route_and_gate(
        &message,
        OutboundSpeechMode::DirectTerminalText,
        "All green.",
    );
    assert_eq!(body["chat_id"], -1001);
    assert_eq!(body["message_thread_id"], 54007);
    assert_eq!(decision, TerminalDeliveryDecision::Deliver);
}

#[test]
fn unaddressed_topic_message_is_heard_but_terminal_text_is_suppressed() {
    let message = json!({
        "message_id": 11,
        "message_thread_id": 54007,
        "is_topic_message": true,
        "chat": {"id": -1001, "type": "supergroup", "is_forum": true},
        "from": {"id": 5, "is_bot": false},
        "text": "lunch at noon?",
    });
    let (body, decision) = route_and_gate(
        &message,
        OutboundSpeechMode::DirectTerminalText,
        "Private note: people are planning lunch.",
    );
    // Route still keeps the topic for any explicit tool send.
    assert_eq!(body["message_thread_id"], 54007);
    assert_eq!(
        decision,
        TerminalDeliveryDecision::Suppress(SuppressReason::ContextOnly)
    );
}

#[test]
fn reply_chain_in_plain_supergroup_goes_to_the_chat_without_thread() {
    let message = json!({
        "message_id": 12,
        "message_thread_id": 7,
        "chat": {"id": -1002, "type": "supergroup"},
        "from": {"id": 5, "is_bot": false},
        "reply_to_message": {"message_id": 7, "from": {"id": 999, "is_bot": true}},
        "text": "thanks",
    });
    let (body, decision) = route_and_gate(
        &message,
        OutboundSpeechMode::DirectTerminalText,
        "You're welcome.",
    );
    assert!(body.get("message_thread_id").is_none());
    assert_eq!(decision, TerminalDeliveryDecision::Deliver);
}

#[test]
fn tool_only_agent_never_speaks_through_terminal_text() {
    let message = json!({
        "message_id": 13,
        "chat": {"id": 5, "type": "private"},
        "from": {"id": 5, "is_bot": false},
        "text": "hi",
    });
    let (_, decision) = route_and_gate(&message, OutboundSpeechMode::ToolOnly, "Hello!");
    assert_eq!(
        decision,
        TerminalDeliveryDecision::Suppress(SuppressReason::ToolOnly)
    );
}

#[test]
fn legacy_metadata_shapes_resolve_to_one_target() {
    let expected = ReplyTarget::telegram(-1001, Some(54007));
    for meta in [
        json!({"replyChannel": "telegram", "replyTarget": "-1001", "messageThreadId": 54007}),
        json!({"replyChannel": "telegram", "replyTarget": "-1001", "messageThreadId": "54007"}),
        json!({"replyChannel": "telegram", "replyTarget": "-1001", "replyThreadId": "54007"}),
        json!({"replyChannel": "telegram", "replyTarget": "-1001", "replyThreadId": null, "messageThreadId": 54007}),
        json!({"replyChannel": "telegram", "replyTarget": "-1001:topic:54007"}),
    ] {
        assert_eq!(
            resolve_reply_target(Some(&meta), None),
            Some(expected.clone()),
            "{meta}"
        );
    }
    let bare = json!({"replyChannel": "telegram", "replyTarget": "-1001"});
    assert_eq!(
        resolve_reply_target(Some(&bare), Some("telegram:example:-1001:topic:54007")),
        Some(expected)
    );
}
