//! Telegram message decoding: content, service messages, media, mentions, topics, and quotes.

use crate::telegram::forum_thread_id;
use serde::{Deserialize, Serialize};

use super::TgUser;

/// One attached media file on an inbound Telegram message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaAttachment {
    /// `"photo"`, `"video"`, `"video_note"`, `"audio"`, `"voice"`, `"document"`,
    /// `"animation"`, or `"sticker"`.
    pub kind: String,
    pub file_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_unique_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_size: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_s: Option<i64>,
}

/// The author, text and provider coordinates of a reply or pinned message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MessageQuote {
    pub chat_id: i64,
    pub message_id: i64,
    pub sender_id: Option<i64>,
    pub author_is_bot: bool,
    pub author: String,
    pub text: String,
    pub mentions: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct TgManagedBotCreated {
    pub bot: TgUser,
}

#[derive(Clone, Debug, Deserialize)]
pub struct TgMessage {
    pub message_id: i64,
    pub chat: TgChat,
    pub date: i64,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    caption: Option<String>,
    #[serde(default)]
    rich_message: Option<serde_json::Value>,
    #[serde(default)]
    message_thread_id: Option<i64>,
    #[serde(default)]
    is_topic_message: Option<bool>,
    #[serde(default)]
    pub from: Option<TgUser>,
    /// The message this one answers, as Telegram sends it. Its presence is the
    /// only proof of a quote; nothing is inferred from the text.
    #[serde(default)]
    reply_to_message: Option<Box<TgMessage>>,
    /// Selected passage when a user quotes a substring of `reply_to_message`.
    #[serde(default)]
    quote: Option<TgTextQuote>,
    /// Service update when a message is pinned in the chat.
    #[serde(default)]
    pinned_message: Option<Box<TgMessage>>,
    /// Service update when members join or are added to the group.
    #[serde(default)]
    pub new_chat_members: Vec<TgUser>,
    /// Service update when a member leaves or is removed from the group.
    #[serde(default)]
    pub left_chat_member: Option<TgUser>,
    /// The `@name` handles this message's text mentions, when Telegram sends
    /// them, and their user ids.
    #[serde(default)]
    entities: Vec<TgEntity>,
    #[serde(default)]
    caption_entities: Vec<TgEntity>,
    #[serde(default)]
    photo: Vec<TgPhotoSize>,
    #[serde(default)]
    video: Option<TgMediaFile>,
    #[serde(default)]
    video_note: Option<TgMediaFile>,
    #[serde(default)]
    audio: Option<TgMediaFile>,
    #[serde(default)]
    voice: Option<TgMediaFile>,
    #[serde(default)]
    document: Option<TgMediaFile>,
    #[serde(default)]
    animation: Option<TgMediaFile>,
    #[serde(default)]
    sticker: Option<TgSticker>,
    #[serde(default)]
    location: Option<TgLocation>,
    #[serde(default)]
    venue: Option<TgVenue>,
    #[serde(default)]
    contact: Option<TgContact>,
    #[serde(default)]
    poll: Option<TgPoll>,
    #[serde(default)]
    checklist: Option<TgChecklist>,
    #[serde(default)]
    dice: Option<TgDice>,
    /// Service update when a user creates a managed bot via `KeyboardButtonRequestManagedBot` (Bot API 9.6+).
    #[serde(default)]
    pub managed_bot_created: Option<TgManagedBotCreated>,
}

#[derive(Clone, Debug, Deserialize)]
struct TgPhotoSize {
    file_id: String,
    #[serde(default)]
    file_unique_id: Option<String>,
    #[serde(default)]
    width: Option<i64>,
    #[serde(default)]
    height: Option<i64>,
    #[serde(default)]
    file_size: Option<i64>,
}

