//! Telegram Bot API request construction and response parsing. The caller owns transport.

use crate::reply_target::{apply_telegram_message_thread_id, telegram_send_message_payload};

use super::formatting::{markdown_to_telegram_html, rich_markdown};

/// Default Telegram Bot API origin. Hosts may configure their own.
pub const API: &str = "https://api.telegram.org";

/// One Bot API call. The body is the JSON object; the token stays with the caller.
#[derive(Clone, Debug, PartialEq)]
pub struct BotCall {
    pub method: String,
    pub body: serde_json::Value,
}

/// `sendRichMessage` (Telegram Bot API 10.1+) for one chat using native
/// Rich Markdown (`rich_message.markdown`).
/// `reply_to` is a Telegram message id, when the agent was answering one.
/// `thread` keeps the post in its forum topic.
pub fn send_message(
    chat_id: i64,
    text: &str,
    reply_to: Option<i64>,
    thread: Option<i64>,
) -> BotCall {
    let markdown = rich_markdown(text);
    let mut body = serde_json::json!({
        "chat_id": chat_id,
        "rich_message": {
            "markdown": markdown,
        },
    });
    if let Some(id) = reply_to {
        body["reply_parameters"] = serde_json::json!({ "message_id": id });
        body["reply_to_message_id"] = serde_json::json!(id);
    }
    apply_telegram_message_thread_id(&mut body, thread);
    BotCall {
        method: "sendRichMessage".to_string(),
        body,
    }
}

/// Plain-text `sendMessage` fallback without `parse_mode`.
pub fn send_plain_message(
    chat_id: i64,
    text: &str,
    reply_to: Option<i64>,
    thread: Option<i64>,
) -> BotCall {
    BotCall {
        method: "sendMessage".to_string(),
        body: telegram_send_message_payload(chat_id, text, thread, reply_to),
    }
}

/// `sendLocation` or `sendVenue` for GPS coordinates.
#[allow(clippy::too_many_arguments)]
pub fn send_location(
    chat_id: i64,
    latitude: f64,
    longitude: f64,
    title: Option<&str>,
    address: Option<&str>,
    reply_to: Option<i64>,
    thread: Option<i64>,
) -> BotCall {
    let is_venue = title.is_some_and(|t| !t.trim().is_empty())
        && address.is_some_and(|a| !a.trim().is_empty());
    let mut body = serde_json::json!({
        "chat_id": chat_id,
        "latitude": latitude,
        "longitude": longitude,
    });
    if is_venue {
        body["title"] = serde_json::json!(title.unwrap_or_default().trim());
        body["address"] = serde_json::json!(address.unwrap_or_default().trim());
    }
    if let Some(id) = reply_to {
        body["reply_to_message_id"] = serde_json::json!(id);
    }
    apply_telegram_message_thread_id(&mut body, thread);
    BotCall {
        method: if is_venue {
            "sendVenue".to_string()
        } else {
            "sendLocation".to_string()
        },
        body,
    }
}

/// `sendPoll` for creating interactive Telegram polls or quizzes.
#[allow(clippy::too_many_arguments)]
pub fn send_poll(
    chat_id: i64,
    question: &str,
    options: &[String],
    is_anonymous: Option<bool>,
    allows_multiple_answers: Option<bool>,
    reply_to: Option<i64>,
    thread: Option<i64>,
) -> BotCall {
    let mut body = serde_json::json!({
        "chat_id": chat_id,
        "question": question,
        "options": options.iter().map(|opt| serde_json::json!({ "text": opt })).collect::<Vec<_>>(),
    });
    if let Some(anon) = is_anonymous {
        body["is_anonymous"] = serde_json::json!(anon);
    }
    if let Some(multi) = allows_multiple_answers {
        body["allows_multiple_answers"] = serde_json::json!(multi);
    }
    if let Some(id) = reply_to {
        body["reply_to_message_id"] = serde_json::json!(id);
    }
    apply_telegram_message_thread_id(&mut body, thread);
    BotCall {
        method: "sendPoll".to_string(),
        body,
    }
}

