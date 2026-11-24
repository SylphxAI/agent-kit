//! Scope proof for JSON-only Telegram Bot API passthrough.

use serde_json::{Map, Value};

use super::BotCall;

/// Telegram accepts unknown parameters, so the presence of chat_id is not a
/// scope proof. Admit only methods documented to operate in that chat, and
/// reject all other authority selectors. Global read-only exceptions are explicit.
/// Reference: <https://core.telegram.org/bots/api>
pub fn scoped_call(
    method: &str,
    params: Option<Value>,
    chat_id: i64,
    thread_id: Option<i32>,
) -> Result<BotCall, &'static str> {
    let bytes = method.as_bytes();
    if !(2..=64).contains(&bytes.len())
        || !bytes[0].is_ascii_alphabetic()
        || !bytes.iter().all(u8::is_ascii_alphanumeric)
    {
        return Err("channel_api method must be 2–64 ASCII alphanumerics starting with a letter");
    }
    if [
        "setWebhook",
        "deleteWebhook",
        "getWebhookInfo",
        "getUpdates",
        "logOut",
        "close",
    ]
    .iter()
    .any(|denied| method.eq_ignore_ascii_case(denied))
    {
        return Err("Telegram control-plane method is not available through channel_api");
    }
    let (canonical, scope) = METHODS
        .iter()
        .find(|(name, _)| method.eq_ignore_ascii_case(name))
        .ok_or("method is not admitted by the Telegram single-chat policy")?;
    match (scope, thread_id) {
        (Scope::Topic, None) => return Err("method requires a topic target"),
        (Scope::Chat | Scope::BotRead, Some(_)) => {
            return Err("method requires a bare chat target");
        }
        _ => {}
    }
    let mut body = match params {
        None => Map::new(),
        Some(Value::Object(body)) => body,
        _ => return Err("channel_api params must be a JSON object"),
    };
    bind_id(&mut body, "chat_id", Some(chat_id))?;
    bind_id(&mut body, "message_thread_id", thread_id.map(i64::from))?;
    if matches!(scope, Scope::BotRead) {
        body.remove("chat_id");
        if !body.is_empty() {
            return Err("getForumTopicIconStickers takes no parameters");
        }
    }
    if body.keys().any(|key| {
        [
            "allow_paid_broadcast",
            "suggested_post_parameters",
            "explanation_media",
            "media",
            "rich_message",
        ]
        .iter()
        .any(|field| key.eq_ignore_ascii_case(field))
    }) {
        return Err("paid operations and native media are not available through channel_api");
    }
    if method.eq_ignore_ascii_case("sendPoll")
        && let Some(options) = body.get("options")
        && !options.is_array()
    {
        return Err("sendPoll options must be a JSON array");
    }
    if thread_id.is_some()
        && body
            .keys()
            .any(|key| key.eq_ignore_ascii_case("reply_to_message_id"))
    {
        return Err("reply_to_message_id is not supported with a topic target");
    }
    if body
        .get("reply_parameters")
        .is_some_and(|value| thread_id.is_some() || !value.is_object())
    {
        return Err(
            "reply_parameters must be an object and is only supported with a bare chat target",
        );
    }
    if body.iter().any(|(key, value)| {
        (!matches!(key.as_str(), "chat_id" | "message_thread_id") && is_authority(key))
            || has_nested_authority(value)
    }) {
        return Err(
            "Telegram params cannot select another chat, topic, inline message, or business connection",
        );
    }
    Ok(BotCall {
        method: (*canonical).into(),
        body: Value::Object(body),
    })
}

fn bind_id(
    body: &mut Map<String, Value>,
    key: &str,
    expected: Option<i64>,
) -> Result<(), &'static str> {
    if let Some(value) = body.get(key) {
        let actual = value.as_i64().or_else(|| value.as_str()?.parse().ok());
        if actual.is_none() || actual != expected {
            return Err(match key {
                "chat_id" => "params.chat_id must match the target chat ID",
                _ => "params.message_thread_id must match a topic ID in target",
            });
        }
    }
    if let Some(id) = expected {
        body.insert(key.into(), Value::from(id));
    }
    Ok(())
}

fn is_authority(key: &str) -> bool {
    [
        "chat_id",
        "from_chat_id",
        "message_thread_id",
        "direct_messages_topic_id",
        "business_connection_id",
        "inline_message_id",
        "ephemeral_message_parameters", // Can target a callback query rather than chat_id.
        "media", // Poll options can carry files even when the request itself is JSON.
    ]
    .iter()
    .any(|name| key.eq_ignore_ascii_case(name))
}

fn has_nested_authority(value: &Value) -> bool {
    match value {
        Value::Array(items) => items.iter().any(has_nested_authority),
        Value::Object(map) => map
            .iter()
            .any(|(key, value)| is_authority(key) || has_nested_authority(value)),
        _ => false,
    }
}

#[derive(Clone, Copy)]
enum Scope {
    Chat,
    Topic,
    ChatOrTopic,
    BotRead,
}

use Scope::{BotRead, Chat, ChatOrTopic, Topic};