#[derive(Clone, Debug, Deserialize)]
struct TgMediaFile {
    file_id: String,
    #[serde(default)]
    file_unique_id: Option<String>,
    #[serde(default)]
    file_name: Option<String>,
    #[serde(default)]
    mime_type: Option<String>,
    #[serde(default)]
    file_size: Option<i64>,
    #[serde(default)]
    width: Option<i64>,
    #[serde(default)]
    height: Option<i64>,
    #[serde(default)]
    duration: Option<i64>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    performer: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct TgSticker {
    file_id: String,
    #[serde(default)]
    file_unique_id: Option<String>,
    #[serde(default)]
    emoji: Option<String>,
    #[serde(default)]
    set_name: Option<String>,
    #[serde(default)]
    width: Option<i64>,
    #[serde(default)]
    height: Option<i64>,
    #[serde(default)]
    file_size: Option<i64>,
}

#[derive(Clone, Debug, Deserialize)]
struct TgLocation {
    latitude: f64,
    longitude: f64,
    #[serde(default)]
    horizontal_accuracy: Option<f64>,
    #[serde(default)]
    live_period: Option<i64>,
    #[serde(default)]
    heading: Option<i64>,
}

#[derive(Clone, Debug, Deserialize)]
struct TgVenue {
    location: TgLocation,
    title: String,
    address: String,
}

#[derive(Clone, Debug, Deserialize)]
struct TgContact {
    phone_number: String,
    first_name: String,
    #[serde(default)]
    last_name: Option<String>,
    #[serde(default)]
    user_id: Option<i64>,
}

#[derive(Clone, Debug, Deserialize)]
struct TgPollOption {
    text: String,
    #[serde(default)]
    voter_count: i64,
}

#[derive(Clone, Debug, Deserialize)]
struct TgPoll {
    #[serde(default)]
    id: Option<String>,
    question: String,
    #[serde(default)]
    options: Vec<TgPollOption>,
    #[serde(default)]
    total_voter_count: i64,
    #[serde(default)]
    is_closed: bool,
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    allows_multiple_answers: bool,
}

#[derive(Clone, Debug, Deserialize)]
struct TgChecklistTask {
    id: i64,
    text: String,
    #[serde(default)]
    completion_date: Option<i64>,
}

#[derive(Clone, Debug, Deserialize)]
struct TgChecklist {
    title: String,
    #[serde(default)]
    tasks: Vec<TgChecklistTask>,
}

#[derive(Clone, Debug, Deserialize)]
struct TgDice {
    emoji: String,
    value: i64,
}

#[derive(Clone, Debug, Deserialize)]
struct TgTextQuote {
    #[serde(default)]
    text: String,
}

/// One formatting or mention entity. Only a mention is read; the rest are
/// Telegram's formatting markup.
#[derive(Clone, Debug, Deserialize)]
struct TgEntity {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    length: Option<usize>,
    #[serde(default)]
    user: Option<TgUser>,
}

impl TgMessage {
    /// The text of a typed message, else the caption of a media message, else
    /// the flattened text of a `RichMessage` (`sendRichMessage` / Bot API 10.1+).
    pub fn body(&self) -> Option<String> {
        if let Some(t) = self
            .text
            .as_deref()
            .or(self.caption.as_deref())
            .filter(|text| !text.is_empty())
        {
            return Some(t.to_string());
        }
        self.rich_message
            .as_ref()
            .and_then(extract_rich_message_text)
            .filter(|t| !t.is_empty())
    }

