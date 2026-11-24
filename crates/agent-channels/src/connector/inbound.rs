use super::{signing::*, *};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use ed25519_compact::{PublicKey, Signature};
use hmac::{Hmac, Mac};
use serde_json::json;
use sha1::Sha1;
use std::collections::BTreeMap;

use IngressError::{Malformed, Unauthorized, UnsupportedMethod};
use WebhookResponse::{Empty, Json, Text};

type Events = Result<WebhookOutcome, IngressError>;

pub(super) fn ingest(auth: &InboundAuth, req: WebhookRequest<'_>) -> Events {
    if req.method != "POST"
        && !(req.method == "GET"
            && matches!(
                auth,
                InboundAuth::Wecom { .. }
                    | InboundAuth::WechatOfficial { .. }
                    | InboundAuth::Messenger { .. }
            ))
    {
        return Err(UnsupportedMethod);
    }
    match auth {
        InboundAuth::Telegram { webhook_token, bot } => telegram(req, webhook_token, bot),
        InboundAuth::Slack {
            signing_secret,
            team_id,
            bot_user_id,
        } => slack(req, signing_secret, team_id, bot_user_id.as_deref()),
        InboundAuth::Discord {
            public_key,
            application_id,
        } => discord(req, public_key, application_id),
        InboundAuth::Email { signing_key }
        | InboundAuth::Webhook { signing_key }
        | InboundAuth::Custom { signing_key } => relay(req, auth.kind(), signing_key),
        InboundAuth::Sms {
            auth_token,
            callback_url,
            destination,
        } => sms(req, auth_token, callback_url, destination),
        InboundAuth::Feishu {
            encrypt_key,
            verification_token,
            app_id,
        } => feishu(req, encrypt_key, verification_token, app_id),
        InboundAuth::Dingtalk {
            app_secret,
            robot_code,
        } => dingtalk(req, app_secret, robot_code),
        InboundAuth::Wecom {
            token,
            encoding_aes_key,
            receiver_id,
            agent_id,
        } => wx(req, token, encoding_aes_key, receiver_id, Some(*agent_id)),
        InboundAuth::WechatOfficial {
            token,
            encoding_aes_key,
            app_id,
        } => wx(req, token, encoding_aes_key, app_id, None),
        InboundAuth::Messenger {
            app_secret,
            verify_token,
            page_id,
        } => messenger(req, app_secret, verify_token, page_id),
    }
}
fn empty(response: WebhookResponse) -> Events {
    Ok(WebhookOutcome {
        events: vec![],
        response,
    })
}
fn single(event: InboundEvent, response: WebhookResponse) -> Events {
    Ok(WebhookOutcome {
        events: vec![event],
        response,
    })
}
fn json_body(req: WebhookRequest<'_>) -> Result<Value, IngressError> {
    let body: Value = serde_json::from_slice(req.body).map_err(|_| Malformed)?;
    if !body.is_object() {
        return Err(Malformed);
    }
    Ok(body)
}
fn field<'a>(v: &'a Value, path: &str) -> Option<&'a str> {
    v.pointer(path)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
}
fn need<'a>(v: &'a Value, path: &str) -> Result<&'a str, IngressError> {
    field(v, path).ok_or(Malformed)
}
fn id(v: &Value, path: &str) -> Option<String> {
    let x = v.pointer(path)?;
    match x {
        Value::Number(n) if n.is_i64() || n.is_u64() => Some(n.to_string()),
        Value::String(s) if !s.trim().is_empty() => Some(s.clone()),
        _ => None,
    }
}
fn auth_eq(got: Option<&str>, want: &str) -> Result<(), IngressError> {
    if !want.trim().is_empty() && got.is_some_and(|v| equal(v, want)) {
        Ok(())
    } else {
        Err(Unauthorized)
    }
}
fn event(
    kind: ChannelKind,
    event_id: impl Into<String>,
    sender: impl Into<String>,
    conversation: impl Into<String>,
) -> InboundEvent {
    InboundEvent {
        channel: kind,
        event_id: event_id.into(),
        sender_id: sender.into(),
        address: Address {
            conversation: conversation.into(),
            thread: None,
            reply_target: None,
        },
        content: Content::default(),
        action_id: None,
        context_only: false,
    }
}
fn media(kind: MediaKind, source: MediaSource, v: &Value) -> Media {
    Media {
        kind,
        source,
        name: field(v, "/file_name").map(str::to_owned),
        content_type: field(v, "/mime_type").map(str::to_owned),
    }
}

