use super::*;
use DeliveryError::{
    InvalidConfiguration, InvalidMessage, Permanent, Retryable, UnknownOutcome, UnsupportedContent,
};
use serde_json::json;

pub(super) fn capabilities(kind: ChannelKind) -> ChannelCapabilities {
    use ChannelKind::*;
    match kind {
        Telegram => ChannelCapabilities {
            threads: true,
            replies: true,
            mentions: true,
            media: true,
            attachments: true,
            buttons: true,
            cards: false,
        },
        Slack | Discord => ChannelCapabilities {
            threads: true,
            replies: true,
            mentions: true,
            buttons: true,
            cards: true,
            ..Default::default()
        },
        Email => ChannelCapabilities {
            threads: true,
            replies: true,
            media: true,
            attachments: true,
            ..Default::default()
        },
        Webhook | Custom => ChannelCapabilities {
            threads: true,
            replies: true,
            mentions: true,
            media: true,
            attachments: true,
            buttons: true,
            cards: true,
        },
        Sms | Wecom | WechatOfficial | Messenger => ChannelCapabilities {
            media: true,
            ..Default::default()
        },
        Feishu => ChannelCapabilities {
            threads: true,
            replies: true,
            mentions: true,
            media: true,
            attachments: true,
            ..Default::default()
        },
        Dingtalk => ChannelCapabilities::default(),
    }
}
fn id(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= 256
        && v.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}