    /// Collect all media attachments on this message (picking the largest resolution for `photo`).
    pub fn attachments(&self) -> Vec<MediaAttachment> {
        let mut out = Vec::new();
        if let Some(best) = self.photo.iter().max_by_key(|p| {
            (
                p.file_size.unwrap_or(0),
                p.width.unwrap_or(0) * p.height.unwrap_or(0),
            )
        }) {
            out.push(MediaAttachment {
                kind: "photo".to_string(),
                file_id: best.file_id.clone(),
                file_unique_id: best.file_unique_id.clone(),
                file_name: None,
                mime_type: Some("image/jpeg".to_string()),
                file_size: best.file_size,
                width: best.width,
                height: best.height,
                duration_s: None,
            });
        }
        for (kind, item) in [
            ("video", &self.video),
            ("video_note", &self.video_note),
            ("audio", &self.audio),
            ("voice", &self.voice),
            ("document", &self.document),
            ("animation", &self.animation),
        ] {
            if let Some(media) = item {
                let file_name =
                    media
                        .file_name
                        .clone()
                        .or_else(|| match (&media.performer, &media.title) {
                            (Some(p), Some(t)) => Some(format!("{p} - {t}")),
                            (None, Some(t)) => Some(t.clone()),
                            _ => None,
                        });
                out.push(MediaAttachment {
                    kind: kind.to_string(),
                    file_id: media.file_id.clone(),
                    file_unique_id: media.file_unique_id.clone(),
                    file_name,
                    mime_type: media.mime_type.clone(),
                    file_size: media.file_size,
                    width: media.width,
                    height: media.height,
                    duration_s: media.duration,
                });
            }
        }
        if let Some(sticker) = &self.sticker {
            out.push(MediaAttachment {
                kind: "sticker".to_string(),
                file_id: sticker.file_id.clone(),
                file_unique_id: sticker.file_unique_id.clone(),
                file_name: sticker.set_name.clone(),
                mime_type: None,
                file_size: sticker.file_size,
                width: sticker.width,
                height: sticker.height,
                duration_s: None,
            });
        }
        out
    }

    /// Rich structured summary for non-text payloads (GPS location, venue, poll, contact, dice, or media).
    fn rich_payload_summary(&self) -> Option<String> {
        let mut parts = Vec::new();
        if let Some(venue) = &self.venue {
            parts.push(format!(
                "[Venue: {} — {} (GPS: {:.6}, {:.6})]",
                venue.title, venue.address, venue.location.latitude, venue.location.longitude
            ));
        } else if let Some(loc) = &self.location {
            let mut details = format!(
                "latitude={:.6}, longitude={:.6}",
                loc.latitude, loc.longitude
            );
            if let Some(acc) = loc.horizontal_accuracy {
                details.push_str(&format!(", accuracy={acc:.1}m"));
            }
            if let Some(live) = loc.live_period {
                details.push_str(&format!(", live_period={live}s"));
            }
            if let Some(heading) = loc.heading {
                details.push_str(&format!(", heading={heading}deg"));
            }
            parts.push(format!("[Location GPS: {details}]"));
        }
        if let Some(poll) = &self.poll {
            let kind = poll.kind.as_deref().unwrap_or("regular");
            let opts: Vec<String> = poll
                .options
                .iter()
                .enumerate()
                .map(|(idx, o)| format!("{}. {} ({} votes)", idx + 1, o.text, o.voter_count))
                .collect();
            parts.push(format!(
                "[Poll{} ({kind}, multi={}, total_votes={}, closed={}): \"{}\" | Options: {}]",
                poll.id
                    .as_deref()
                    .map(|id| format!(" #{id}"))
                    .unwrap_or_default(),
                poll.allows_multiple_answers,
                poll.total_voter_count,
                poll.is_closed,
                poll.question,
                opts.join("; ")
            ));
        }
        if let Some(checklist) = &self.checklist {
            let tasks: Vec<String> = checklist
                .tasks
                .iter()
                .map(|t| {
                    let mark = if t.completion_date.unwrap_or(0) > 0 {
                        "☑"
                    } else {
                        "☐"
                    };
                    format!("{mark} #{}. {}", t.id, t.text)
                })
                .collect();
            parts.push(format!(
                "[Checklist: \"{}\" | Tasks: {}]",
                checklist.title,
                tasks.join("; ")
            ));
        }
        if let Some(contact) = &self.contact {
            let full_name = match &contact.last_name {
                Some(last) if !last.is_empty() => format!("{} {}", contact.first_name, last),
                _ => contact.first_name.clone(),
            };
            parts.push(format!(
                "[Contact: {} | phone={}{}]",
                full_name,
                contact.phone_number,
                contact
                    .user_id
                    .map(|id| format!(" | user_id={id}"))
                    .unwrap_or_default()
            ));
        }
        if let Some(dice) = &self.dice {
            parts.push(format!("[Dice {}: value={}]", dice.emoji, dice.value));
        }
        if let Some(sticker) = &self.sticker {
            parts.push(format!(
                "[Sticker{}{}: file_id={}]",
                sticker
                    .emoji
                    .as_deref()
                    .map(|e| format!(" {e}"))
                    .unwrap_or_default(),
                sticker
                    .set_name
                    .as_deref()
                    .map(|s| format!(" from set \"{s}\""))
                    .unwrap_or_default(),
                sticker.file_id
            ));
        }
        for att in self.attachments() {
            if att.kind == "sticker" {
                continue;
            }
            let mut meta = vec![format!("file_id={}", att.file_id)];
            if let Some(name) = &att.file_name {
                meta.push(format!("name=\"{name}\""));
            }
            if let Some(mime) = &att.mime_type {
                meta.push(format!("mime={mime}"));
            }
            if let Some(w) = att.width
                && let Some(h) = att.height
            {
                meta.push(format!("{w}x{h}"));
            }
            if let Some(d) = att.duration_s {
                meta.push(format!("duration={d}s"));
            }
            if let Some(sz) = att.file_size {
                meta.push(format!("size={sz}B"));
            }
            parts.push(format!("[Attachment ({}): {}]", att.kind, meta.join(", ")));
        }
        (!parts.is_empty()).then(|| parts.join("\n"))
    }

