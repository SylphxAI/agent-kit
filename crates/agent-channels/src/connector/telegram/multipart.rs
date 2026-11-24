//! Multipart wire encoding; callers own source admission, limits and I/O.

use super::bot_api::media_method;
use super::markdown_to_telegram_html;
use crate::connector::DeliveryError;

pub struct FilePart<'a> {
    pub field: &'a str,
    pub file_name: &'a str,
    pub content_type: &'a str,
    pub bytes: &'a [u8],
}

/// Encode form fields and one bounded file. Use a fresh, unpredictable boundary
/// per request; it must be an unquoted ASCII token.
pub fn encode_multipart<'a>(
    boundary: &str,
    fields: impl IntoIterator<Item = (&'a str, String)>,
    file: FilePart<'_>,
) -> Result<(String, Vec<u8>), DeliveryError> {
    let name = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    if !(1..=70).contains(&boundary.len())
        || !boundary
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        || !name(file.field)
        || file.content_type.is_empty()
        || !file.content_type.bytes().all(|b| b.is_ascii_graphic())
    {
        return Err(DeliveryError::InvalidMessage);
    }
    let file_name = file
        .file_name
        .replace('"', "%22")
        .replace('\\', "%5C")
        .replace(char::is_control, "_");
    let mut payload = Vec::with_capacity(file.bytes.len() + 1024);
    for (field, value) in fields {
        if !name(field) {
            return Err(DeliveryError::InvalidMessage);
        }
        payload.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{field}\"\r\n\r\n{value}\r\n").as_bytes());
    }
    payload.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{}\"; filename=\"{file_name}\"\r\nContent-Type: {}\r\n\r\n", file.field, file.content_type).as_bytes());
    payload.extend_from_slice(file.bytes);
    payload.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    Ok((format!("multipart/form-data; boundary={boundary}"), payload))
}

/// Prepare the fixed Bot API method, file part name, and text fields. An
/// explicit reply field can be added by the host for each delivery attempt.
pub fn media_upload_fields(
    chat_id: i64,
    kind: &str,
    caption: Option<&str>,
    title: Option<&str>,
    performer: Option<&str>,
    thread: Option<i64>,
) -> (&'static str, &'static str, Vec<(&'static str, String)>) {
    let (method, field) = media_method(kind);
    let mut fields = vec![("chat_id", chat_id.to_string())];
    if let Some(caption) = caption.filter(|s| !s.trim().is_empty()) {
        fields.push(("caption", markdown_to_telegram_html(caption)));
        fields.push(("parse_mode", "HTML".into()));
    }
    if field == "audio" {
        for (key, value) in [("title", title), ("performer", performer)] {
            if let Some(value) = value.map(str::trim).filter(|s| !s.is_empty()) {
                fields.push((key, value.into()));
            }
        }
    }
    if let Some(id) = thread {
        fields.push(("message_thread_id", id.to_string()));
    }
    (method, field, fields)
}
