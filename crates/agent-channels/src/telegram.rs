//! Telegram Bot API inbound parsing.
//!
//! Every function takes the raw `Message` object (`update.message`,
//! `update.edited_message`, ...) as a [`serde_json::Value`], so hosts keep
//! their own Bot API types and HTTP clients.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The kind of chat a message was sent in (`chat.type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TelegramChatType {
    /// A one-to-one chat with the bot.
    Private,
    /// A basic group.
    Group,
    /// A supergroup, with or without forum topics.
    Supergroup,
    /// A broadcast channel.
    Channel,
    /// Missing or not a type this crate knows.
    Unknown,
}

impl TelegramChatType {
    /// Parse a `chat.type` string. Unknown values map to [`Self::Unknown`].
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        match raw.trim() {
            "private" => Self::Private,
            "group" => Self::Group,
            "supergroup" => Self::Supergroup,
            "channel" => Self::Channel,
            _ => Self::Unknown,
        }
    }

    /// The Bot API spelling, or `"unknown"`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Private => "private",
            Self::Group => "group",
            Self::Supergroup => "supergroup",
            Self::Channel => "channel",
            Self::Unknown => "unknown",
        }
    }

    /// Whether the chat has more than two participants that can talk to the
    /// bot (`group` or `supergroup`). Addressing only matters there.
    #[must_use]
    pub fn is_group(self) -> bool {
        matches!(self, Self::Group | Self::Supergroup)
    }
}

/// The receiving bot, used to decide whether a message addresses it.
///
/// Either field may be unknown. With neither, no group message counts as
/// addressed (the host does not know its own name, which is a fact, not a
/// reason to answer everything).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BotIdentity {
    username: Option<String>,
    user_id: Option<i64>,
}

impl BotIdentity {
    /// `username` may carry a leading `@`; it is compared case-insensitively.
    #[must_use]
    pub fn new(username: Option<&str>, user_id: Option<i64>) -> Self {
        let username = username
            .map(|name| name.trim().trim_start_matches('@').to_ascii_lowercase())
            .filter(|name| !name.is_empty());
        Self { username, user_id }
    }

    /// The lowercased username without `@`, when known.
    #[must_use]
    pub fn username(&self) -> Option<&str> {
        self.username.as_deref()
    }

    /// The bot's user id, when known.
    #[must_use]
    pub fn user_id(&self) -> Option<i64> {
        self.user_id
    }

    fn is_user(&self, user: &Value) -> bool {
        if let (Some(want), Some(got)) = (self.user_id, user.get("id").and_then(Value::as_i64)) {
            return want == got;
        }
        match (&self.username, user.get("username").and_then(Value::as_str)) {
            (Some(want), Some(got)) => got.eq_ignore_ascii_case(want),
            _ => false,
        }
    }
}

/// Normalized facts about one inbound Telegram message.
///
/// `thread_id` is set only for a real forum topic (see [`forum_thread_id`]).
/// `context_only` marks an unaddressed group message: the agent may read it,
/// but it is not a request, and terminal text for it must not be delivered
/// (see [`crate::speech_gate`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TelegramInboundMeta {
    /// `chat.id`.
    pub chat_id: i64,
    /// `chat.type`.
    pub chat_type: TelegramChatType,
    /// The forum topic's `message_thread_id`, always positive when present.
    pub thread_id: Option<i64>,
    /// `message_id`.
    pub message_id: i64,
    /// `from.id`, absent for anonymous admins and channel posts.
    pub sender_id: Option<i64>,
    /// `from.is_bot`.
    pub sender_is_bot: bool,
    /// See [`is_addressed_to_bot`].
    pub addressed: bool,
    /// See [`is_context_only`].
    pub context_only: bool,
}

impl TelegramInboundMeta {
    /// Parse a Bot API `Message`. Returns `None` when `chat.id` or
    /// `message_id` is missing, since such a message cannot be answered.
    #[must_use]
    pub fn from_message(message: &Value, bot: &BotIdentity) -> Option<Self> {
        let chat_id = message.pointer("/chat/id").and_then(Value::as_i64)?;
        let message_id = message.get("message_id").and_then(Value::as_i64)?;
        let chat_type = chat_type(message);
        let addressed = is_addressed_to_bot(message, bot);
        let from = message.get("from");
        Some(Self {
            chat_id,
            chat_type,
            thread_id: forum_thread_id(message),
            message_id,
            sender_id: from.and_then(|f| f.get("id")).and_then(Value::as_i64),
            sender_is_bot: from
                .and_then(|f| f.get("is_bot"))
                .and_then(Value::as_bool)
                .unwrap_or(false),
            addressed,
            context_only: chat_type.is_group() && !addressed,
        })
    }