/// `setMessageReaction` to add, replace, or clear an emoji reaction on a message.
pub fn set_message_reaction(
    chat_id: i64,
    message_id: i64,
    emoji: Option<&str>,
    is_big: Option<bool>,
) -> BotCall {
    let reaction = match emoji.map(str::trim).filter(|e| !e.is_empty()) {
        Some(e) => serde_json::json!([{ "type": "emoji", "emoji": e }]),
        None => serde_json::json!([]),
    };
    let mut body = serde_json::json!({
        "chat_id": chat_id,
        "message_id": message_id,
        "reaction": reaction,
    });
    if let Some(big) = is_big {
        body["is_big"] = serde_json::json!(big);
    }
    BotCall {
        method: "setMessageReaction".to_string(),
        body,
    }
}

/// `editMessageText` to update the rich Markdown content of an existing message.
pub fn edit_message_text(chat_id: i64, message_id: i64, text: &str) -> BotCall {
    let markdown = rich_markdown(text);
    BotCall {
        method: "editMessageText".to_string(),
        body: serde_json::json!({
            "chat_id": chat_id,
            "message_id": message_id,
            "rich_message": {
                "markdown": markdown,
            },
        }),
    }
}

/// `deleteMessage` to remove a message in a chat.
pub fn delete_message(chat_id: i64, message_id: i64) -> BotCall {
    BotCall {
        method: "deleteMessage".to_string(),
        body: serde_json::json!({
            "chat_id": chat_id,
            "message_id": message_id,
        }),
    }
}

/// `sendChecklist` for creating a native Telegram interactive checklist (when `business_connection_id`
/// is supplied), or `format_checklist_markdown` for standard chats.
pub fn send_checklist(
    chat_id: i64,
    business_connection_id: &str,
    title: &str,
    tasks: &[String],
    reply_to: Option<i64>,
    thread: Option<i64>,
) -> BotCall {
    let task_items: Vec<serde_json::Value> = tasks
        .iter()
        .enumerate()
        .map(|(idx, text)| {
            serde_json::json!({
                "id": (idx + 1) as i64,
                "text": text,
            })
        })
        .collect();
    let mut body = serde_json::json!({
        "business_connection_id": business_connection_id,
        "chat_id": chat_id,
        "checklist": {
            "title": title,
            "others_can_add_tasks": true,
            "others_can_mark_tasks_as_done": true,
            "tasks": task_items,
        }
    });
    if let Some(id) = reply_to {
        body["reply_parameters"] = serde_json::json!({ "message_id": id });
    }
    apply_telegram_message_thread_id(&mut body, thread);
    BotCall {
        method: "sendChecklist".to_string(),
        body,
    }
}

/// `sendPhoto`, `sendVideo`, `sendAudio` (`music`), `sendVoice`, or `sendDocument` by URL or `file_id`.
#[allow(clippy::too_many_arguments)]
pub fn send_media_ref(
    chat_id: i64,
    kind: &str,
    file_ref: &str,
    caption: Option<&str>,
    title: Option<&str>,
    performer: Option<&str>,
    reply_to: Option<i64>,
    thread: Option<i64>,
) -> BotCall {
    let (method, field) = media_method(kind);
    let mut body = serde_json::json!({
        "chat_id": chat_id,
        field: file_ref,
    });
    if let Some(cap) = caption.filter(|c| !c.trim().is_empty()) {
        body["caption"] = serde_json::json!(markdown_to_telegram_html(cap));
        body["parse_mode"] = serde_json::json!("HTML");
    }
    if field == "audio" {
        if let Some(t) = title.map(str::trim).filter(|s| !s.is_empty()) {
            body["title"] = serde_json::json!(t);
        }
        if let Some(p) = performer.map(str::trim).filter(|s| !s.is_empty()) {
            body["performer"] = serde_json::json!(p);
        }
    }
    if let Some(id) = reply_to {
        body["reply_to_message_id"] = serde_json::json!(id);
    }
    apply_telegram_message_thread_id(&mut body, thread);
    BotCall {
        method: method.to_string(),
        body,
    }
}

