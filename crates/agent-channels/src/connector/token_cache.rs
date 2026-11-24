use super::{ChannelKind, Secret};
use sha2::{Digest as _, Sha256};
use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

/// Opaque process-local cache key. Rotating credentials creates a new key.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct TokenKey([u8; 32]);
impl TokenKey {
    /// Include the exchange origin, account, scopes, and credential version
    /// (or credential itself) in distinct components.
    pub fn new(kind: ChannelKind, parts: &[&str]) -> Self {
        let mut hash = Sha256::new();
        hash.update([kind as u8]);
        for part in parts {
            hash.update(part.len().to_be_bytes());
            hash.update(part.as_bytes());
        }
        Self(hash.finalize().into())
    }
}

/// Bounded, process-local cache; does not perform token exchanges or hold a
/// lock across I/O. The host owns refresh coalescing and credential storage.
#[derive(Default)]
pub struct AccessTokenCache(Mutex<HashMap<TokenKey, (Secret, Instant)>>);
impl AccessTokenCache {
    pub fn get(&self, key: &TokenKey) -> Option<Secret> {
        let mut entries = self.0.lock().ok()?;
        if entries.get(key)?.1 <= Instant::now() {
            entries.remove(key);
            return None;
        }
        entries.get(key).map(|(token, _)| token.clone())
    }
    pub fn put(&self, key: TokenKey, token: Secret, expires_in: Duration) {
        let now = Instant::now();
        let usable = expires_in.saturating_sub(Duration::from_secs(30));
        if let Ok(mut entries) = self.0.lock() {
            entries.remove(&key);
            if token.nonempty().is_none() || usable.is_zero() {
                return;
            }
            let Some(expiry) = now.checked_add(usable) else {
                return;
            };
            entries.retain(|_, (_, end)| *end > now);
            if entries.len() >= 1024
                && let Some(oldest) = entries
                    .iter()
                    .min_by_key(|(_, (_, end))| *end)
                    .map(|(k, _)| *k)
            {
                entries.remove(&oldest);
            }
            entries.insert(key, (token, expiry));
        }
    }
    /// Evict only the rejected value; an older in-flight request cannot
    /// evict a token inserted by a newer refresh.
    pub fn invalidate(&self, key: &TokenKey, rejected: &Secret) {
        if let Ok(mut entries) = self.0.lock()
            && entries
                .get(key)
                .is_some_and(|(token, _)| super::signing::equal(token.expose(), rejected.expose()))
        {
            entries.remove(key);
        }
    }
}