    /// Reply metadata in the shape [`crate::reply_target::resolve_reply_target`]
    /// reads: `replyChannel`, `replyTarget`, `replyThreadId` (string, only for a
    /// topic), `replyToMessageId`, `chatType` and `contextOnly`.
    #[must_use]
    pub fn reply_metadata(&self) -> Value {
        let mut meta = json!({
            "replyChannel": crate::reply_target::TELEGRAM_CHANNEL,
            "replyTarget": self.chat_id.to_string(),
            "replyToMessageId": self.message_id.to_string(),
            "chatType": self.chat_type.as_str(),
            "contextOnly": self.context_only,
        });
        if let Some(thread) = self.thread_id {
            meta["replyThreadId"] = Value::String(thread.to_string());
        }
        meta
    }
}

/// `chat.type` of a message.
#[must_use]
pub fn chat_type(message: &Value) -> TelegramChatType {
    message
        .pointer("/chat/type")
        .and_then(Value::as_str)
        .map_or(TelegramChatType::Unknown, TelegramChatType::parse)
}

/// The forum topic a message belongs to.
///
/// Telegram sets `message_thread_id` both for forum topics and for reply
/// chains in ordinary supergroups (and in a forum's General topic). Only a
/// message with `is_topic_message: true` is in a topic, so only then is the
/// id returned, and only when it is positive. Messages in a forum's General
/// topic carry no topic and return `None`; replies there go to General
/// without a `message_thread_id`.
///
/// ```
/// use agent_channels::telegram::forum_thread_id;
/// use serde_json::json;
///
/// let topic = json!({"message_thread_id": 54007, "is_topic_message": true});
/// assert_eq!(forum_thread_id(&topic), Some(54007));
///
/// // Reply chain in a non-forum supergroup: not a topic.
/// let chain = json!({"message_thread_id": 12, "chat": {"type": "supergroup"}});
/// assert_eq!(forum_thread_id(&chain), None);
/// ```
#[must_use]
pub fn forum_thread_id(message: &Value) -> Option<i64> {
    if message.get("is_topic_message").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    message
        .get("message_thread_id")
        .and_then(Value::as_i64)
        .filter(|id| *id > 0)
}

/// Whether a message is addressed to `bot`.
///
/// A private chat always addresses the bot. In other chats, any of these
/// does:
///
/// - a `mention` entity equal to `@username`;
/// - a `text_mention` entity whose user is the bot;
/// - a `bot_command` entity `/cmd@username`, or a bare `/cmd` at offset 0;
/// - `@username` in the text on word boundaries (for clients that send no
///   entities);
/// - a reply to a message sent by the bot.
///
/// A command aimed at another bot (`/cmd@other_bot`) does not.
#[must_use]
pub fn is_addressed_to_bot(message: &Value, bot: &BotIdentity) -> bool {
    if chat_type(message) == TelegramChatType::Private {
        return true;
    }
    let (text, entities) = text_and_entities(message);
    if entities
        .iter()
        .any(|entity| entity_addresses_bot(entity, text, bot))
    {
        return true;
    }
    if let Some(username) = bot.username()
        && text_mentions_username(text, username)
    {
        return true;
    }
    is_reply_to_bot(message, bot)
}

/// Whether a message is context only: sent in a group or supergroup and not
/// addressed to `bot`.
#[must_use]
pub fn is_context_only(message: &Value, bot: &BotIdentity) -> bool {
    chat_type(message).is_group() && !is_addressed_to_bot(message, bot)
}

/// Whether the message replies to one the bot sent.
#[must_use]
pub fn is_reply_to_bot(message: &Value, bot: &BotIdentity) -> bool {
    let Some(from) = message.pointer("/reply_to_message/from") else {
        return false;
    };
    from.get("is_bot").and_then(Value::as_bool) == Some(true) && bot.is_user(from)
}

fn text_and_entities(message: &Value) -> (&str, &[Value]) {
    let (text, entities) = match message.get("text").and_then(Value::as_str) {
        Some(text) => (text, message.get("entities")),
        None => (
            message.get("caption").and_then(Value::as_str).unwrap_or(""),
            message.get("caption_entities"),
        ),
    };
    let entities = entities
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    (text, entities)
}