// Topic targets are accepted only when the method actually consumes a thread
// ID. A Telegram message ID identifies a message in the whole chat, not a topic.
const METHODS: &[(&str, Scope)] = &[
    ("createForumTopic", Chat),
    ("editForumTopic", Topic),
    ("closeForumTopic", Topic),
    ("reopenForumTopic", Topic),
    ("deleteForumTopic", Topic),
    ("unpinAllForumTopicMessages", Topic),
    ("editGeneralForumTopic", Chat),
    ("closeGeneralForumTopic", Chat),
    ("reopenGeneralForumTopic", Chat),
    ("hideGeneralForumTopic", Chat),
    ("unhideGeneralForumTopic", Chat),
    ("unpinAllGeneralForumTopicMessages", Chat),
    ("getForumTopicIconStickers", BotRead),
    ("getChat", Chat),
    ("getChatAdministrators", Chat),
    ("getChatMemberCount", Chat),
    ("getChatMember", Chat),
    ("setChatTitle", Chat),
    ("setChatDescription", Chat),
    ("deleteChatPhoto", Chat),
    ("setChatPermissions", Chat),
    ("banChatMember", Chat),
    ("unbanChatMember", Chat),
    ("restrictChatMember", Chat),
    ("promoteChatMember", Chat),
    ("setChatAdministratorCustomTitle", Chat),
    ("banChatSenderChat", Chat),
    ("unbanChatSenderChat", Chat),
    ("setChatStickerSet", Chat),
    ("deleteChatStickerSet", Chat),
    ("leaveChat", Chat),
    ("getUserChatBoosts", Chat),
    ("exportChatInviteLink", Chat),
    ("createChatInviteLink", Chat),
    ("editChatInviteLink", Chat),
    ("revokeChatInviteLink", Chat),
    ("approveChatJoinRequest", Chat),
    ("declineChatJoinRequest", Chat),
    ("pinChatMessage", Chat),
    ("unpinChatMessage", Chat),
    ("unpinAllChatMessages", Chat),
    ("sendChatAction", ChatOrTopic),
    ("setMessageReaction", Chat),
    ("stopPoll", Chat),
    ("deleteMessage", Chat),
    ("deleteMessages", Chat),
    ("editMessageText", Chat),
    ("editMessageCaption", Chat),
    ("editMessageReplyMarkup", Chat),
    ("sendMessage", ChatOrTopic),
    ("sendPoll", ChatOrTopic),
    ("sendLocation", ChatOrTopic),
    ("sendVenue", ChatOrTopic),
    ("sendContact", ChatOrTopic),
    ("sendDice", ChatOrTopic),
];

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn native_scope_rejects_control_plane_global_and_unknown_methods() {
        for method in [
            "sEtWeBhOoK",
            "deleteWebhook",
            "getWebhookInfo",
            "getUpdates",
            "LoGoUt",
            "ClOsE",
        ] {
            assert_eq!(
                scoped_call(method, None, -42, None).unwrap_err(),
                "Telegram control-plane method is not available through channel_api"
            );
        }
        for method in [
            "",
            "g",
            "../getMe",
            "createForumTopic?x=1",
            "getChat#x",
            "4getChat",
            "getChat ",
            "getChät",
            "setMyCommands",
            "getMe",
            "copyMessage",
            "forwardMessage",
            "getFile",
            &"m".repeat(65),
        ] {
            assert!(scoped_call(method, None, -42, None).is_err(), "{method}");
        }
        assert_eq!(
            scoped_call("GETCHAT", None, -42, None).unwrap().method,
            "getChat"
        );
    }

    #[test]
    fn native_scope_blocks_secondary_authorities_and_spend() {
        for params in [
            json!({"chat_id":"@elsewhere"}),
            json!({"CHAT_ID":-43}),
            json!({"chat_id":-42.0}),
            json!({"message_thread_id":17}),
            json!({"from_chat_id":-43}),
            json!({"business_connection_id":"other"}),
            json!({"inline_message_id":"other"}),
            json!({"direct_messages_topic_id":17}),
            json!({"ephemeral_message_parameters":{"callback_query_id":17}}),
            json!({"reply_parameters":{"message_id":1,"chat_id":-43}}),
            json!({"reply_parameters":"{\"chat_id\":-43}"}),
            json!({"allow_paid_broadcast":true}),
            json!({"suggested_post_parameters":{"price":{"currency":"XTR","amount":1}}}),
        ] {
            assert!(
                scoped_call("sendMessage", Some(params.clone()), -42, None).is_err(),
                "{params}"
            );
        }
        assert!(
            scoped_call(
                "sendMessage",
                Some(json!({"reply_to_message_id":1})),
                -42,
                Some(17)
            )
            .is_err()
        );
        for options in [
            json!([{"text":"Option","media":{"type":"photo","media":"file:///example"}}]),
            json!("[]"),
        ] {
            assert!(scoped_call("sendPoll", Some(json!({"options": options})), -42, None).is_err());
        }
        assert!(scoped_call("editForumTopic", None, -42, None).is_err());
        assert!(scoped_call("deleteMessage", None, -42, Some(17)).is_err());
        assert!(scoped_call("createForumTopic", None, -42, Some(17)).is_err());
    }
}
