//! Canonical reply routing with forum-topic isolation.
//!
//! A [`ReplyTarget`] is channel + target + optional thread. It can only be
//! built through validating constructors, so a Telegram target never carries
//! a zero, negative, or non-numeric thread, nor a compound chat id.
//!
//! [`resolve_reply_target`] reads every spelling of the thread hosts have
//! used (`replyThreadId`, `messageThreadId`, snake-case forms; JSON strings
//! or positive integers), then a thread embedded in the target, then the
//! session key (`...:<target>:topic:<id>` or `...:<target>:thread:<id>`).
//! A placeholder in one field (`"null"`, `""`, `0`) never hides a valid id
//! in the next.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The channel name for Telegram.
pub const TELEGRAM_CHANNEL: &str = "telegram";

/// Metadata keys read for the thread id, in priority order.
pub const THREAD_ID_KEYS: &[&str] = &[
    "replyThreadId",
    "messageThreadId",
    "reply_thread_id",
    "message_thread_id",
];

const CHANNEL_KEYS: &[&str] = &["replyChannel", "channel"];
const TARGET_KEYS: &[&str] = &["replyTarget", "target", "chatId", "chat_id"];
const THREAD_MARKERS: &[&str] = &[":topic:", ":thread:"];
const TELEGRAM_CHAT_ADDRESS_PREFIX: &str = "telegram:chat:";

/// Where a reply goes: a channel, a target on it, and an optional thread.
///
/// Fields are private; construct with [`ReplyTarget::new`],
/// [`ReplyTarget::telegram`], [`resolve_reply_target`], or deserialization,
/// all of which validate. Serialized as `{"channel", "target", "threadId"}`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "ReplyTargetWire", into = "ReplyTargetWire")]
pub struct ReplyTarget {
    channel: String,
    target: String,
    thread_id: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReplyTargetWire {
    channel: String,
    target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    thread_id: Option<Value>,
}

impl TryFrom<ReplyTargetWire> for ReplyTarget {
    type Error = ReplyTargetError;

    fn try_from(wire: ReplyTargetWire) -> Result<Self, Self::Error> {
        let thread = match wire.thread_id {
            None | Some(Value::Null) => None,
            Some(value) => Some(
                normalize_thread_id(&wire.channel, &value)
                    .ok_or(ReplyTargetError::InvalidThread)?,
            ),
        };
        Self::new(&wire.channel, &wire.target, thread.as_deref())
    }
}

impl From<ReplyTarget> for ReplyTargetWire {
    fn from(target: ReplyTarget) -> Self {
        Self {
            channel: target.channel,
            target: target.target,
            thread_id: target.thread_id.map(Value::String),
        }
    }
}

/// Why a reply target or payload could not be built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplyTargetError {
    /// The channel name is empty.
    MissingChannel,
    /// The target is empty, or is not a valid chat on its channel (for
    /// Telegram: an integer chat id or an `@username`).
    InvalidTarget,
    /// The thread is not valid on the channel (for Telegram: not a positive
    /// integer).
    InvalidThread,
    /// A Telegram payload was requested for another channel.
    NotTelegram,
}

impl fmt::Display for ReplyTargetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::MissingChannel => "reply target has no channel",
            Self::InvalidTarget => "reply target is not a valid chat on its channel",
            Self::InvalidThread => "reply thread is not valid on its channel",
            Self::NotTelegram => "reply target is not a Telegram chat",
        })
    }
}

impl std::error::Error for ReplyTargetError {}

impl ReplyTarget {
    /// Validate and build a target. `thread` is trimmed and empty or
    /// placeholder values (`null`, `undefined`, `none`, `nil`) mean no thread.
    /// For Telegram the thread must be a positive integer and is stored in
    /// canonical form (`"054007"` becomes `"54007"`).
    ///
    /// # Errors
    ///
    /// See [`ReplyTargetError`].
    pub fn new(
        channel: &str,
        target: &str,
        thread: Option<&str>,
    ) -> Result<Self, ReplyTargetError> {
        let channel = channel.trim();
        if channel.is_empty() {
            return Err(ReplyTargetError::MissingChannel);
        }
        let target = target.trim();
        if target.is_empty() || (is_telegram(channel) && !is_telegram_chat(target)) {
            return Err(ReplyTargetError::InvalidTarget);
        }
        let thread_id = match thread.and_then(non_placeholder) {
            None => None,
            Some(raw) => {
                Some(normalize_thread_text(channel, raw).ok_or(ReplyTargetError::InvalidThread)?)
            }
        };
        Ok(Self {
            channel: if is_telegram(channel) {
                TELEGRAM_CHANNEL.to_string()
            } else {
                channel.to_string()
            },
            target: target.to_string(),
            thread_id,
        })
    }