fn telegram(req: WebhookRequest<'_>, token: &Secret, bot: &crate::telegram::BotIdentity) -> Events {
    auth_eq(
        req.header("X-Telegram-Bot-Api-Secret-Token"),
        token.expose(),
    )?;
    let body = json_body(req)?;
    let event_id = body["update_id"].as_u64().ok_or(Malformed)?.to_string();
    let cb = body.get("callback_query");
    let msg = if let Some(cb) = cb {
        cb.get("message")
    } else {
        body.get("message").or_else(|| body.get("channel_post"))
    };
    let Some(msg) = msg else {
        return empty(Empty);
    };
    let meta = crate::telegram::TelegramInboundMeta::from_message(msg, bot).ok_or(Malformed)?;
    if meta.chat_id == 0
        || meta.message_id <= 0
        || (msg["is_topic_message"] == true && meta.thread_id.is_none())
    {
        return Err(Malformed);
    }
    if cb.is_none()
        && (meta.sender_is_bot || (meta.sender_id.is_some() && meta.sender_id == bot.user_id()))
    {
        return empty(Empty);
    }
    let sender = if let Some(cb) = cb {
        id(cb, "/from/id").ok_or(Malformed)?
    } else {
        meta.sender_id
            .map(|v| v.to_string())
            .or_else(|| id(msg, "/sender_chat/id"))
            .ok_or(Malformed)?
    };
    let mut e = event(
        ChannelKind::Telegram,
        event_id,
        sender,
        meta.chat_id.to_string(),
    );
    e.address.thread = meta.thread_id.map(|v| v.to_string());
    e.address.reply_target = Some(meta.message_id.to_string());
    if let Some(cb) = cb {
        e.action_id = field(cb, "/data").map(str::to_owned);
        if e.action_id.is_none() {
            return empty(Empty);
        }
        return single(e, Empty);
    }
    e.context_only = meta.context_only
        || matches!(
            meta.chat_type,
            crate::telegram::TelegramChatType::Unknown | crate::telegram::TelegramChatType::Channel
        );
    e.content.text = field(msg, "/text")
        .or_else(|| field(msg, "/caption"))
        .unwrap_or_default()
        .into();
    for (key, kind) in [
        ("document", MediaKind::File),
        ("voice", MediaKind::Audio),
        ("audio", MediaKind::Audio),
        ("video", MediaKind::Video),
    ] {
        if let Some(v) = msg.get(key)
            && let Some(src) = field(v, "/file_id")
        {
            let m = media(kind, MediaSource::ProviderId(src.into()), v);
            if kind == MediaKind::File {
                e.content.attachments.push(m);
            } else {
                e.content.media.push(m);
            }
        }
    }
    if let Some(photo) = msg["photo"].as_array().and_then(|a| a.last())
        && let Some(src) = field(photo, "/file_id")
    {
        e.content.media.push(media(
            MediaKind::Image,
            MediaSource::ProviderId(src.into()),
            photo,
        ));
    }
    if let Some(entities) = msg
        .get(if msg.get("text").is_some() {
            "entities"
        } else {
            "caption_entities"
        })
        .and_then(Value::as_array)
    {
        for ent in entities {
            let label = field(ent, "/user/username").map(str::to_owned);
            if ent["type"] == "text_mention"
                && let Some(id) = id(ent, "/user/id")
            {
                e.content.mentions.push(Mention { id, label });
            } else if ent["type"] == "mention"
                && let (Some(off), Some(len)) = (ent["offset"].as_u64(), ent["length"].as_u64())
                && let Some(name) =
                    utf16_span(&e.content.text, off, len).and_then(|s| s.strip_prefix('@'))
                && !name.is_empty()
            {
                e.content.mentions.push(Mention {
                    id: name.to_owned(),
                    label: None,
                });
            }
        }
    }
    if e.content == Content::default() {
        empty(Empty)
    } else {
        single(e, Empty)
    }
}
fn utf16_span(s: &str, start: u64, len: u64) -> Option<&str> {
    let end = start.checked_add(len)?;
    let mut offset = 0;
    let mut found = None;
    for (i, c) in s.char_indices().chain(std::iter::once((s.len(), '\0'))) {
        if offset == start {
            found = Some(i);
        }
        if offset == end {
            return s.get(found?..i);
        }
        offset += c.len_utf16() as u64;
    }
    None
}

