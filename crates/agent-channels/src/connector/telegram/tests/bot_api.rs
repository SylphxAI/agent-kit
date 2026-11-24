use super::super::{
    BotIdentity, PollError, bot_identity, get_me, managed_bot_token_of, send_message,
    send_plain_message, sent_message_id,
};

#[test]
fn send_message_names_the_chat_and_keeps_the_token_out() {
    let call = send_message(100, "hello", Some(5), None);
    assert_eq!(call.method, "sendRichMessage");
    assert_eq!(call.body["chat_id"], 100);
    assert_eq!(call.body["rich_message"]["markdown"], "hello");
    assert_eq!(call.body["reply_parameters"]["message_id"], 5);
    assert_eq!(call.body["reply_to_message_id"], 5);
    assert!(call.body.get("message_thread_id").is_none());
    let id = sent_message_id(r#"{"ok":true,"result":{"message_id":9}}"#).unwrap();
    assert_eq!(id, 9);
}

#[test]
fn a_refused_call_reports_telegrams_own_words() {
    let refused = sent_message_id(
        r#"{"ok":false,"error_code":400,"description":"Bad Request: message thread not found"}"#,
    )
    .unwrap_err();
    assert!(matches!(refused, PollError::Refused(_)));
    assert!(refused.to_string().contains("message thread not found"));
}

#[test]
fn get_me_names_the_bot_without_its_token() {
    let identity = bot_identity(
        r#"{"ok":true,"result":{"id":777,"is_bot":true,"first_name":"Ada","username":"ada_bot"}}"#,
    )
    .unwrap();
    assert_eq!(
        identity,
        BotIdentity {
            id: 777,
            username: "ada_bot".into(),
            can_manage_bots: false,
        }
    );
    assert_eq!(get_me().method, "getMe");
    let manager = bot_identity(
        r#"{"ok":true,"result":{"id":888,"is_bot":true,"first_name":"Manager","username":"manager_bot","can_manage_bots":true}}"#,
    )
    .unwrap();
    assert!(manager.can_manage_bots);
    assert_eq!(
        managed_bot_token_of(r#"{"ok":true,"result":"999:AAExampleManagedToken"}"#).unwrap(),
        "999:AAExampleManagedToken"
    );
}

#[test]
fn send_message_sends_native_telegram_rich_message_markdown() {
    let input =
        "## Status\n| # | Owner | Model |\n|---|---|---|\n| **1** | **Team A** | `Model A` |\n";
    let call = send_message(-100200300400, input, Some(42), None);
    assert_eq!(call.method, "sendRichMessage");
    assert_eq!(call.body["rich_message"]["markdown"], input);
    assert_eq!(call.body["reply_parameters"]["message_id"], 42);
    assert_eq!(call.body["reply_to_message_id"], 42);
}

#[test]
fn send_payloads_keep_only_positive_topics() {
    let plain = send_plain_message(-1001, "answer", Some(55), Some(7));
    assert_eq!(plain.method, "sendMessage");
    assert_eq!(
        plain.body,
        serde_json::json!({"chat_id": -1001, "text": "answer", "reply_parameters": {"message_id": 55}, "message_thread_id": 7})
    );
    for id in [None, Some(0), Some(-7)] {
        assert!(
            send_plain_message(-1001, "answer", None, id)
                .body
                .get("message_thread_id")
                .is_none()
        );
        assert!(
            send_message(-1001, "answer", None, id)
                .body
                .get("message_thread_id")
                .is_none()
        );
    }
}