fn digits(v: &str) -> bool {
    !v.is_empty() && v.len() <= 24 && v.bytes().all(|b| b.is_ascii_digit())
}
fn dingtalk_destination(v: &str, group: bool) -> bool {
    id(v)
        || (group
            && v.starts_with("cid")
            && v.len() <= 256
            && v.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=')))
}
fn decimal_ts(v: &str) -> bool {
    v.split_once('.')
        .is_some_and(|(a, b)| digits(a) && digits(b))
}
fn url(v: &str) -> bool {
    v.starts_with("https://")
        && v.len() > 8
        && v.len() <= 2048
        && !v
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '<' | '>' | '"' | '\\'))
}
fn mailbox(v: &str) -> bool {
    v.len() <= 254
        && v.split_once('@').is_some_and(|(name, domain)| {
            !name.is_empty()
                && name.len() <= 64
                && !name.starts_with('.')
                && !name.ends_with('.')
                && !name.contains("..")
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b".!#$%&'*+-/=?^_{|}~".contains(&b))
                && domain.contains('.')
                && domain.split('.').all(|part| {
                    !part.is_empty()
                        && part.len() <= 63
                        && !part.starts_with('-')
                        && !part.ends_with('-')
                        && part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                })
        })
}
fn phone(v: &str) -> bool {
    v.strip_prefix('+')
        .is_some_and(|n| digits(n) && (2..=15).contains(&n.len()) && !n.starts_with('0'))
}
fn valid_text(text: &str, max: usize) -> bool {
    text.chars().count() <= max
        && !text
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\t' | '\n' | '\r'))
}
fn credential(s: &Secret) -> Result<&str, DeliveryError> {
    s.nonempty()
        .filter(|s| s.len() <= 8192 && s.bytes().all(|b| b.is_ascii_graphic()))
        .ok_or(InvalidConfiguration)
}
fn basic(account: &str, token: &Secret) -> Result<Secret, DeliveryError> {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    Ok(Secret::new(format!(
        "Basic {}",
        STANDARD.encode(format!("{account}:{}", credential(token)?))
    )))
}
fn auth_value(prefix: &str, token: &Secret) -> Result<Secret, DeliveryError> {
    Ok(Secret::new(format!("{prefix}{}", credential(token)?)))
}
fn escape_slack(v: &str) -> String {
    v.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
fn escape_xml(v: &str) -> String {
    quick_xml::escape::escape(v).into_owned()
}
fn button_action(button: &Button) -> Result<&ButtonAction, DeliveryError> {
    if button.label.trim().is_empty() || !valid_text(&button.label, 80) {
        return Err(InvalidMessage);
    }
    match &button.action {
        ButtonAction::OpenUrl(v) if url(v) => Ok(&button.action),
        ButtonAction::Callback(v) if !v.trim().is_empty() && valid_text(v, 256) => {
            Ok(&button.action)
        }
        _ => Err(InvalidMessage),
    }
}
fn route(a: &Address) -> (&str, Option<&str>, Option<&str>) {
    (
        &a.conversation,
        a.thread.as_deref(),
        a.reply_target.as_deref(),
    )
}
fn add_route(v: &mut Value, key: &str, val: Option<&str>) {
    if let Some(val) = val {
        v[key] = json!(val);
    }
}
fn validate(kind: ChannelKind, msg: &OutboundMessage) -> Result<(), DeliveryError> {
    let c = &msg.content;
    let a = &msg.address;
    let caps = capabilities(kind);
    if (a.thread.is_some() && !caps.threads)
        || (a.reply_target.is_some() && !caps.replies)
        || (!c.mentions.is_empty() && !caps.mentions)
        || (!c.media.is_empty() && !caps.media)
        || (!c.attachments.is_empty() && !caps.attachments)
        || (!c.buttons.is_empty() && !caps.buttons)
        || (!c.cards.is_empty() && !caps.cards)
    {
        return Err(UnsupportedContent);
    }
    let invalid_id =
        |v: &str| v.len() > 512 || v.trim().is_empty() || v.chars().any(char::is_control);
    if invalid_id(&a.conversation)
        || a.thread.as_deref().is_some_and(invalid_id)
        || a.reply_target.as_deref().is_some_and(invalid_id)
        || !valid_text(&c.text, 40_000)
        || c.media.len() + c.attachments.len() > 10
        || c.mentions.len() > 20
        || c.cards.len() > 4
        || c.buttons.len() > 20
        || (c.text.trim().is_empty()
            && c.media.is_empty()
            && c.attachments.is_empty()
            && c.buttons.is_empty()
            && c.cards.is_empty())
    {
        return Err(InvalidMessage);
    }
    for item in c.media.iter().chain(&c.attachments) {
        if match &item.source {
            MediaSource::Url(v) => !url(v),
            MediaSource::ProviderId(v) => invalid_id(v),
        } || item.name.as_ref().is_some_and(|v| !valid_text(v, 255))
            || item
                .content_type
                .as_ref()
                .is_some_and(|v| !valid_text(v, 128))
        {
            return Err(InvalidMessage);
        }
    }
    for m in &c.mentions {
        if invalid_id(&m.id) || m.label.as_ref().is_some_and(|v| !valid_text(v, 80)) {
            return Err(InvalidMessage);
        }
    }
    for card in &c.cards {
        if card.title.trim().is_empty()
            || !valid_text(&card.title, 150)
            || !valid_text(&card.text, 2000)
            || card.buttons.len() > 5
        {
            return Err(InvalidMessage);
        }
    }
    for btn in c
        .buttons
        .iter()
        .chain(c.cards.iter().flat_map(|card| &card.buttons))
    {
        button_action(btn)?;
    }
    Ok(())
}

pub(super) fn prepare(
    auth: &OutboundAuth,
    msg: &OutboundMessage,
    ctx: DeliveryContext<'_>,
) -> Result<DeliveryRequest, DeliveryError> {
    let kind = auth.kind();
    validate(kind, msg)?;
    let c = &msg.content;
    let (to, thread, reply) = route(&msg.address);
    let mut request = DeliveryRequest {
        channel: kind,
        path: String::new(),
        headers: Vec::new(),
        query: Vec::new(),
        content_type: "application/json",
        body: Vec::new(),
    };
    let body = match auth {
        OutboundAuth::Telegram { bot_token } => {
            let token = credential(bot_token)?;
            if !token
                .split_once(':')
                .is_some_and(|(a, b)| digits(a) && id(b))
            {
                return Err(InvalidConfiguration);
            }
            let (method, body) = telegram(msg)?;
            request.path = format!("/bot{token}/{method}");
            body
        }
        OutboundAuth::Slack { bot_token } => {
            request.path = "/chat.postMessage".into();
            request
                .headers
                .push(("authorization", auth_value("Bearer ", bot_token)?));
            slack(msg)?
        }
        OutboundAuth::Discord { bot_token } => {
            let channel = thread.unwrap_or(to);
            if !digits(channel) || !digits(to) || reply.is_some_and(|v| !digits(v)) {
                return Err(InvalidMessage);
            }
            request.path = format!("/channels/{channel}/messages");
            request
                .headers
                .push(("authorization", auth_value("Bot ", bot_token)?));
            discord(msg)?
        }
        OutboundAuth::Email { from, signing_key } => {
            if !mailbox(from) {
                return Err(InvalidConfiguration);
            }
            if !mailbox(to) {
                return Err(InvalidMessage);
            }
            let mut body = json!({"from": from, "to": to, "text": c.text, "media": c.media, "attachments": c.attachments});
            add_route(&mut body, "thread", thread);
            add_route(&mut body, "in_reply_to", reply);
            request.body = encode(&body)?;
            request.headers = signing::sign_webhook(
                signing_key,
                ctx.delivery_id,
                ctx.unix_seconds,
                &request.body,
            )?;
            return Ok(request);
        }
        OutboundAuth::Sms {
            account_sid,
            auth_token,
            from,
        } => {
            if !id(account_sid) || !phone(from) {
                return Err(InvalidConfiguration);
            }
            if !phone(to) || !valid_text(&c.text, 1600) {
                return Err(InvalidMessage);
            }
            request.path = format!("/2010-04-01/Accounts/{account_sid}/Messages.json");
            request
                .headers
                .push(("authorization", basic(account_sid, auth_token)?));
            request.content_type = "application/x-www-form-urlencoded";
            let mut form = form_urlencoded::Serializer::new(String::new());
            form.append_pair("To", to)
                .append_pair("From", from)
                .append_pair("Body", &c.text);
            for m in &c.media {
                let MediaSource::Url(src) = &m.source else {
                    return Err(UnsupportedContent);
                };
                form.append_pair("MediaUrl", src);
            }
            request.body = form.finish().into_bytes();
            return Ok(request);
        }
        OutboundAuth::Webhook { signing_key } | OutboundAuth::Custom { signing_key } => {
            request.body = encode(&json!(msg))?;
            request.headers = signing::sign_webhook(
                signing_key,
                ctx.delivery_id,
                ctx.unix_seconds,
                &request.body,
            )?;
            return Ok(request);
        }
        OutboundAuth::Feishu { access_token } => {
            request
                .headers
                .push(("authorization", auth_value("Bearer ", access_token)?));
            let body = feishu(msg)?;
            if let Some(root) = thread.or(reply) {
                if !id(root) {
                    return Err(InvalidMessage);
                }
                request.path = format!("/open-apis/im/v1/messages/{root}/reply");
            } else {
                if !id(to) {
                    return Err(InvalidMessage);
                }
                request.path = "/open-apis/im/v1/messages".into();
                request
                    .query
                    .push(("receive_id_type", Secret::new("chat_id")));
            }
            body
        }
        OutboundAuth::Dingtalk {
            access_token,
            robot_code,
            group,
        } => {
            if !id(robot_code) {
                return Err(InvalidConfiguration);
            }
            let (scope, dest) = to.split_once(':').ok_or(InvalidMessage)?;
            if scope != if *group { "group" } else { "user" }
                || !dingtalk_destination(dest, *group)
                || !valid_text(&c.text, 4000)
            {
                return Err(InvalidMessage);
            }
            request
                .headers
                .push(("x-acs-dingtalk-access-token", auth_value("", access_token)?));
            let param = json!({"content": c.text}).to_string();
            let mut body =
                json!({"robotCode": robot_code, "msgKey": "sampleText", "msgParam": param});
            if *group {
                request.path = "/v1.0/robot/groupMessages/send".into();
                body["openConversationId"] = json!(dest);
            } else {
                request.path = "/v1.0/robot/oToMessages/batchSend".into();
                body["userIds"] = json!([dest]);
            }
            body
        }
        OutboundAuth::Wecom {
            access_token,
            agent_id,
        } => {
            if *agent_id == 0 {
                return Err(InvalidConfiguration);
            }
            if !id(to) {
                return Err(InvalidMessage);
            }
            request.path = "/cgi-bin/message/send".into();
            request
                .query
                .push(("access_token", auth_value("", access_token)?));
            let mut body = wx_content(c, false)?;
            body["touser"] = json!(to);
            body["agentid"] = json!(agent_id);
            body
        }
        OutboundAuth::WechatOfficial { access_token } => {
            if !id(to) {
                return Err(InvalidMessage);
            }
            request.path = "/cgi-bin/message/custom/send".into();
            request
                .query
                .push(("access_token", auth_value("", access_token)?));
            let mut body = wx_content(c, true)?;
            body["touser"] = json!(to);
            body
        }
        OutboundAuth::Messenger {
            page_access_token,
            page_id,
        } => {
            if !digits(page_id) {
                return Err(InvalidConfiguration);
            }
            if !digits(to) {
                return Err(InvalidMessage);
            }
            request.path = format!("/{page_id}/messages");
            request
                .headers
                .push(("authorization", auth_value("Bearer ", page_access_token)?));
            let message = if let Some(m) = only_media(c)? {
                let MediaSource::Url(v) = &m.source else {
                    return Err(UnsupportedContent);
                };
                let kind = match m.kind {
                    MediaKind::Image => "image",
                    MediaKind::Audio => "audio",
                    MediaKind::Video => "video",
                    MediaKind::File => "file",
                };
                json!({"attachment": {"type": kind, "payload": {"url": v}}})
            } else {
                if !valid_text(&c.text, 2000) {
                    return Err(InvalidMessage);
                }
                json!({"text": c.text})
            };
            json!({"recipient": {"id": to}, "messaging_type": "RESPONSE", "message": message})
        }
    };
    request.body = encode(&body)?;
    Ok(request)
}
fn encode(v: &Value) -> Result<Vec<u8>, DeliveryError> {
    let data = serde_json::to_vec(v).map_err(|_| InvalidMessage)?;
    if data.len() > MAX_BODY_BYTES {
        return Err(InvalidMessage);
    }
    Ok(data)
}
fn only_media(c: &Content) -> Result<Option<&Media>, DeliveryError> {
    let mut items = c.media.iter().chain(&c.attachments);
    let first = items.next();
    if first.is_some() && (!c.text.is_empty() || !c.mentions.is_empty() || items.next().is_some()) {
        return Err(UnsupportedContent);
    }
    Ok(first)
}
fn telegram(msg: &OutboundMessage) -> Result<(&'static str, Value), DeliveryError> {
    let (chat, thread, reply) = route(&msg.address);
    let chat = chat
        .parse::<i64>()
        .ok()
        .filter(|n| *n != 0)
        .ok_or(InvalidMessage)?;
    let positive = |s: &str| {
        s.parse::<i64>()
            .ok()
            .filter(|n| *n > 0)
            .ok_or(InvalidMessage)
    };
    let thread = thread.map(positive).transpose()?;
    let reply = reply.map(positive).transpose()?;
    let c = &msg.content;
    // No parse_mode: markup and special characters are literal. Structured
    // text mentions use UTF-16 offsets, not user-supplied markup.
    let mut text = c.text.clone();
    let mut entities = Vec::new();
    for m in &c.mentions {
        let user = positive(&m.id)?;
        let label = m.label.as_deref().unwrap_or(&m.id);
        if label.trim().is_empty() {
            return Err(InvalidMessage);
        }
        if !text.is_empty() {
            text.push(' ');
        }
        let start = text.encode_utf16().count();
        text.push('@');
        text.push_str(label);
        entities.push(json!({"type": "text_mention", "offset": start, "length": text.encode_utf16().count()-start, "user": {"id": user}}));
    }
    let mut body = json!({"chat_id": chat});
    crate::reply_target::apply_telegram_message_thread_id(&mut body, thread);
    if let Some(reply) = reply {
        body["reply_parameters"] =
            json!({"message_id": reply, "allow_sending_without_reply": false});
    }
    let mut items = c.media.iter().chain(&c.attachments);
    let method = if let Some(item) = items.next() {
        if items.next().is_some() {
            return Err(UnsupportedContent);
        }
        if text.encode_utf16().count() > 1024 {
            return Err(InvalidMessage);
        }
        let (method, key) = match item.kind {
            MediaKind::Image => ("sendPhoto", "photo"),
            MediaKind::Audio => ("sendAudio", "audio"),
            MediaKind::Video => ("sendVideo", "video"),
            MediaKind::File => ("sendDocument", "document"),
        };
        body[key] = json!(match &item.source {
            MediaSource::Url(v) | MediaSource::ProviderId(v) => v,
        });
        if !text.is_empty() {
            body["caption"] = json!(text);
        }
        if !entities.is_empty() {
            body["caption_entities"] = json!(entities);
        }
        method
    } else {
        if text.trim().is_empty() || text.encode_utf16().count() > 4096 {
            return Err(InvalidMessage);
        }
        body["text"] = json!(text);
        if !entities.is_empty() {
            body["entities"] = json!(entities);
        }
        "sendMessage"
    };
    if !c.buttons.is_empty() {
        let buttons = c
            .buttons
            .iter()
            .map(|b| {
                Ok(vec![match button_action(b)? {
                    ButtonAction::OpenUrl(v) => json!({"text": b.label, "url": v}),
                    ButtonAction::Callback(v) if v.len() <= 64 => {
                        json!({"text": b.label, "callback_data": v})
                    }
                    _ => return Err(InvalidMessage),
                }])
            })
            .collect::<Result<Vec<_>, _>>()?;
        body["reply_markup"] = json!({"inline_keyboard": buttons});
    }
    Ok((method, body))
}

fn slack(msg: &OutboundMessage) -> Result<Value, DeliveryError> {
    let (channel, thread, reply) = route(&msg.address);
    if !id(channel)
        || thread.is_some_and(|v| !decimal_ts(v))
        || reply.is_some_and(|v| !decimal_ts(v))
    {
        return Err(InvalidMessage);
    }
    let c = &msg.content;
    let mut text = escape_slack(&c.text);
    for m in &c.mentions {
        if !id(&m.id) {
            return Err(InvalidMessage);
        }
        text.push_str(&format!(" <@{}>", m.id));
    }
    if !valid_text(&text, 40_000) {
        return Err(InvalidMessage);
    }
    let mut body = json!({"channel": channel, "text": text, "mrkdwn": !c.mentions.is_empty(), "unfurl_links": false, "unfurl_media": false, "link_names": false});
    add_route(&mut body, "thread_ts", thread.or(reply));
    let mut blocks = Vec::new();
    let buttons = |buttons: &[Button]| -> Result<Value, DeliveryError> {
        let values = buttons.iter().enumerate().map(|(i,b)| {
            Ok(match button_action(b)? {
                ButtonAction::OpenUrl(v) => json!({"type": "button", "text": {"type": "plain_text", "text": b.label}, "url": v, "action_id": format!("url_{i}")}),
                ButtonAction::Callback(v) => json!({"type": "button", "text": {"type": "plain_text", "text": b.label}, "value": v, "action_id": format!("callback_{i}")}),
            })
        }).collect::<Result<Vec<_>, _>>()?;
        Ok(json!({"type": "actions", "elements": values}))
    };
    if !c.buttons.is_empty() || !c.cards.is_empty() {
        if !text.is_empty() {
            if !valid_text(&text, 3000) {
                return Err(InvalidMessage);
            }
            blocks.push(json!({"type": "section", "text": {"type": if c.mentions.is_empty() {"plain_text"} else {"mrkdwn"}, "text": text, "verbatim": true}}));
            if c.mentions.is_empty() {
                blocks[0]["text"]
                    .as_object_mut()
                    .expect("constructed block")
                    .remove("verbatim");
                blocks[0]["text"]["text"] = json!(c.text);
            }
        } else {
            body["text"] = json!(
                c.cards
                    .first()
                    .map(|v| v.title.as_str())
                    .unwrap_or("Actions")
            );
        }
        for card in &c.cards {
            blocks.push(
                json!({"type": "header", "text": {"type": "plain_text", "text": card.title}}),
            );
            if !card.text.is_empty() {
                blocks
                    .push(json!({"type":"section","text":{"type":"plain_text","text": card.text}}));
            }
            if !card.buttons.is_empty() {
                blocks.push(buttons(&card.buttons)?);
            }
        }
        if !c.buttons.is_empty() {
            blocks.push(buttons(&c.buttons)?);
        }
        body["blocks"] = json!(blocks);
    }
    Ok(body)
}

fn discord(msg: &OutboundMessage) -> Result<Value, DeliveryError> {
    let c = &msg.content;
    let mut text = c.text.clone();
    let mut allowed = Vec::new();
    for m in &c.mentions {
        if !digits(&m.id) {
            return Err(InvalidMessage);
        }
        text.push_str(&format!(" <@{}>", m.id));
        allowed.push(&m.id);
    }
    if !valid_text(&text, 2000) {
        return Err(InvalidMessage);
    }
    let mut body = json!({"content": text, "allowed_mentions": {"parse": [], "replied_user": false, "users": allowed}});
    if let Some(reply) = &msg.address.reply_target {
        body["message_reference"] = json!({"message_id": reply, "fail_if_not_exists": true});
    }
    if !c.cards.is_empty() {
        body["embeds"] = json!(
            c.cards
                .iter()
                .map(|card| json!({"title": card.title, "description": card.text}))
                .collect::<Vec<_>>()
        );
    }
    let buttons = c
        .buttons
        .iter()
        .chain(c.cards.iter().flat_map(|card| &card.buttons))
        .collect::<Vec<_>>();
    if buttons.len() > 25
        || c.cards
            .iter()
            .map(|v| v.title.chars().count() + v.text.chars().count())
            .sum::<usize>()
            > 6000
    {
        return Err(InvalidMessage);
    }
    if !buttons.is_empty() {
        let rows = buttons
            .chunks(5)
            .map(|chunk| {
                let components = chunk
                    .iter()
                    .map(|b| {
                        Ok(match button_action(b)? {
                            ButtonAction::OpenUrl(v) => {
                                json!({"type": 2, "style": 5, "url": v, "label": b.label})
                            }
                            ButtonAction::Callback(v) if v.chars().count() <= 100 => {
                                json!({"type": 2, "style": 1, "custom_id": v, "label": b.label})
                            }
                            _ => return Err(InvalidMessage),
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(json!({"type": 1, "components": components}))
            })
            .collect::<Result<Vec<_>, _>>()?;
        body["components"] = json!(rows);
    }
    Ok(body)
}

fn feishu(msg: &OutboundMessage) -> Result<Value, DeliveryError> {
    let c = &msg.content;
    let (kind, content) = if let Some(m) = only_media(c)? {
        let MediaSource::ProviderId(v) = &m.source else {
            return Err(UnsupportedContent);
        };
        match m.kind {
            MediaKind::Image => ("image", json!({"image_key": v})),
            MediaKind::File => ("file", json!({"file_key": v})),
            MediaKind::Audio => ("audio", json!({"file_key": v})),
            MediaKind::Video => return Err(UnsupportedContent),
        }
    } else {
        let mut text = escape_xml(&c.text);
        for m in &c.mentions {
            if !id(&m.id) || m.id == "all" {
                return Err(InvalidMessage);
            }
            text.push_str(&format!(" <at user_id=\"{}\"></at>", m.id));
        }
        if text.len() > 20_000 {
            return Err(InvalidMessage);
        }
        ("text", json!({"text": text}))
    };
    let mut body = json!({"msg_type": kind, "content": content.to_string()});
    if msg.address.thread.is_some() {
        body["reply_in_thread"] = json!(true);
    } else if msg.address.reply_target.is_none() {
        body["receive_id"] = json!(msg.address.conversation);
    }
    Ok(body)
}

fn wx_content(c: &Content, official: bool) -> Result<Value, DeliveryError> {
    if let Some(m) = only_media(c)? {
        let MediaSource::ProviderId(v) = &m.source else {
            return Err(UnsupportedContent);
        };
        let kind = match (m.kind, official) {
            (MediaKind::Image, _) => "image",
            (MediaKind::Audio, _) => "voice",
            (MediaKind::Video, false) => "video",
            (MediaKind::File, false) => "file",
            _ => return Err(UnsupportedContent),
        };
        return Ok(json!({"msgtype": kind, kind: {"media_id": v}}));
    }
    if c.text.len() > 2048 {
        return Err(InvalidMessage);
    }
    Ok(json!({"msgtype": "text", "text": {"content": c.text}}))
}

pub(super) fn check_response(kind: ChannelKind, r: &DeliveryResponse) -> Result<(), DeliveryError> {
    let retry_after = r.retry_after;
    let retry = || Err(Retryable { retry_after });
    if r.status == 429 {
        return retry();
    }
    if (400..500).contains(&r.status) && !matches!(r.status, 408 | 409 | 425) {
        return Err(Permanent);
    }
    if !(200..300).contains(&r.status) {
        return Err(UnknownOutcome);
    }
    if matches!(
        kind,
        ChannelKind::Email | ChannelKind::Webhook | ChannelKind::Custom
    ) {
        return Ok(());
    }
    if r.body.len() > MAX_BODY_BYTES {
        return Err(UnknownOutcome);
    }
    let v: Value = serde_json::from_slice(&r.body).map_err(|_| UnknownOutcome)?;
    match kind {
        ChannelKind::Telegram => match v["ok"].as_bool() {
            Some(true) if v["result"]["message_id"].as_i64().is_some_and(|n| n > 0) => Ok(()),
            Some(false) => match v["error_code"].as_u64() {
                Some(429) => Err(Retryable {
                    retry_after: retry_after.or_else(|| {
                        v["parameters"]["retry_after"]
                            .as_u64()
                            .map(Duration::from_secs)
                    }),
                }),
                Some(400 | 401 | 403 | 404) => Err(Permanent),
                _ => Err(UnknownOutcome),
            },
            _ => Err(UnknownOutcome),
        },
        ChannelKind::Slack => match v["ok"].as_bool() {
            Some(true) if v["ts"].as_str().is_some_and(decimal_ts) => Ok(()),
            Some(false) => match v["error"].as_str() {
                Some("ratelimited" | "rate_limited") => retry(),
                Some(
                    "invalid_auth" | "token_revoked" | "channel_not_found" | "not_in_channel"
                    | "is_archived" | "msg_too_long" | "invalid_blocks" | "missing_scope"
                    | "no_text",
                ) => Err(Permanent),
                _ => Err(UnknownOutcome),
            },
            _ => Err(UnknownOutcome),
        },
        ChannelKind::Discord => {
            if v["id"].as_str().is_some_and(digits) {
                Ok(())
            } else {
                Err(UnknownOutcome)
            }
        }
        ChannelKind::Messenger => {
            if v["message_id"].as_str().is_some_and(|s| !s.is_empty()) {
                Ok(())
            } else {
                Err(UnknownOutcome)
            }
        }
        ChannelKind::Sms => {
            if v["sid"].as_str().is_some_and(id)
                && matches!(
                    v["status"].as_str(),
                    Some("accepted" | "queued" | "sending" | "sent" | "delivered" | "scheduled")
                )
            {
                Ok(())
            } else {
                Err(UnknownOutcome)
            }
        }
        ChannelKind::Feishu => match v["code"].as_i64() {
            Some(0) if v["data"]["message_id"].as_str().is_some_and(id) => Ok(()),
            Some(99991400 | 230020) => retry(),
            Some(230001 | 230002 | 230006 | 230013 | 230027 | 99991663 | 99991668) => {
                Err(Permanent)
            }
            _ => Err(UnknownOutcome),
        },
        ChannelKind::Dingtalk => {
            let has_rejections = |field: &str| v[field].as_array().is_some_and(|a| !a.is_empty());
            if has_rejections("invalidStaffIdList") || has_rejections("filteredStaffIdList") {
                return Err(Permanent);
            }
            if has_rejections("flowControlledStaffIdList") {
                return retry();
            }
            if v["processQueryKey"].as_str().is_some_and(|s| !s.is_empty()) {
                return Ok(());
            }
            match v["code"].as_str() {
                Some("throttling") => retry(),
                Some("InvalidParameter" | "Unauthorized") => Err(Permanent),
                _ => Err(UnknownOutcome),
            }
        }
        ChannelKind::Wecom | ChannelKind::WechatOfficial => match v["errcode"].as_i64() {
            Some(0)
                if ["invaliduser", "unlicenseduser"]
                    .iter()
                    .any(|k| v[*k].as_str().is_some_and(|s| !s.is_empty())) =>
            {
                Err(Permanent)
            }
            Some(0) => Ok(()),
            Some(-1 | 45009 | 45011 | 45033) => retry(),
            Some(
                40001 | 40003 | 40007 | 40013 | 40014 | 41001 | 42001 | 43004 | 44004 | 45002
                | 45015 | 45047 | 48001 | 48002 | 60020 | 60111 | 81013,
            ) => Err(Permanent),
            _ => Err(UnknownOutcome),
        },
        ChannelKind::Email | ChannelKind::Webhook | ChannelKind::Custom => unreachable!(),
    }
}