fn slack(req: WebhookRequest<'_>, key: &Secret, team: &str, bot: Option<&str>) -> Events {
    let timestamp = req
        .header("X-Slack-Request-Timestamp")
        .filter(|ts| fresh(req.unix_seconds, ts, 300))
        .ok_or(Unauthorized)?;
    let signature = req
        .header("X-Slack-Signature")
        .and_then(|v| v.strip_prefix("v0="))
        .and_then(hex::<32>)
        .ok_or(Unauthorized)?;
    let mut mac = sha256_mac(
        key.nonempty().ok_or(Unauthorized)?.as_bytes(),
        format!("v0:{timestamp}:").as_bytes(),
    );
    mac.update(req.body);
    mac.verify_slice(&signature).map_err(|_| Unauthorized)?;
    if req
        .header("content-type")
        .is_some_and(|v| v.split(';').next() == Some("application/x-www-form-urlencoded"))
    {
        return slack_action(req, team);
    }
    let body = json_body(req)?;
    match need(&body, "/type")? {
        "url_verification" => return empty(Json(json!({"challenge": need(&body, "/challenge")?}))),
        "event_callback" => (),
        _ => return empty(Empty),
    }
    auth_eq(field(&body, "/team_id"), team)?;
    let v = body.get("event").ok_or(Malformed)?;
    if !matches!(need(v, "/type")?, "message" | "app_mention")
        || v.get("bot_id").is_some()
        || v.get("subtype").is_some_and(|s| s != "file_share")
        || field(v, "/user").is_some_and(|u| Some(u) == bot)
    {
        return empty(Empty);
    }
    let mut e = event(
        ChannelKind::Slack,
        need(&body, "/event_id")?,
        need(v, "/user")?,
        need(v, "/channel")?,
    );
    e.address.reply_target = Some(need(v, "/ts")?.into());
    e.address.thread = field(v, "/thread_ts").map(str::to_owned);
    let text = field(v, "/text").unwrap_or_default();
    e.content.mentions = text
        .split("<@")
        .skip(1)
        .filter_map(|s| {
            let (token, _) = s.split_once('>')?;
            let (user, label) = token
                .split_once('|')
                .map_or((token, None), |(u, l)| (u, Some(l.to_owned())));
            (!user.is_empty() && user.bytes().all(|b| b.is_ascii_alphanumeric())).then(|| Mention {
                id: user.into(),
                label,
            })
        })
        .collect();
    e.content.text = text
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&");
    if let Some(files) = v["files"].as_array() {
        for file in files {
            if let Some(fid) = field(file, "/id") {
                e.content.attachments.push(Media {
                    kind: MediaKind::File,
                    source: MediaSource::ProviderId(fid.into()),
                    name: field(file, "/name").map(str::to_owned),
                    content_type: field(file, "/mimetype").map(str::to_owned),
                });
            }
        }
    }
    single(e, Empty)
}

fn slack_action(req: WebhookRequest<'_>, team: &str) -> Events {
    let mut pairs = form_urlencoded::parse(req.body);
    let (_, payload) = pairs
        .next()
        .filter(|(k, _)| k == "payload")
        .ok_or(Malformed)?;
    if pairs.next().is_some() {
        return Err(Malformed);
    }
    let body: Value = serde_json::from_str(&payload).map_err(|_| Malformed)?;
    auth_eq(field(&body, "/team/id"), team)?;
    if need(&body, "/type")? != "block_actions" {
        return empty(Empty);
    }
    let actions = body["actions"]
        .as_array()
        .filter(|list| list.len() == 1)
        .ok_or(Malformed)?;
    let Some(value) = field(&actions[0], "/value") else {
        return empty(Empty);
    };
    let mut e = event(
        ChannelKind::Slack,
        need(&body, "/trigger_id")?,
        need(&body, "/user/id")?,
        need(&body, "/channel/id")?,
    );
    e.address.thread = field(&body, "/message/thread_ts").map(str::to_owned);
    e.address.reply_target = Some(need(&body, "/container/message_ts")?.into());
    e.action_id = Some(value.into());
    single(e, Empty)
}

