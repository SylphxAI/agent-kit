//! Request signing for relays and the primitives used by provider callbacks.

use super::{DeliveryError, IngressError, Secret, WebhookRequest};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;

pub(super) fn sha256_mac(key: &[u8], data: &[u8]) -> Hmac<Sha256> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    mac
}
pub(super) fn equal(a: &str, b: &str) -> bool {
    bool::from(a.as_bytes().ct_eq(b.as_bytes()))
}
pub(super) fn fresh(now: i64, raw: &str, skew: u64) -> bool {
    raw.parse::<i64>().is_ok_and(|ts| now.abs_diff(ts) <= skew)
}
pub(super) fn hex<const N: usize>(raw: &str) -> Option<[u8; N]> {
    if raw.len() != N * 2 || !raw.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut bytes = [0; N];
    for (i, b) in bytes.iter_mut().enumerate() {
        *b = u8::from_str_radix(&raw[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(bytes)
}

fn signing_key(key: &Secret) -> Option<Vec<u8>> {
    let key = key.nonempty()?;
    if let Some(encoded) = key.strip_prefix("whsec_") {
        STANDARD.decode(encoded).ok().filter(|v| !v.is_empty())
    } else {
        Some(key.as_bytes().to_vec())
    }
}
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 256
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}
fn relay_mac(key: &[u8], id: &str, timestamp: &str, body: &[u8]) -> Hmac<Sha256> {
    let mut mac = sha256_mac(key, id.as_bytes());
    mac.update(b".");
    mac.update(timestamp.as_bytes());
    mac.update(b".");
    mac.update(body);
    mac
}

/// Sign the exact encoded body using Standard Webhooks v1. Keys may be raw
/// text or prefixed with whsec_ (base64); malformed prefixed keys are refused.
pub fn sign_webhook(
    key: &Secret,
    id: &str,
    unix_seconds: i64,
    body: &[u8],
) -> Result<Vec<(&'static str, Secret)>, DeliveryError> {
    if !valid_id(id) {
        return Err(DeliveryError::InvalidMessage);
    }
    let key = signing_key(key).ok_or(DeliveryError::InvalidConfiguration)?;
    let timestamp = unix_seconds.to_string();
    let mac = relay_mac(&key, id, &timestamp, body)
        .finalize()
        .into_bytes();
    Ok(vec![
        ("webhook-id", Secret::new(id)),
        ("webhook-timestamp", Secret::new(timestamp)),
        (
            "webhook-signature",
            Secret::new(format!("v1,{}", STANDARD.encode(mac))),
        ),
    ])
}

/// Verifies the ID, timestamp (within 300 seconds), and raw body. The host
/// must still deduplicate accepted events per connection across that window.
pub fn verify_webhook<'a>(
    key: &Secret,
    request: WebhookRequest<'a>,
) -> Result<&'a str, IngressError> {
    let id = request
        .header("webhook-id")
        .filter(|v| valid_id(v))
        .ok_or(IngressError::Unauthorized)?;
    let timestamp = request
        .header("webhook-timestamp")
        .filter(|v| fresh(request.unix_seconds, v, 300))
        .ok_or(IngressError::Unauthorized)?;
    let signature = request
        .header("webhook-signature")
        .ok_or(IngressError::Unauthorized)?;
    let key = signing_key(key).ok_or(IngressError::Unauthorized)?;
    let mac = relay_mac(&key, id, timestamp, request.body);
    if signature
        .split_ascii_whitespace()
        .filter_map(|v| v.strip_prefix("v1,"))
        .filter_map(|v| STANDARD.decode(v).ok())
        .any(|v| mac.clone().verify_slice(&v).is_ok())
    {
        Ok(id)
    } else {
        Err(IngressError::Unauthorized)
    }
}
