//! Telegram Bot API extensions for hosts that need lossless provider details.
//!
//! The normalized connector is sufficient for common messages. Use these wire
//! types for provider-specific requests, polling, service updates and rich
//! content. Transport, authorization policy and update custody stay in the host.

mod bot_api;
mod chat;
mod formatting;
mod message;
mod multipart;
mod native_api;
mod update;
mod user;

pub use bot_api::{
    API, BotCall, BotIdentity, PollError, bot_identity, delete_message, edit_message_text,
    file_path_of, get_file, get_managed_bot_token, get_me, managed_bot_token_of,
    replace_managed_bot_token, send_checklist, send_location, send_media_ref, send_message,
    send_plain_message, send_poll, sent_message_id, set_message_reaction,
};
pub use chat::{
    pin_chat_message, set_chat_description, set_chat_title, stop_poll, unpin_chat_message,
};
pub use formatting::{
    escape_rich_markdown_dollars, format_checklist_markdown, markdown_to_telegram_html,
    preserve_rich_markdown_line_breaks, rich_markdown,
};
pub use message::{MediaAttachment, MessageQuote, TgChat, TgManagedBotCreated, TgMessage};
pub use multipart::{FilePart, encode_multipart, media_upload_fields};
pub use native_api::scoped_call;
pub use update::{
    ALLOWED_UPDATES, ChatMember, ChatMemberUpdate, TgManagedBotUpdated, TgMessageReactionUpdated,
    TgReactionType, Update, decode_updates, get_updates, updates_payload,
};
pub use user::TgUser;

/// Construct the relative path for an explicit Bot API call. This contains the
/// credential; send it only to the configured Bot API origin and never log it.
pub fn bot_api_path(token: &super::Secret, method: &str) -> Result<String, super::DeliveryError> {
    let valid = |text: &str| {
        !text.is_empty()
            && text.len() <= 256
            && text
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    };
    if !token.expose().split_once(':').is_some_and(|(id, secret)| {
        !id.is_empty() && id.len() <= 24 && id.bytes().all(|b| b.is_ascii_digit()) && valid(secret)
    }) {
        return Err(super::DeliveryError::InvalidConfiguration);
    }
    if method.is_empty() || !method.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err(super::DeliveryError::InvalidMessage);
    }
    Ok(format!("/bot{}/{method}", token.expose()))
}

/// Verify Telegram's shared callback token without inspecting the payload.
pub fn verify_webhook_secret(expected: &str, provided: Option<&str>) -> bool {
    !expected.trim().is_empty()
        && provided.is_some_and(|value| super::signing::equal(expected, value))
}

/// Derive a Telegram-compatible callback token using a caller-supplied binding label.
pub fn derive_webhook_secret(key: &[u8], binding: &[u8]) -> String {
    use hmac::Mac as _;
    super::signing::sha256_mac(key, binding)
        .finalize()
        .into_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests;