fn discord(req: WebhookRequest<'_>, key: &[u8; 32], app: &str) -> Events {
    let timestamp = req
        .header("X-Signature-Timestamp")
        .filter(|ts| fresh(req.unix_seconds, ts, 300))
        .ok_or(Unauthorized)?;
    let sig = req
        .header("X-Signature-Ed25519")
        .and_then(hex::<64>)
        .ok_or(Unauthorized)?;
    let mut verifier = PublicKey::new(*key)
        .verify_incremental(&Signature::new(sig))
        .map_err(|_| Unauthorized)?;
    verifier.absorb(timestamp.as_bytes());
    verifier.absorb(req.body);
    verifier.verify().map_err(|_| Unauthorized)?;
    let body = json_body(req)?;
    auth_eq(field(&body, "/application_id"), app)?;
    match body["type"].as_u64().ok_or(Malformed)? {
        1 => empty(Json(json!({"type": 1}))),
        3 => {
            let mut e = event(
                ChannelKind::Discord,
                need(&body, "/id")?,
                field(&body, "/member/user/id")
                    .or_else(|| field(&body, "/user/id"))
                    .ok_or(Malformed)?,
                need(&body, "/channel_id")?,
            );
            e.address.reply_target = Some(need(&body, "/message/id")?.into());
            e.action_id = Some(need(&body, "/data/custom_id")?.into());
            single(e, Json(json!({"type": 6})))
        }
        // Slash commands and modals need an explicit interaction-response policy.
        _ => empty(Json(
            json!({"type": 4, "data": {"content": "Unsupported interaction", "flags": 64}}),
        )),
    }
}

fn relay(req: WebhookRequest<'_>, kind: ChannelKind, key: &Secret) -> Events {
    let event_id = verify_webhook(key, req)?;
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct RelayEvent {
        sender_id: String,
        address: Address,
        content: Content,
        action_id: Option<String>,
    }
    let wire: RelayEvent = serde_json::from_slice(req.body).map_err(|_| Malformed)?;
    if wire.sender_id.trim().is_empty()
        || wire.address.conversation.trim().is_empty()
        || [
            &wire.address.thread,
            &wire.address.reply_target,
            &wire.action_id,
        ]
        .into_iter()
        .flatten()
        .any(|v| v.trim().is_empty())
        || (wire.content == Content::default() && wire.action_id.is_none())
    {
        return Err(Malformed);
    }
    let mut e = event(kind, event_id, wire.sender_id, &wire.address.conversation);
    e.address = wire.address;
    e.content = wire.content;
    e.action_id = wire.action_id;
    single(e, Empty)
}

fn sms(req: WebhookRequest<'_>, key: &Secret, url: &str, destination: &str) -> Events {
    if url.trim().is_empty() {
        return Err(Unauthorized);
    }
    let sig = req
        .header("X-Twilio-Signature")
        .and_then(|s| STANDARD.decode(s).ok())
        .ok_or(Unauthorized)?;
    std::str::from_utf8(req.body).map_err(|_| Malformed)?;
    let mut params = BTreeMap::new();
    for (key, value) in form_urlencoded::parse(req.body) {
        if params.insert(key, value).is_some() {
            return Err(Malformed);
        }
    }
    let mut mac = Hmac::<Sha1>::new_from_slice(key.nonempty().ok_or(Unauthorized)?.as_bytes())
        .expect("HMAC accepts any key length");
    mac.update(url.as_bytes());
    for (key, value) in &params {
        mac.update(key.as_bytes());
        mac.update(value.as_bytes());
    }
    mac.verify_slice(&sig).map_err(|_| Unauthorized)?;
    let param = |key: &str| {
        params
            .get(key)
            .map(|v| v.as_ref())
            .filter(|v| !v.trim().is_empty())
    };
    auth_eq(param("To"), destination)?;
    let from = param("From").ok_or(Malformed)?;
    let mut e = event(
        ChannelKind::Sms,
        param("MessageSid").ok_or(Malformed)?,
        from,
        from,
    );
    e.content.text = param("Body").unwrap_or_default().into();
    let count = param("NumMedia")
        .unwrap_or("0")
        .parse::<usize>()
        .ok()
        .filter(|n| *n <= 10)
        .ok_or(Malformed)?;
    for i in 0..count {
        let url = param(&format!("MediaUrl{i}")).ok_or(Malformed)?;
        let content_type = param(&format!("MediaContentType{i}"));
        let kind = match content_type
            .and_then(|v| v.split_once('/'))
            .map(|(major, _)| major)
        {
            Some("image") => MediaKind::Image,
            Some("audio") => MediaKind::Audio,
            Some("video") => MediaKind::Video,
            _ => MediaKind::File,
        };
        e.content.media.push(Media {
            kind,
            source: MediaSource::Url(url.into()),
            content_type: content_type.map(str::to_owned),
            name: None,
        });
    }
    if e.content == Content::default() {
        return Err(Malformed);
    }
    single(e, Text("<Response/>".into()))
}