pub(super) fn media_method(kind: &str) -> (&'static str, &'static str) {
    match kind {
        "photo" | "image" => ("sendPhoto", "photo"),
        "video" => ("sendVideo", "video"),
        "audio" | "music" => ("sendAudio", "audio"),
        "voice" => ("sendVoice", "voice"),
        "animation" | "gif" => ("sendAnimation", "animation"),
        _ => ("sendDocument", "document"),
    }
}

/// `getFile` to resolve a `file_id` into a downloadable `file_path`.
pub fn get_file(file_id: &str) -> BotCall {
    BotCall {
        method: "getFile".to_string(),
        body: serde_json::json!({ "file_id": file_id }),
    }
}

/// Extract `result.file_path` from a `getFile` response.
pub fn file_path_of(response: &str) -> Result<String, PollError> {
    ok_result(response)?
        .get("file_path")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| PollError::BadPayload("no result.file_path".to_string()))
}

/// `getMe`: the bot's own id and user name, so a recorded token is shown as the
/// bot it is before any message is posted with it.
pub fn get_me() -> BotCall {
    BotCall {
        method: "getMe".to_string(),
        body: serde_json::json!({}),
    }
}

/// `getManagedBotToken` (Telegram Bot API 9.6+) to retrieve the HTTP API token
/// of a subordinate bot managed by this bot.
pub fn get_managed_bot_token(bot_user_id: i64) -> BotCall {
    BotCall {
        method: "getManagedBotToken".to_string(),
        body: serde_json::json!({ "user_id": bot_user_id }),
    }
}

/// `replaceManagedBotToken` (Telegram Bot API 9.6+) to revoke and regenerate
/// the HTTP API token of a subordinate bot managed by this bot.
pub fn replace_managed_bot_token(bot_user_id: i64) -> BotCall {
    BotCall {
        method: "replaceManagedBotToken".to_string(),
        body: serde_json::json!({ "user_id": bot_user_id }),
    }
}

/// Extract the subordinate bot token string from a `getManagedBotToken` or
/// `replaceManagedBotToken` response.
pub fn managed_bot_token_of(response: &str) -> Result<String, PollError> {
    ok_result(response)?
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| PollError::BadPayload("no token string in result".to_string()))
}

/// The bot's id, user name, and `can_manage_bots` capability from a `getMe` response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BotIdentity {
    pub id: i64,
    pub username: String,
    pub can_manage_bots: bool,
}

pub fn bot_identity(response: &str) -> Result<BotIdentity, PollError> {
    let value = ok_result(response)?;
    let id = value
        .get("id")
        .and_then(serde_json::Value::as_i64)
        .ok_or_else(|| PollError::BadPayload("no result.id".to_string()))?;
    let username = value
        .get("username")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| PollError::BadPayload("no result.username".to_string()))?
        .to_string();
    let can_manage_bots = value
        .get("can_manage_bots")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    Ok(BotIdentity {
        id,
        username,
        can_manage_bots,
    })
}

/// The message id Telegram assigned to a `sendMessage` we made.
pub fn sent_message_id(response: &str) -> Result<i64, PollError> {
    ok_result(response)?
        .get("message_id")
        .and_then(serde_json::Value::as_i64)
        .ok_or_else(|| PollError::BadPayload("no result.message_id".to_string()))
}

/// `result` of a Bot API answer, which must say `ok: true`.
fn ok_result(response: &str) -> Result<serde_json::Value, PollError> {
    let value: serde_json::Value =
        serde_json::from_str(response).map_err(|e| PollError::BadPayload(e.to_string()))?;
    if value.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
        let description = value
            .get("description")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("telegram refused the call");
        return Err(PollError::Refused(description.to_string()));
    }
    value
        .get("result")
        .cloned()
        .ok_or_else(|| PollError::BadPayload("no result".to_string()))
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PollError {
    #[error("telegram payload: {0}")]
    BadPayload(String),
    /// Telegram answered `ok: false` and said why.
    #[error("telegram refused: {0}")]
    Refused(String),
}