    /// A Telegram chat, optionally in a forum topic. A non-positive `thread`
    /// is dropped.
    #[must_use]
    pub fn telegram(chat_id: i64, thread: Option<i64>) -> Self {
        Self {
            channel: TELEGRAM_CHANNEL.to_string(),
            target: chat_id.to_string(),
            thread_id: thread.filter(|t| *t > 0).map(|t| t.to_string()),
        }
    }

    /// The channel name.
    #[must_use]
    pub fn channel(&self) -> &str {
        &self.channel
    }

    /// The chat or address on the channel, never compound.
    #[must_use]
    pub fn target(&self) -> &str {
        &self.target
    }

    /// The thread, in canonical form for the channel.
    #[must_use]
    pub fn thread_id(&self) -> Option<&str> {
        self.thread_id.as_deref()
    }

    /// Whether this is a Telegram target.
    #[must_use]
    pub fn is_telegram(&self) -> bool {
        self.channel == TELEGRAM_CHANNEL
    }

    /// The Telegram forum topic, when this is a Telegram target in one.
    #[must_use]
    pub fn telegram_thread_id(&self) -> Option<i64> {
        if !self.is_telegram() {
            return None;
        }
        self.thread_id.as_deref()?.parse().ok()
    }

    /// A Bot API `sendMessage` body for this target. See
    /// [`telegram_send_message_payload`].
    ///
    /// # Errors
    ///
    /// [`ReplyTargetError::NotTelegram`] for other channels.
    pub fn telegram_send_message_payload(
        &self,
        text: &str,
        reply_to_message_id: Option<i64>,
    ) -> Result<Value, ReplyTargetError> {
        if !self.is_telegram() {
            return Err(ReplyTargetError::NotTelegram);
        }
        let mut body = json!({ "chat_id": telegram_chat_id_value(&self.target), "text": text });
        if let Some(id) = reply_to_message_id.filter(|id| *id > 0) {
            body["reply_parameters"] = json!({ "message_id": id });
        }
        apply_telegram_message_thread_id(&mut body, self.telegram_thread_id());
        Ok(body)
    }
}

/// Normalize one thread value for `channel`.
///
/// Accepts a positive JSON integer or a non-placeholder string. For
/// Telegram the result must be a positive integer (a trailing `#...`
/// continuation suffix is ignored); anything else is `None`.
///
/// ```
/// use agent_channels::reply_target::normalize_thread_id;
/// use serde_json::json;
///
/// assert_eq!(normalize_thread_id("telegram", &json!(54007)).as_deref(), Some("54007"));
/// assert_eq!(normalize_thread_id("telegram", &json!(" 54007 ")).as_deref(), Some("54007"));
/// assert_eq!(normalize_thread_id("telegram", &json!(0)), None);
/// assert_eq!(normalize_thread_id("telegram", &json!("null")), None);
/// ```
#[must_use]
pub fn normalize_thread_id(channel: &str, value: &Value) -> Option<String> {
    match value {
        Value::Number(n) => {
            let id = n
                .as_i64()
                .or_else(|| n.as_u64().and_then(|u| i64::try_from(u).ok()))?;
            (id > 0).then(|| id.to_string())
        }
        Value::String(text) => {
            non_placeholder(text).and_then(|raw| normalize_thread_text(channel, raw))
        }
        _ => None,
    }
}

/// The first valid thread id in `metadata` under [`THREAD_ID_KEYS`].
#[must_use]
pub fn resolve_thread_id(channel: &str, metadata: &Value) -> Option<String> {
    THREAD_ID_KEYS
        .iter()
        .filter_map(|key| metadata.get(*key))
        .find_map(|value| normalize_thread_id(channel, value))
}