fn entity_addresses_bot(entity: &Value, text: &str, bot: &BotIdentity) -> bool {
    let kind = entity.get("type").and_then(Value::as_str).unwrap_or("");
    if kind == "text_mention" {
        return entity.get("user").is_some_and(|user| bot.is_user(user));
    }
    let offset = entity.get("offset").and_then(Value::as_u64);
    let length = entity.get("length").and_then(Value::as_u64);
    let (Some(offset), Some(length)) = (offset, length) else {
        return false;
    };
    let Some(slice) = utf16_slice(text, offset as usize, length as usize) else {
        return false;
    };
    let Some(username) = bot.username() else {
        // A bare command at the start still addresses whoever receives it.
        return kind == "bot_command" && offset == 0 && !slice.contains('@');
    };
    match kind {
        "mention" => slice
            .strip_prefix('@')
            .is_some_and(|name| name.eq_ignore_ascii_case(username)),
        "bot_command" => match slice.split_once('@') {
            None => offset == 0,
            Some((_, target)) => target.eq_ignore_ascii_case(username),
        },
        _ => false,
    }
}

/// Telegram entity offsets and lengths count UTF-16 code units.
fn utf16_slice(text: &str, offset: usize, length: usize) -> Option<String> {
    let units: Vec<u16> = text.encode_utf16().collect();
    let end = offset.checked_add(length)?;
    if length == 0 || end > units.len() {
        return None;
    }
    String::from_utf16(&units[offset..end]).ok()
}