    /// A readable representation of this message: the message body/caption, any rich
    /// media/location/poll summary, or a formatted `[Pinned message #<id> by <author>]: ...` notice.
    pub fn deliverable_text(&self) -> Option<String> {
        let rich = self.rich_payload_summary();
        if let Some(body) = self.body() {
            return match rich {
                Some(summary) => Some(format!("{body}\n{summary}")),
                None => Some(body),
            };
        }
        if let Some(summary) = rich {
            return Some(summary);
        }
        if !self.new_chat_members.is_empty() {
            let names: Vec<String> = self
                .new_chat_members
                .iter()
                .map(TgUser::display_name)
                .collect();
            return Some(format!("[Member joined group: {}]", names.join(", ")));
        }
        if let Some(left) = &self.left_chat_member {
            return Some(format!(
                "[Member removed/left group: {}]",
                left.display_name()
            ));
        }
        let pinned = self.pinned_message.as_deref()?;
        let author = pinned
            .from
            .as_ref()
            .map(TgUser::display_name)
            .unwrap_or_else(|| "unknown".to_string());
        let snippet = pinned
            .body()
            .or_else(|| pinned.rich_payload_summary())
            .unwrap_or_else(|| "(non-text message)".to_string());
        Some(format!(
            "[Pinned message #{} by {author}]: {snippet}",
            pinned.message_id
        ))
    }

    /// Extract `@username` and `text_mention` handles from `entities`, `caption_entities`, and `rich_message`.
    fn entity_mentions(&self) -> Vec<String> {
        let raw_text = self.body().unwrap_or_default();
        let utf16: Vec<u16> = raw_text.encode_utf16().collect();
        let mut out = Vec::new();
        for entity in self.entities.iter().chain(self.caption_entities.iter()) {
            let handle = if entity.kind == "text_mention" {
                entity.user.as_ref().map(TgUser::handle)
            } else if entity.kind == "mention" {
                match (entity.offset, entity.length) {
                    (Some(offset), Some(length))
                        if length > 0 && offset.saturating_add(length) <= utf16.len() =>
                    {
                        Some(String::from_utf16_lossy(&utf16[offset..offset + length]))
                    }
                    _ => entity.user.as_ref().map(TgUser::handle),
                }
            } else {
                None
            };
            if let Some(handle) = handle.filter(|h| !h.is_empty())
                && !out.contains(&handle)
            {
                out.push(handle);
            }
        }
        if let Some(rm) = &self.rich_message {
            extract_rich_message_mentions(rm, &mut out);
        }
        out
    }

