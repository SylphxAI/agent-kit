//! Telegram Bot API updates used by long polls and webhooks.

use super::{BotCall, PollError, TgChat, TgMessage, TgUser};
use serde::Deserialize;
use serde_json::{Value, json};

/// Update subscriptions shared by polling and webhook registration.
pub const ALLOWED_UPDATES: &[&str] = &[
    "message",
    "edited_message",
    "my_chat_member",
    "message_reaction",
    "managed_bot",
];

/// The next long poll, starting at the caller's last persisted offset.
pub fn get_updates(offset: i64, timeout_s: u64) -> BotCall {
    BotCall {
        method: "getUpdates".into(),
        body: json!({"offset": offset, "timeout": timeout_s, "allowed_updates": ALLOWED_UPDATES}),
    }
}

/// Extract the update array from a Bot API result read by the host transport.
pub fn updates_payload(response: &str) -> Result<String, &'static str> {
    let value: Value =
        serde_json::from_str(response).map_err(|_| "telegram response is not json")?;
    match value.get("result") {
        Some(Value::Array(items)) => Ok(Value::Array(items.clone()).to_string()),
        _ => Err("telegram response has no update list"),
    }
}

/// Decode an entire batch before advancing any offset.
pub fn decode_updates(payload: &str) -> Result<Vec<Update>, PollError> {
    serde_json::from_str(payload).map_err(|e| PollError::BadPayload(e.to_string()))
}

#[derive(Clone, Debug, Deserialize)]
pub struct Update {
    pub update_id: i64,
    pub message: Option<TgMessage>,
    #[serde(default)]
    pub edited_message: Option<TgMessage>,
    /// The bot's own membership in a chat changed. This is how a bot learns it
    /// was added to, or removed from, a group.
    pub my_chat_member: Option<ChatMemberUpdate>,
    /// A reaction on a message in the chat changed (`message_reaction`).
    #[serde(default)]
    pub message_reaction: Option<TgMessageReactionUpdated>,
    /// A bot managed by this bot was created or its token was updated (`managed_bot`, Bot API 9.6+).
    #[serde(default)]
    pub managed_bot: Option<TgManagedBotUpdated>,
}

impl Update {
    // Match dispatch priority; another field cannot authorize this event.
    pub fn caller(&self) -> (Option<i64>, Option<i64>) {
        if let Some(member) = &self.my_chat_member {
            (member.from.as_ref().map(|u| u.id), Some(member.chat.id))
        } else if let Some(bot) = &self.managed_bot {
            (Some(bot.user.id), None)
        } else if let Some(reaction) = &self.message_reaction {
            (reaction.user.as_ref().map(|u| u.id), Some(reaction.chat.id))
        } else if let Some(msg) = self.message.as_ref().or(self.edited_message.as_ref()) {
            (msg.from.as_ref().map(|u| u.id), Some(msg.chat.id))
        } else {
            (None, None)
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct TgManagedBotUpdated {
    pub user: TgUser,
    pub bot: TgUser,
}

#[derive(Clone, Debug, Deserialize)]
pub struct TgMessageReactionUpdated {
    pub chat: TgChat,
    pub message_id: i64,
    pub date: i64,
    #[serde(default)]
    pub user: Option<TgUser>,
    #[serde(default)]
    pub new_reaction: Vec<TgReactionType>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct TgReactionType {
    #[serde(default)]
    pub emoji: Option<String>,
    #[serde(default)]
    pub custom_emoji_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ChatMemberUpdate {
    pub chat: TgChat,
    pub from: Option<TgUser>,
    pub new_chat_member: ChatMember,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ChatMember {
    pub status: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_batch_keeps_provider_details_and_caller_precedence() {
        let chat = json!({"id": -7, "type": "supergroup", "is_forum": true});
        let user = json!({"id": 42, "first_name": "Example"});
        let items = json!([
            {"update_id": 10, "edited_message": {"message_id": 3, "date": 1,
                "chat": chat, "from": user, "message_thread_id": 9, "text": "edited"}},
            {"update_id": 11, "message_reaction": {"chat": chat, "message_id": 3,
                "date": 2, "user": user, "new_reaction": [{"emoji":"👍"}]}},
            {"update_id": 12, "my_chat_member": {"chat": chat, "from": user,
                "new_chat_member": {"status":"left"}}, "managed_bot": {
                "user": {"id": 99}, "bot": {"id": 101}}},
            {"update_id": 13, "managed_bot": {"user": user,
                "bot": {"id": 101, "username":"example_bot"}}}
        ]);
        let batch = decode_updates(&items.to_string()).unwrap();
        assert_eq!(batch[0].caller(), (Some(42), Some(-7)));
        assert_eq!(
            batch[0].edited_message.as_ref().unwrap().forum_thread_id(),
            Some(9)
        );
        assert_eq!(
            batch[1].message_reaction.as_ref().unwrap().new_reaction[0]
                .emoji
                .as_deref(),
            Some("👍")
        );
        assert_eq!(batch[2].caller(), (Some(42), Some(-7)));
        assert_eq!(
            batch[2]
                .my_chat_member
                .as_ref()
                .unwrap()
                .new_chat_member
                .status,
            "left"
        );
        assert_eq!(batch[3].caller(), (Some(42), None));
        assert_eq!(batch[3].managed_bot.as_ref().unwrap().bot.id, 101);
        assert_eq!(
            get_updates(14, 15).body["allowed_updates"],
            json!(ALLOWED_UPDATES)
        );
        assert!(decode_updates(r#"[{"update_id": 1}, {"message": {}}]"#).is_err());
    }
}