fn feishu(req: WebhookRequest<'_>, key: &Secret, token: &Secret, app: &str) -> Events {
    let body = super::sealed::feishu(req, key, token)?;
    if body["type"] == "url_verification" {
        return empty(Json(json!({"challenge": need(&body, "/challenge")?})));
    }
    auth_eq(field(&body, "/header/app_id"), app)?;
    let response = Json(json!({}));
    if need(&body, "/header/event_type")? != "im.message.receive_v1" {
        return empty(response);
    }
    if body
        .pointer("/event/sender/sender_type")
        .and_then(Value::as_str)
        == Some("app")
    {
        return empty(response);
    }
    let msg = body.pointer("/event/message").ok_or(Malformed)?;
    let mut e = event(
        ChannelKind::Feishu,
        need(&body, "/header/event_id")?,
        need(&body, "/event/sender/sender_id/open_id")?,
        need(msg, "/chat_id")?,
    );
    let msg_id = need(msg, "/message_id")?;
    e.address.reply_target = Some(msg_id.into());
    if field(msg, "/thread_id").is_some() {
        e.address.thread = Some(field(msg, "/root_id").unwrap_or(msg_id).into());
    }
    let content: Value = serde_json::from_str(need(msg, "/content")?).map_err(|_| Malformed)?;
    match need(msg, "/message_type")? {
        "text" => e.content.text = need(&content, "/text")?.into(),
        "image" | "file" | "audio" | "media" => {
            let (kind, pointer) = match msg["message_type"].as_str() {
                Some("image") => (MediaKind::Image, "/image_key"),
                Some("audio") => (MediaKind::Audio, "/file_key"),
                Some("media") => (MediaKind::Video, "/file_key"),
                _ => (MediaKind::File, "/file_key"),
            };
            let item = media(
                kind,
                MediaSource::ProviderId(need(&content, pointer)?.into()),
                &content,
            );
            if kind == MediaKind::File {
                e.content.attachments.push(item);
            } else {
                e.content.media.push(item);
            }
        }
        _ => return empty(response),
    }
    if let Some(mentions) = msg["mentions"].as_array() {
        e.content.mentions = mentions
            .iter()
            .filter_map(|v| {
                Some(Mention {
                    id: field(v, "/id/open_id")?.into(),
                    label: field(v, "/name").map(str::to_owned),
                })
            })
            .collect();
    }
    single(e, response)
}

fn dingtalk(req: WebhookRequest<'_>, key: &Secret, robot: &str) -> Events {
    let timestamp = req
        .header("timestamp")
        .filter(|s| {
            s.parse::<i64>()
                .is_ok_and(|ms| req.unix_seconds.saturating_mul(1000).abs_diff(ms) <= 3_600_000)
        })
        .ok_or(Unauthorized)?;
    let tag = req
        .header("sign")
        .and_then(|s| STANDARD.decode(s).ok())
        .ok_or(Unauthorized)?;
    let secret = key.nonempty().ok_or(Unauthorized)?;
    sha256_mac(
        secret.as_bytes(),
        format!("{timestamp}\n{secret}").as_bytes(),
    )
    .verify_slice(&tag)
    .map_err(|_| Unauthorized)?;
    let body = json_body(req)?;
    auth_eq(field(&body, "/robotCode"), robot)?;
    let from = need(&body, "/senderStaffId")?;
    let target = match need(&body, "/conversationType")? {
        "1" => format!("user:{from}"),
        "2" => format!("group:{}", need(&body, "/conversationId")?),
        _ => return Err(Malformed),
    };
    if need(&body, "/msgtype")? != "text" {
        return empty(Empty);
    }
    let mut e = event(ChannelKind::Dingtalk, need(&body, "/msgId")?, from, target);
    e.content.text = need(&body, "/text/content")?.into();
    single(e, Empty)
}