/// Resolve the reply route for an inbound item.
///
/// Reads `replyChannel` (or `channel`) and `replyTarget` (or `target`,
/// `chatId`, `chat_id`; strings or integers) from `metadata`. The target may
/// be compound (`-1001:topic:54007`, `telegram:chat:-1001:54007`); it is
/// decomposed. The thread is the first valid of: [`resolve_thread_id`], the
/// thread in the target, the thread in `session_key` for the same target.
///
/// With no channel and target in metadata, the route is derived from a
/// `session_key` of the form `<channel>:...:<target>:topic:<id>` (or
/// `:thread:`, or `telegram:chat:<chat>:<thread>`); keys without a thread
/// marker do not name a route and give `None`.
///
/// Returns `None` when no valid route exists, including a Telegram target
/// that is not a bare chat after decomposition: an unresolvable route is
/// never guessed.
///
/// ```
/// use agent_channels::reply_target::resolve_reply_target;
/// use serde_json::json;
///
/// let meta = json!({"replyChannel": "telegram", "replyTarget": "-1001", "messageThreadId": 54007});
/// let target = resolve_reply_target(Some(&meta), None).unwrap();
/// assert_eq!(target.telegram_thread_id(), Some(54007));
///
/// let meta = json!({"replyChannel": "telegram", "replyTarget": "-1001"});
/// let target = resolve_reply_target(Some(&meta), Some("telegram:example:-1001:topic:54007")).unwrap();
/// assert_eq!(target.telegram_thread_id(), Some(54007));
/// ```
#[must_use]
pub fn resolve_reply_target(
    metadata: Option<&Value>,
    session_key: Option<&str>,
) -> Option<ReplyTarget> {
    let session = session_key.and_then(parse_address);
    let Some(meta) = metadata.filter(|m| m.is_object()) else {
        return session.and_then(Address::into_reply_target);
    };
    let meta_channel = CHANNEL_KEYS.iter().find_map(|key| {
        meta.get(*key)
            .and_then(Value::as_str)
            .and_then(non_placeholder)
    });
    let raw_target = TARGET_KEYS.iter().find_map(|key| match meta.get(*key)? {
        Value::String(s) => non_placeholder(s).map(str::to_string),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    });
    let Some(raw_target) = raw_target else {
        // Metadata without a target cannot be combined with anything safely,
        // unless it names no channel either (then only the key speaks).
        return match meta_channel {
            None => session.and_then(Address::into_reply_target),
            Some(_) => None,
        };
    };
    let address = parse_address(&raw_target).unwrap_or(Address {
        channel: None,
        target: raw_target.clone(),
        thread: None,
    });
    let channel = match (meta_channel, address.channel.as_deref()) {
        (Some(meta), Some(addr)) if !meta.eq_ignore_ascii_case(addr) => return None,
        (Some(meta), _) => meta.to_string(),
        (None, Some(addr)) => addr.to_string(),
        (None, None) => return None,
    };
    let thread = resolve_thread_id(&channel, meta)
        .or_else(|| {
            address
                .thread
                .as_deref()
                .and_then(|t| normalize_thread_text(&channel, t))
        })
        .or_else(|| {
            session
                .filter(|s| s.same_route(&channel, &address.target))
                .and_then(|s| s.thread)
                .and_then(|t| normalize_thread_text(&channel, &t))
        });
    ReplyTarget::new(&channel, &address.target, thread.as_deref()).ok()
}

/// A Bot API `sendMessage` body.
///
/// `message_thread_id` is attached exactly when `thread_id` is positive; a
/// zero or negative id is never sent. `reply_to_message_id` becomes
/// `reply_parameters.message_id`.
///
/// ```
/// use agent_channels::reply_target::telegram_send_message_payload;
///
/// let body = telegram_send_message_payload(-1001, "hi", Some(54007), None);
/// assert_eq!(body["message_thread_id"], 54007);
/// let general = telegram_send_message_payload(-1001, "hi", Some(0), None);
/// assert!(general.get("message_thread_id").is_none());
/// ```
#[must_use]
pub fn telegram_send_message_payload(
    chat_id: i64,
    text: &str,
    thread_id: Option<i64>,
    reply_to_message_id: Option<i64>,
) -> Value {
    ReplyTarget::telegram(chat_id, thread_id)
        .telegram_send_message_payload(text, reply_to_message_id)
        .expect("telegram target")
}

/// Set `message_thread_id` on any Bot API send body (`sendPhoto`,
/// `sendDocument`, ...) when `thread_id` is positive, and remove any
/// existing non-positive or non-integer value either way.
pub fn apply_telegram_message_thread_id(body: &mut Value, thread_id: Option<i64>) {
    let Some(object) = body.as_object_mut() else {
        return;
    };
    match thread_id.filter(|t| *t > 0) {
        Some(id) => {
            object.insert("message_thread_id".into(), json!(id));
        }
        None => {
            let keep = object
                .get("message_thread_id")
                .and_then(Value::as_i64)
                .is_some_and(|t| t > 0);
            if !keep {
                object.remove("message_thread_id");
            }
        }
    }
}

