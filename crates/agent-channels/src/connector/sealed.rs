use super::{
    IngressError, Secret, WebhookRequest,
    signing::{equal, fresh},
};
use aes::{
    Aes256,
    cipher::{
        BlockDecryptMut, KeyIvInit,
        block_padding::{NoPadding, Pkcs7},
    },
};
use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD, STANDARD_NO_PAD},
};
use serde::Deserialize;
use serde_json::Value;
use sha1::Sha1;
use sha2::{Digest as _, Sha256};

type AesCbc = cbc::Decryptor<Aes256>;
use IngressError::{Malformed, Unauthorized};

pub(super) fn feishu(
    request: WebhookRequest<'_>,
    key: &Secret,
    token: &Secret,
) -> Result<Value, IngressError> {
    let key = key.nonempty().ok_or(Unauthorized)?;
    let token = token.nonempty().ok_or(Unauthorized)?;
    let signature_present = request
        .headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("X-Lark-Signature"));
    if signature_present {
        let provided = request.header("X-Lark-Signature").ok_or(Unauthorized)?;
        let ts = request
            .header("X-Lark-Request-Timestamp")
            .filter(|ts| fresh(request.unix_seconds, ts, 300))
            .ok_or(Unauthorized)?;
        let nonce = request
            .header("X-Lark-Request-Nonce")
            .filter(|n| !n.is_empty())
            .ok_or(Unauthorized)?;
        let mut hash = Sha256::new();
        for part in [
            ts.as_bytes(),
            nonce.as_bytes(),
            key.as_bytes(),
            request.body,
        ] {
            hash.update(part);
        }
        if !equal(&format!("{:x}", hash.finalize()), provided) {
            return Err(Unauthorized);
        }
    }
    let envelope: Value = serde_json::from_slice(request.body).map_err(|_| Malformed)?;
    let enc = envelope
        .get("encrypt")
        .and_then(Value::as_str)
        .ok_or(Unauthorized)?;
    let raw = STANDARD.decode(enc).map_err(|_| Unauthorized)?;
    if raw.len() < 32 {
        return Err(Unauthorized);
    }
    let (iv, data) = raw.split_at(16);
    let key = Sha256::digest(key.as_bytes());
    let plain = AesCbc::new_from_slices(&key, iv)
        .map_err(|_| Unauthorized)?
        .decrypt_padded_vec_mut::<Pkcs7>(data)
        .map_err(|_| Unauthorized)?;
    let body: Value = serde_json::from_slice(&plain).map_err(|_| Unauthorized)?;
    let challenge = body.get("type").and_then(Value::as_str) == Some("url_verification");
    let field = if challenge { "/token" } else { "/header/token" };
    if (!signature_present && !challenge)
        || !body
            .pointer(field)
            .and_then(Value::as_str)
            .is_some_and(|v| equal(token, v))
    {
        return Err(Unauthorized);
    }
    Ok(body)
}

#[derive(Deserialize)]
#[serde(rename = "xml", rename_all = "PascalCase")]
pub(super) struct WxXml {
    pub encrypt: Option<String>,
    pub to_user_name: Option<String>,
    pub from_user_name: Option<String>,
    #[serde(rename = "MsgId")]
    pub msg_id: Option<String>,
    pub msg_type: Option<String>,
    #[serde(rename = "AgentID")]
    pub agent_id: Option<u64>,
    pub content: Option<String>,
    pub media_id: Option<String>,
}

fn xml(raw: &[u8]) -> Result<WxXml, IngressError> {
    // No DTD, entity resolver, or external resources. Require the actual root.
    use quick_xml::{Reader, events::Event};
    let mut reader = Reader::from_reader(raw);
    reader.config_mut().trim_text(true);
    loop {
        match reader.read_event() {
            Ok(Event::Decl(_) | Event::Comment(_)) => (),
            Ok(Event::Start(root)) if root.name().as_ref() == b"xml" => break,
            _ => return Err(Malformed),
        }
    }
    quick_xml::de::from_reader(raw).map_err(|_| Malformed)
}