    /// The forum topic `message_thread_id`, or `None` when `message_thread_id`
    /// is merely a non-forum supergroup reply chain root id.
    pub fn forum_thread_id(&self) -> Option<i64> {
        let tid = self.message_thread_id?;
        // Adapt legacy updates that omit topic flags before canonical validation.
        let is_topic = self.is_topic_message == Some(true)
            || self.chat.is_forum == Some(true)
            || (self.is_topic_message.is_none()
                && self.chat.is_forum.is_none()
                && !self
                    .reply_to_message
                    .as_ref()
                    .is_some_and(|r| r.message_id == tid));
        forum_thread_id(
            &serde_json::json!({"is_topic_message": is_topic, "message_thread_id": tid}),
        )
    }

    pub fn quoted_attachments(&self) -> Vec<MediaAttachment> {
        self.reply_to_message
            .as_deref()
            .or(self.pinned_message.as_deref())
            .map(TgMessage::attachments)
            .unwrap_or_default()
    }

    /// The quote Telegram sent with this message: the replied-to (or pinned)
    /// message's own author, text and mentions, and the ids that name it.
    /// The host can suppress sensitive source text before the quote is formatted.
    pub fn quote(&self, suppress: impl Fn(&str) -> bool) -> Option<MessageQuote> {
        let quoted = self
            .reply_to_message
            .as_deref()
            .or(self.pinned_message.as_deref())?;
        let mentions = quoted.entity_mentions();
        let base_text = self
            .quote
            .as_ref()
            .map(|q| q.text.as_str())
            .filter(|t| !t.is_empty())
            .map(str::to_string)
            .or_else(|| quoted.body());
        if base_text.as_deref().is_some_and(&suppress)
            || quoted.body().as_deref().is_some_and(&suppress)
        {
            return None;
        }
        let rich = quoted.rich_payload_summary();
        let quoted_text = match (base_text, rich) {
            (Some(body), Some(summary)) => format!("{body}\n{summary}"),
            (Some(body), None) => body,
            (None, Some(summary)) => summary,
            (None, None) => String::new(),
        };
        let quote = MessageQuote {
            chat_id: quoted.chat.id,
            message_id: quoted.message_id,
            sender_id: quoted.from.as_ref().map(|user| user.id),
            author_is_bot: quoted
                .from
                .as_ref()
                .is_some_and(|user| user.is_bot == Some(true)),
            author: quoted
                .from
                .as_ref()
                .map(TgUser::display_name)
                .unwrap_or_default(),
            text: quoted_text,
            mentions,
        };
        (!quote.author.is_empty() || !quote.text.is_empty()).then_some(quote)
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct TgChat {
    pub id: i64,
    /// `private`, `group`, `supergroup` or `channel`.
    #[serde(default, rename = "type")]
    pub kind: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    first_name: Option<String>,
    #[serde(default)]
    last_name: Option<String>,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    is_forum: Option<bool>,
}

impl TgChat {
    pub fn display_title(&self) -> Option<String> {
        if let Some(t) = self
            .title
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return Some(t.to_string());
        }
        let full = match (
            self.first_name
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty()),
            self.last_name
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty()),
        ) {
            (Some(f), Some(l)) => Some(format!("{f} {l}")),
            (Some(f), None) => Some(f.to_string()),
            (None, Some(l)) => Some(l.to_string()),
            (None, None) => None,
        };
        match (
            full,
            self.username
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty()),
        ) {
            (Some(name), Some(u)) => Some(format!("{name} (@{u})")),
            (Some(name), None) => Some(name),
            (None, Some(u)) => Some(format!("@{u}")),
            (None, None) => None,
        }
    }
}

fn extract_rich_message_text(rm: &serde_json::Value) -> Option<String> {
    let mut lines = Vec::new();
    if let Some(blocks) = rm.get("blocks").and_then(serde_json::Value::as_array) {
        for block in blocks {
            let mut block_text = String::new();
            collect_rich_text(block, &mut block_text);
            let trimmed = block_text.trim();
            if !trimmed.is_empty() {
                lines.push(trimmed.to_string());
            }
        }
    } else {
        let mut text = String::new();
        collect_rich_text(rm, &mut text);
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            lines.push(trimmed.to_string());
        }
    }
    (!lines.is_empty()).then(|| lines.join("\n"))
}