/// Telegram chat ids are sent as numbers; `@username` targets as strings.
fn telegram_chat_id_value(target: &str) -> Value {
    target
        .parse::<i64>()
        .map_or_else(|_| Value::String(target.to_string()), |id| json!(id))
}

fn is_telegram(channel: &str) -> bool {
    channel.eq_ignore_ascii_case(TELEGRAM_CHANNEL)
}

fn is_telegram_chat(target: &str) -> bool {
    if target.parse::<i64>().is_ok_and(|id| id != 0) {
        return true;
    }
    target.strip_prefix('@').is_some_and(|name| {
        !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

fn non_placeholder(raw: &str) -> Option<&str> {
    let trimmed = raw.trim();
    let placeholder = trimmed.is_empty()
        || ["null", "undefined", "none", "nil"]
            .iter()
            .any(|p| trimmed.eq_ignore_ascii_case(p));
    (!placeholder).then_some(trimmed)
}

fn normalize_thread_text(channel: &str, raw: &str) -> Option<String> {
    let raw = non_placeholder(raw)?;
    if !is_telegram(channel) {
        return Some(raw.to_string());
    }
    let raw = strip_continuation(raw).trim();
    if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    raw.parse::<i64>()
        .ok()
        .filter(|id| *id > 0)
        .map(|id| id.to_string())
}

fn strip_continuation(raw: &str) -> &str {
    raw.split_once('#').map_or(raw, |(head, _)| head)
}

/// A route parsed from a compound target or session key.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Address {
    channel: Option<String>,
    target: String,
    thread: Option<String>,
}

impl Address {
    fn into_reply_target(self) -> Option<ReplyTarget> {
        // A session key only names a route when it names a thread.
        let thread = self.thread?;
        ReplyTarget::new(self.channel.as_deref()?, &self.target, Some(&thread)).ok()
    }

    fn same_route(&self, channel: &str, target: &str) -> bool {
        self.target == target
            && self
                .channel
                .as_deref()
                .is_none_or(|c| c.eq_ignore_ascii_case(channel))
    }
}

/// Parse `telegram:chat:<chat>[:<thread>]`, `...:<target>:topic:<id>`,
/// `...:<target>:thread:<id>`, `telegram:<chat>`, or a bare target.
fn parse_address(raw: &str) -> Option<Address> {
    let raw = strip_continuation(raw.trim()).trim();
    if raw.is_empty() {
        return None;
    }
    if let Some(rest) = raw.strip_prefix(TELEGRAM_CHAT_ADDRESS_PREFIX) {
        let (chat, thread) = match rest.split_once(':') {
            Some((chat, thread)) => (chat, Some(thread.to_string())),
            None => (rest, None),
        };
        return Some(Address {
            channel: Some(TELEGRAM_CHANNEL.into()),
            target: chat.to_string(),
            thread,
        });
    }
    let marker = THREAD_MARKERS
        .iter()
        .filter_map(|marker| raw.rfind(marker).map(|at| (at, marker.len())))
        .max_by_key(|(at, _)| *at);
    if let Some((at, len)) = marker {
        let prefix = &raw[..at];
        let thread = raw[at + len..].split(':').next().unwrap_or("");
        let (channel, target) = match prefix.split_once(':') {
            Some((channel, _)) => (
                Some(channel.to_string()),
                prefix.rsplit(':').next().unwrap_or(""),
            ),
            None => (None, prefix),
        };
        return Some(Address {
            channel,
            target: target.to_string(),
            thread: (!thread.is_empty()).then(|| thread.to_string()),
        });
    }
    if let Some(chat) = raw.strip_prefix("telegram:")
        && !chat.contains(':')
    {
        return Some(Address {
            channel: Some(TELEGRAM_CHANNEL.into()),
            target: chat.to_string(),
            thread: None,
        });
    }
    Some(Address {
        channel: None,
        target: raw.to_string(),
        thread: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(extra: Value) -> Value {
        let mut base = json!({"replyChannel": "telegram", "replyTarget": "-1001"});
        for (k, v) in extra.as_object().unwrap() {
            base[k] = v.clone();
        }
        base
    }

    fn thread_of(meta: &Value, key: Option<&str>) -> Option<i64> {
        resolve_reply_target(Some(meta), key)?.telegram_thread_id()
    }

    #[test]
    fn integer_message_thread_id_is_kept() {
        assert_eq!(
            thread_of(&meta(json!({"messageThreadId": 54007})), None),
            Some(54007)
        );
    }

    #[test]
    fn string_thread_ids_under_every_key_are_kept() {
        for key in THREAD_ID_KEYS {
            assert_eq!(
                thread_of(&meta(json!({ *key: "54007" })), None),
                Some(54007),
                "{key}"
            );
            assert_eq!(
                thread_of(&meta(json!({ *key: 54007 })), None),
                Some(54007),
                "{key}"
            );
        }
    }

    #[test]
    fn placeholder_reply_thread_id_does_not_hide_message_thread_id() {
        for placeholder in [
            json!("null"),
            json!(""),
            json!(0),
            json!(-3),
            json!("abc"),
            json!(null),
        ] {
            let m = meta(json!({"replyThreadId": placeholder, "messageThreadId": 54007}));
            assert_eq!(thread_of(&m, None), Some(54007), "{placeholder}");
        }
    }

    #[test]
    fn reply_thread_id_wins_over_message_thread_id() {
        let m = meta(json!({"replyThreadId": "7", "messageThreadId": 54007}));
        assert_eq!(thread_of(&m, None), Some(7));
    }

    #[test]
    fn topic_session_key_fallback() {
        let m = meta(json!({}));
        assert_eq!(
            thread_of(&m, Some("telegram:example:-1001:topic:54007")),
            Some(54007)
        );
        assert_eq!(
            thread_of(&m, Some("telegram:example:-1001:thread:54007")),
            Some(54007)
        );
        assert_eq!(
            thread_of(&m, Some("telegram:example:-1001:topic:54007#chain-1")),
            Some(54007)
        );
        assert_eq!(
            thread_of(&m, Some("telegram:chat:-1001:54007")),
            Some(54007)
        );
    }

    #[test]
    fn session_key_for_another_chat_or_channel_is_ignored() {
        let m = meta(json!({}));
        assert_eq!(
            thread_of(&m, Some("telegram:example:-2002:topic:54007")),
            None
        );
        assert_eq!(
            thread_of(&m, Some("slack:example:-1001:thread:54007")),
            None
        );
    }

    #[test]
    fn metadata_thread_wins_over_session_key() {
        let m = meta(json!({"messageThreadId": 9}));
        assert_eq!(
            thread_of(&m, Some("telegram:example:-1001:topic:54007")),
            Some(9)
        );
    }

    #[test]
    fn general_topic_has_no_thread() {
        let target =
            resolve_reply_target(Some(&meta(json!({}))), Some("telegram:example:-1001")).unwrap();
        assert_eq!(target, ReplyTarget::telegram(-1001, None));
        let body = target.telegram_send_message_payload("hi", None).unwrap();
        assert!(body.get("message_thread_id").is_none());
    }

    #[test]
    fn compound_targets_are_decomposed() {
        for raw in [
            "-1001:topic:54007",
            "telegram:chat:-1001:54007",
            "-1001:topic:54007#c",
        ] {
            let m = json!({"replyChannel": "telegram", "replyTarget": raw});
            let target = resolve_reply_target(Some(&m), None).unwrap();
            assert_eq!(target, ReplyTarget::telegram(-1001, Some(54007)), "{raw}");
        }
        let no_channel = json!({"replyTarget": "telegram:chat:-1001:54007"});
        assert_eq!(
            resolve_reply_target(Some(&no_channel), None),
            Some(ReplyTarget::telegram(-1001, Some(54007)))
        );
    }

    #[test]
    fn integer_chat_id_target_is_accepted() {
        let m = json!({"replyChannel": "telegram", "chatId": -1001, "message_thread_id": 54007});
        assert_eq!(
            resolve_reply_target(Some(&m), None),
            Some(ReplyTarget::telegram(-1001, Some(54007)))
        );
    }

    #[test]
    fn unresolvable_telegram_target_fails_closed() {
        for raw in ["binding:abc:-1001:x", "not-a-chat", "0"] {
            let m = json!({"replyChannel": "telegram", "replyTarget": raw});
            assert_eq!(resolve_reply_target(Some(&m), None), None, "{raw}");
        }
        let conflict = json!({"replyChannel": "slack", "replyTarget": "telegram:chat:-1001"});
        assert_eq!(resolve_reply_target(Some(&conflict), None), None);
    }

    #[test]
    fn session_key_alone_derives_route_only_with_thread() {
        assert_eq!(
            resolve_reply_target(None, Some("telegram:example:-1001:topic:54007")),
            Some(ReplyTarget::telegram(-1001, Some(54007)))
        );
        assert_eq!(
            resolve_reply_target(None, Some("telegram:example:-1001")),
            None
        );
        assert_eq!(
            resolve_reply_target(None, Some("telegram:example:-1001:topic:0")),
            None
        );
    }

    #[test]
    fn non_telegram_threads_are_opaque_strings() {
        let m = json!({"replyChannel": "slack", "replyTarget": "C123", "replyThreadId": "1700.01"});
        let target = resolve_reply_target(Some(&m), None).unwrap();
        assert_eq!(target.thread_id(), Some("1700.01"));
        assert_eq!(target.telegram_thread_id(), None);
        assert_eq!(
            target.telegram_send_message_payload("x", None),
            Err(ReplyTargetError::NotTelegram)
        );
    }

    #[test]
    fn constructor_rejects_invalid_telegram_threads() {
        assert_eq!(
            ReplyTarget::new("telegram", "-1001", Some("0")),
            Err(ReplyTargetError::InvalidThread)
        );
        assert_eq!(
            ReplyTarget::new("telegram", "-1001", Some("-5")),
            Err(ReplyTargetError::InvalidThread)
        );
        assert_eq!(
            ReplyTarget::new("telegram", "-1001:topic:5", None),
            Err(ReplyTargetError::InvalidTarget)
        );
        assert_eq!(
            ReplyTarget::new("Telegram", "-1001", Some("054007")).unwrap(),
            ReplyTarget::telegram(-1001, Some(54007))
        );
        assert_eq!(ReplyTarget::telegram(-1001, Some(0)).thread_id(), None);
        assert!(ReplyTarget::new("telegram", "@example_channel", None).is_ok());
    }

    #[test]
    fn serde_round_trip_validates() {
        let target = ReplyTarget::telegram(-1001, Some(54007));
        let wire = serde_json::to_value(&target).unwrap();
        assert_eq!(
            wire,
            json!({"channel": "telegram", "target": "-1001", "threadId": "54007"})
        );
        assert_eq!(serde_json::from_value::<ReplyTarget>(wire).unwrap(), target);
        let int_thread = json!({"channel": "telegram", "target": "-1001", "threadId": 54007});
        assert_eq!(
            serde_json::from_value::<ReplyTarget>(int_thread).unwrap(),
            target
        );
        let zero = json!({"channel": "telegram", "target": "-1001", "threadId": 0});
        assert!(serde_json::from_value::<ReplyTarget>(zero).is_err());
    }

    #[test]
    fn send_message_payload_attaches_positive_thread_only() {
        let body = telegram_send_message_payload(-1001, "hi", Some(54007), Some(42));
        assert_eq!(
            body,
            json!({
                "chat_id": -1001,
                "text": "hi",
                "message_thread_id": 54007,
                "reply_parameters": {"message_id": 42},
            })
        );
        for bad in [Some(0), Some(-1), None] {
            let body = telegram_send_message_payload(-1001, "hi", bad, None);
            assert!(body.get("message_thread_id").is_none(), "{bad:?}");
        }
        let channel = ReplyTarget::new("telegram", "@example_channel", None).unwrap();
        let body = channel.telegram_send_message_payload("hi", None).unwrap();
        assert_eq!(body["chat_id"], "@example_channel");
    }

    #[test]
    fn apply_thread_scrubs_invalid_existing_values() {
        let mut body = json!({"chat_id": 1, "message_thread_id": 0});
        apply_telegram_message_thread_id(&mut body, None);
        assert!(body.get("message_thread_id").is_none());

        let mut body = json!({"chat_id": 1, "message_thread_id": "5"});
        apply_telegram_message_thread_id(&mut body, Some(-1));
        assert!(body.get("message_thread_id").is_none());

        let mut body = json!({"chat_id": 1});
        apply_telegram_message_thread_id(&mut body, Some(54007));
        assert_eq!(body["message_thread_id"], 54007);
    }
}