fn text_mentions_username(text: &str, username: &str) -> bool {
    let lower = text.to_lowercase();
    let needle = format!("@{username}");
    let is_name_char = |c: char| c.is_ascii_alphanumeric() || c == '_';
    lower.match_indices(&needle).any(|(index, _)| {
        let before = lower[..index].chars().next_back();
        let after = lower[index + needle.len()..].chars().next();
        before.is_none_or(|c| c != '@' && !is_name_char(c))
            && after.is_none_or(|c| !is_name_char(c))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bot() -> BotIdentity {
        BotIdentity::new(Some("@Example_Bot"), Some(999))
    }

    fn group(text: &str) -> Value {
        json!({
            "message_id": 1,
            "chat": {"id": -100, "type": "supergroup"},
            "from": {"id": 5, "is_bot": false},
            "text": text,
        })
    }

    #[test]
    fn forum_topic_thread_is_kept() {
        let message = json!({
            "message_id": 3,
            "message_thread_id": 54007,
            "is_topic_message": true,
            "chat": {"id": -1001, "type": "supergroup", "is_forum": true},
        });
        assert_eq!(forum_thread_id(&message), Some(54007));
    }

    #[test]
    fn general_topic_has_no_thread() {
        let plain = json!({"chat": {"type": "supergroup", "is_forum": true}});
        assert_eq!(forum_thread_id(&plain), None);
        // A reply chain inside General carries a thread id but no topic flag.
        let reply = json!({
            "message_thread_id": 77,
            "chat": {"type": "supergroup", "is_forum": true},
            "reply_to_message": {"message_id": 77},
        });
        assert_eq!(forum_thread_id(&reply), None);
    }

    #[test]
    fn non_forum_reply_chain_thread_is_rejected() {
        let message = json!({
            "message_thread_id": 12,
            "chat": {"type": "supergroup"},
            "reply_to_message": {"message_id": 12},
        });
        assert_eq!(forum_thread_id(&message), None);
        let flagged_false = json!({"message_thread_id": 12, "is_topic_message": false});
        assert_eq!(forum_thread_id(&flagged_false), None);
    }

    #[test]
    fn zero_or_negative_thread_is_rejected() {
        for id in [0, -5] {
            let message = json!({"message_thread_id": id, "is_topic_message": true});
            assert_eq!(forum_thread_id(&message), None);
        }
        let stringy = json!({"message_thread_id": "54007", "is_topic_message": true});
        assert_eq!(forum_thread_id(&stringy), None, "Bot API sends integers");
    }

    #[test]
    fn private_chat_is_always_addressed() {
        let message = json!({"message_id": 1, "chat": {"id": 5, "type": "private"}, "text": "hi"});
        assert!(is_addressed_to_bot(&message, &BotIdentity::default()));
        assert!(!is_context_only(&message, &BotIdentity::default()));
    }

    #[test]
    fn mention_entity_addresses_bot_case_insensitively() {
        let mut message = group("hey @example_BOT look");
        message["entities"] = json!([{"type": "mention", "offset": 4, "length": 12}]);
        assert!(is_addressed_to_bot(&message, &bot()));
    }

    #[test]
    fn mention_offsets_are_utf16() {
        // The emoji is two UTF-16 units.
        let mut message = group("😀 @example_bot");
        message["entities"] = json!([{"type": "mention", "offset": 3, "length": 12}]);
        let no_text_fallback = BotIdentity::new(Some("example_bot"), None);
        assert!(entity_addresses_bot(
            &message["entities"][0],
            message["text"].as_str().unwrap(),
            &no_text_fallback
        ));
    }

    #[test]
    fn text_mention_by_user_id() {
        let mut message = group("Bot, please");
        message["entities"] =
            json!([{"type": "text_mention", "offset": 0, "length": 3, "user": {"id": 999}}]);
        assert!(is_addressed_to_bot(&message, &bot()));
    }

    #[test]
    fn commands_for_this_bot_or_bare_address_it() {
        let mut bare = group("/status now");
        bare["entities"] = json!([{"type": "bot_command", "offset": 0, "length": 7}]);
        assert!(is_addressed_to_bot(&bare, &bot()));

        let mut ours = group("/status@example_bot");
        ours["entities"] = json!([{"type": "bot_command", "offset": 0, "length": 19}]);
        assert!(is_addressed_to_bot(&ours, &bot()));

        let mut theirs = group("/status@other_bot");
        theirs["entities"] = json!([{"type": "bot_command", "offset": 0, "length": 17}]);
        assert!(!is_addressed_to_bot(&theirs, &bot()));
    }

    #[test]
    fn plain_text_mention_respects_word_boundaries() {
        assert!(is_addressed_to_bot(&group("ping @example_bot!"), &bot()));
        assert!(!is_addressed_to_bot(&group("ping @example_bot2"), &bot()));
        assert!(!is_addressed_to_bot(&group("mail x@example_bot"), &bot()));
    }

    #[test]
    fn reply_to_bot_addresses_it_but_reply_to_other_bot_does_not() {
        let mut message = group("thanks");
        message["reply_to_message"] = json!({"message_id": 2, "from": {"id": 999, "is_bot": true, "username": "example_bot"}});
        assert!(is_addressed_to_bot(&message, &bot()));

        message["reply_to_message"]["from"] = json!({"id": 1000, "is_bot": true});
        assert!(!is_addressed_to_bot(&message, &bot()));

        // A human sharing the bot's username cannot spoof the reply check by id.
        message["reply_to_message"]["from"] =
            json!({"id": 1000, "is_bot": false, "username": "example_bot"});
        assert!(!is_addressed_to_bot(&message, &bot()));
    }

    #[test]
    fn caption_entities_are_read_for_media() {
        let message = json!({
            "message_id": 1,
            "chat": {"id": -100, "type": "group"},
            "caption": "@example_bot see this",
            "caption_entities": [{"type": "mention", "offset": 0, "length": 12}],
        });
        assert!(is_addressed_to_bot(
            &message,
            &BotIdentity::new(Some("example_bot"), None)
        ));
    }

    #[test]
    fn unaddressed_group_message_is_context_only() {
        let meta = TelegramInboundMeta::from_message(&group("just chatting"), &bot()).unwrap();
        assert!(!meta.addressed);
        assert!(meta.context_only);
        assert_eq!(meta.chat_type, TelegramChatType::Supergroup);
    }

    #[test]
    fn unknown_bot_identity_never_addresses_group_text() {
        let meta =
            TelegramInboundMeta::from_message(&group("@example_bot hi"), &BotIdentity::default())
                .unwrap();
        assert!(meta.context_only);
    }

    #[test]
    fn inbound_meta_envelope_and_reply_metadata() {
        let message = json!({
            "message_id": 42,
            "message_thread_id": 54007,
            "is_topic_message": true,
            "chat": {"id": -1001, "type": "supergroup", "is_forum": true},
            "from": {"id": 5, "is_bot": true},
            "text": "@example_bot status",
        });
        let meta = TelegramInboundMeta::from_message(&message, &bot()).unwrap();
        assert_eq!(
            meta,
            TelegramInboundMeta {
                chat_id: -1001,
                chat_type: TelegramChatType::Supergroup,
                thread_id: Some(54007),
                message_id: 42,
                sender_id: Some(5),
                sender_is_bot: true,
                addressed: true,
                context_only: false,
            }
        );
        let reply = meta.reply_metadata();
        assert_eq!(reply["replyThreadId"], "54007");
        assert_eq!(reply["replyTarget"], "-1001");
        assert_eq!(reply["contextOnly"], false);

        let round_trip: TelegramInboundMeta =
            serde_json::from_value(serde_json::to_value(&meta).unwrap()).unwrap();
        assert_eq!(round_trip, meta);
    }

    #[test]
    fn message_without_chat_or_id_is_rejected() {
        assert!(TelegramInboundMeta::from_message(&json!({"message_id": 1}), &bot()).is_none());
        assert!(TelegramInboundMeta::from_message(&json!({"chat": {"id": 1}}), &bot()).is_none());
    }
}