pub(super) enum WxVerified {
    Challenge(String),
    Message(WxXml),
}

pub(super) fn wx(
    request: WebhookRequest<'_>,
    token: &Secret,
    key: &Secret,
    receiver: &str,
) -> Result<WxVerified, IngressError> {
    let token = token.nonempty().ok_or(Unauthorized)?;
    let key = key.nonempty().ok_or(Unauthorized)?;
    if receiver.is_empty() || key.len() != 43 {
        return Err(Unauthorized);
    }
    let stamp = request
        .param("timestamp")
        .filter(|s| fresh(request.unix_seconds, s, 300))
        .ok_or(Unauthorized)?;
    let nonce = request
        .param("nonce")
        .filter(|n| !n.is_empty())
        .ok_or(Unauthorized)?;
    let provided = request.param("msg_signature").ok_or(Unauthorized)?;
    let challenge = request.method == "GET";
    let envelope;
    let enc = if challenge {
        request.param("echostr").ok_or(Unauthorized)?
    } else {
        envelope = xml(request.body)?;
        envelope.encrypt.as_deref().ok_or(Unauthorized)?
    };
    let mut parts = [token, stamp, nonce, enc];
    parts.sort_unstable();
    let mut hash = Sha1::new();
    for part in parts {
        hash.update(part.as_bytes());
    }
    if !equal(&format!("{:x}", hash.finalize()), provided) {
        return Err(Unauthorized);
    }
    let key = STANDARD_NO_PAD.decode(key).map_err(|_| Unauthorized)?;
    let iv = key.get(..16).ok_or(Unauthorized)?;
    let raw = STANDARD.decode(enc).map_err(|_| Unauthorized)?;
    let mut data = AesCbc::new_from_slices(&key, iv)
        .map_err(|_| Unauthorized)?
        .decrypt_padded_vec_mut::<NoPadding>(&raw)
        .map_err(|_| Unauthorized)?;
    let padding = usize::from(*data.last().ok_or(Unauthorized)?);
    if data.len() % 32 != 0
        || !(1..=32).contains(&padding)
        || padding > data.len()
        || !data[data.len() - padding..]
            .iter()
            .all(|&b| usize::from(b) == padding)
    {
        return Err(Unauthorized);
    }
    data.truncate(data.len() - padding);
    let count = u32::from_be_bytes(
        data.get(16..20)
            .ok_or(Unauthorized)?
            .try_into()
            .map_err(|_| Unauthorized)?,
    ) as usize;
    let end = 20usize.checked_add(count).ok_or(Unauthorized)?;
    let message = data.get(20..end).ok_or(Unauthorized)?;
    if data.get(end..) != Some(receiver.as_bytes()) {
        return Err(Unauthorized);
    }
    if challenge {
        Ok(WxVerified::Challenge(
            String::from_utf8(message.to_vec()).map_err(|_| Malformed)?,
        ))
    } else {
        Ok(WxVerified::Message(xml(message)?))
    }
}

/// The Official Account setup handshake does not authenticate any POST body.
pub(super) fn wechat_challenge(
    req: WebhookRequest<'_>,
    token: &Secret,
) -> Result<String, IngressError> {
    let token = token.nonempty().ok_or(Unauthorized)?;
    let stamp = req
        .param("timestamp")
        .filter(|s| fresh(req.unix_seconds, s, 300))
        .ok_or(Unauthorized)?;
    let nonce = req
        .param("nonce")
        .filter(|s| !s.is_empty())
        .ok_or(Unauthorized)?;
    let mut parts = [token, stamp, nonce];
    parts.sort_unstable();
    let mut hash = Sha1::new();
    for part in parts {
        hash.update(part.as_bytes());
    }
    if !req
        .param("signature")
        .is_some_and(|v| equal(&format!("{:x}", hash.finalize()), v))
    {
        return Err(Unauthorized);
    }
    Ok(req
        .param("echostr")
        .filter(|s| !s.is_empty())
        .ok_or(Malformed)?
        .into())
}