fn collect_rich_text(val: &serde_json::Value, out: &mut String) {
    match val {
        serde_json::Value::String(s) => out.push_str(s),
        serde_json::Value::Array(arr) => {
            for item in arr {
                collect_rich_text(item, out);
            }
        }
        serde_json::Value::Object(map) => {
            let kind = map
                .get("type")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            if kind == "table"
                && let Some(rows) = map.get("cells").and_then(serde_json::Value::as_array)
            {
                for (r_idx, row) in rows.iter().enumerate() {
                    if r_idx > 0 {
                        out.push('\n');
                    }
                    if let Some(cells) = row.as_array() {
                        for (c_idx, cell) in cells.iter().enumerate() {
                            if c_idx > 0 {
                                out.push_str(" | ");
                            }
                            if let Some(t) = cell.get("text") {
                                collect_rich_text(t, out);
                            }
                        }
                    }
                }
                return;
            }
            if let Some(t) = map.get("text") {
                collect_rich_text(t, out);
            } else if let Some(expr) = map
                .get("expression")
                .or_else(|| map.get("formula"))
                .or_else(|| map.get("latex"))
                .or_else(|| map.get("math"))
                .and_then(serde_json::Value::as_str)
            {
                out.push('$');
                out.push_str(expr);
                out.push('$');
            }
            if let Some(items) = map.get("items").and_then(serde_json::Value::as_array) {
                for (idx, item) in items.iter().enumerate() {
                    if idx > 0 || !out.is_empty() {
                        out.push('\n');
                    }
                    collect_rich_text(item, out);
                }
            }
            if let Some(blocks) = map.get("blocks").and_then(serde_json::Value::as_array) {
                for (idx, b) in blocks.iter().enumerate() {
                    if idx > 0 || !out.is_empty() {
                        out.push('\n');
                    }
                    collect_rich_text(b, out);
                }
            }
        }
        _ => {}
    }
}

fn extract_rich_message_mentions(val: &serde_json::Value, out: &mut Vec<String>) {
    match val {
        serde_json::Value::Array(arr) => {
            for item in arr {
                extract_rich_message_mentions(item, out);
            }
        }
        serde_json::Value::Object(map) => {
            let kind = map
                .get("type")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            if kind == "mention" {
                let mut text = String::new();
                if let Some(t) = map.get("text") {
                    collect_rich_text(t, &mut text);
                }
                let trimmed = text.trim();
                let handle = if trimmed.starts_with('@') {
                    trimmed.to_string()
                } else if !trimmed.is_empty() {
                    format!("@{trimmed}")
                } else {
                    String::new()
                };
                if !handle.is_empty() && !out.contains(&handle) {
                    out.push(handle);
                }
            } else if kind == "text_mention"
                && let Some(user_val) = map.get("user")
                && let Ok(user) = serde_json::from_value::<TgUser>(user_val.clone())
            {
                let handle = user.handle();
                if !handle.is_empty() && !out.contains(&handle) {
                    out.push(handle);
                }
            }
            for v in map.values() {
                extract_rich_message_mentions(v, out);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::extract_rich_message_text;

    #[test]
    fn inbound_rich_message_preserves_mathematical_expression_text() {
        // Historical inbound rich_message with mathematical_expression preserves expression text
        let rm = serde_json::json!({
            "blocks": [
                {
                    "type": "paragraph",
                    "text": [
                        { "type": "plain", "text": "今日 S" },
                        { "type": "mathematical_expression", "expression": "123.45 同 US" },
                        { "type": "plain", "text": "170 已經入帳" }
                    ]
                }
            ]
        });
        assert_eq!(
            extract_rich_message_text(&rm).as_deref(),
            Some("今日 S$123.45 同 US$170 已經入帳")
        );
    }
}
