use super::{
    Address, ChannelKind, Content, InboundEvent, IngressError, MAX_BODY_BYTES, Media, MediaKind,
    MediaSource, Mention,
};
use serde_json::Value;

/// Normalize a MESSAGE_CREATE dispatch from a host-authenticated bot Gateway
/// connection, not an HTTP webhook. The host owns intents and session lifecycle.
pub fn normalize_discord_gateway_message(
    frame: &[u8],
) -> Result<Option<InboundEvent>, IngressError> {
    if frame.len() > MAX_BODY_BYTES {
        return Err(IngressError::TooLarge);
    }
    let frame: Value = serde_json::from_slice(frame).map_err(|_| IngressError::Malformed)?;
    if !frame.is_object() {
        return Err(IngressError::Malformed);
    }
    if frame["op"].as_u64() != Some(0) || frame["t"] != "MESSAGE_CREATE" {
        return Ok(None);
    }
    let data = &frame["d"];
    if data["author"]["bot"].as_bool() == Some(true) || data["webhook_id"].is_string() {
        return Ok(None);
    }
    let snowflake = |path: &str| {
        data.pointer(path)
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty() && v.len() <= 24 && v.bytes().all(|b| b.is_ascii_digit()))
            .ok_or(IngressError::Malformed)
    };
    let message = snowflake("/id")?;
    let channel = snowflake("/channel_id")?;
    let sender = snowflake("/author/id")?;
    let mut content = Content {
        text: data["content"].as_str().unwrap_or_default().into(),
        ..Default::default()
    };
    if let Some(mentions) = data["mentions"].as_array() {
        content.mentions.extend(mentions.iter().filter_map(|v| {
            Some(Mention {
                id: v["id"].as_str()?.into(),
                label: None,
            })
        }));
    }
    if let Some(files) = data["attachments"].as_array() {
        content.attachments.extend(files.iter().filter_map(|v| {
            let mime = v["content_type"].as_str();
            let kind = match mime
                .and_then(|v| v.split_once('/'))
                .map(|(primary, _)| primary)
            {
                Some("image") => MediaKind::Image,
                Some("audio") => MediaKind::Audio,
                Some("video") => MediaKind::Video,
                _ => MediaKind::File,
            };
            let url = v["url"].as_str()?;
            Some(Media {
                kind,
                source: MediaSource::Url(url.into()),
                name: v["filename"].as_str().map(str::to_owned),
                content_type: mime.map(str::to_owned),
            })
        }));
    }
    if content == Content::default() {
        return Ok(None);
    }
    // Threads are themselves Gateway channels. No parent-channel inference.
    Ok(Some(InboundEvent {
        channel: ChannelKind::Discord,
        event_id: message.into(),
        sender_id: sender.into(),
        address: Address {
            conversation: channel.into(),
            thread: None,
            reply_target: Some(message.into()),
        },
        content,
        action_id: None,
        context_only: false,
    }))
}