fn wx(
    req: WebhookRequest<'_>,
    token: &Secret,
    key: &Secret,
    receiver: &str,
    agent: Option<u64>,
) -> Events {
    use super::sealed::{WxVerified, wx as verify};
    if agent.is_none() && req.method == "GET" {
        return empty(Text(super::sealed::wechat_challenge(req, token)?));
    }
    let m = match verify(req, token, key, receiver)? {
        WxVerified::Challenge(value) => return empty(Text(value)),
        WxVerified::Message(message) => message,
    };
    let (channel, response) = if let Some(agent) = agent {
        if agent == 0 || m.agent_id != Some(agent) || m.to_user_name.as_deref() != Some(receiver) {
            return Err(Unauthorized);
        }
        (ChannelKind::Wecom, Empty)
    } else {
        (ChannelKind::WechatOfficial, Text("success".into()))
    };
    let kind = match m.msg_type.as_deref().ok_or(Malformed)? {
        "text" => None,
        "image" => Some(MediaKind::Image),
        "voice" => Some(MediaKind::Audio),
        "video" | "shortvideo" => Some(MediaKind::Video),
        "file" => Some(MediaKind::File),
        _ => return empty(response),
    };
    let from = m
        .from_user_name
        .filter(|s| !s.trim().is_empty())
        .ok_or(Malformed)?;
    let mut e = event(
        channel,
        m.msg_id.filter(|s| !s.trim().is_empty()).ok_or(Malformed)?,
        &from,
        &from,
    );
    if let Some(kind) = kind {
        let file = media(
            kind,
            MediaSource::ProviderId(
                m.media_id
                    .filter(|s| !s.trim().is_empty())
                    .ok_or(Malformed)?,
            ),
            &Value::Null,
        );
        if kind == MediaKind::File {
            e.content.attachments.push(file);
        } else {
            e.content.media.push(file);
        }
    } else {
        e.content.text = m
            .content
            .filter(|s| !s.trim().is_empty())
            .ok_or(Malformed)?;
    }
    single(e, response)
}

fn messenger(req: WebhookRequest<'_>, key: &Secret, verify_token: &Secret, page: &str) -> Events {
    if req.method == "GET" {
        auth_eq(req.param("hub.mode"), "subscribe")?;
        auth_eq(req.param("hub.verify_token"), verify_token.expose())?;
        return empty(Text(
            req.param("hub.challenge")
                .filter(|s| !s.is_empty())
                .ok_or(Malformed)?
                .into(),
        ));
    }
    let tag = req
        .header("X-Hub-Signature-256")
        .and_then(|s| s.strip_prefix("sha256="))
        .and_then(hex::<32>)
        .ok_or(Unauthorized)?;
    sha256_mac(key.nonempty().ok_or(Unauthorized)?.as_bytes(), req.body)
        .verify_slice(&tag)
        .map_err(|_| Unauthorized)?;
    let body = json_body(req)?;
    if body["object"] != "page" {
        return empty(Text("EVENT_RECEIVED".into()));
    }
    let mut events = Vec::new();
    for entry in body["entry"].as_array().ok_or(Malformed)? {
        if field(entry, "/id") != Some(page) {
            continue;
        }
        for m in entry["messaging"].as_array().ok_or(Malformed)? {
            if m.pointer("/message/is_echo") == Some(&Value::Bool(true)) {
                continue;
            }
            auth_eq(field(m, "/recipient/id"), page)?;
            let from = need(m, "/sender/id")?;
            let Some(mid) = field(m, "/message/mid").or_else(|| field(m, "/postback/mid")) else {
                continue;
            };
            let mut e = event(ChannelKind::Messenger, mid, from, from);
            e.action_id = field(m, "/postback/payload")
                .or_else(|| field(m, "/message/quick_reply/payload"))
                .map(str::to_owned);
            e.content.text = field(m, "/message/text").unwrap_or_default().into();
            if let Some(attachments) = m.pointer("/message/attachments").and_then(Value::as_array) {
                for a in attachments {
                    let kind = match field(a, "/type") {
                        Some("image") => MediaKind::Image,
                        Some("audio") => MediaKind::Audio,
                        Some("video") => MediaKind::Video,
                        Some("file") => MediaKind::File,
                        _ => continue,
                    };
                    if let Some(url) = field(a, "/payload/url") {
                        let item = media(kind, MediaSource::Url(url.into()), a);
                        if kind == MediaKind::File {
                            e.content.attachments.push(item);
                        } else {
                            e.content.media.push(item);
                        }
                    }
                }
            }
            if e.action_id.is_some() || e.content != Content::default() {
                events.push(e);
            }
        }
    }
    Ok(WebhookOutcome {
        events,
        response: Text("EVENT_RECEIVED".into()),
    })
}
