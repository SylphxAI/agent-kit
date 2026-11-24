//! Transport-independent channel connectors. Only explicit delivery uses I/O.

mod discord_gateway;
pub use discord_gateway::normalize_discord_gateway_message;
mod inbound;
mod outbound;
mod sealed;
pub mod signing;
pub mod telegram;
mod token_cache;
pub use token_cache::{AccessTokenCache, TokenKey};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{fmt, time::Duration};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelKind {
    Telegram,
    Slack,
    Discord,
    Email,
    Sms,
    Webhook,
    Feishu,
    Dingtalk,
    Wecom,
    WechatOfficial,
    Messenger,
    Custom,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChannelCapabilities {
    pub threads: bool,
    pub replies: bool,
    pub mentions: bool,
    pub media: bool,
    pub attachments: bool,
    pub buttons: bool,
    pub cards: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Address {
    pub conversation: String,
    pub thread: Option<String>,
    pub reply_target: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    Image,
    Audio,
    Video,
    File,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum MediaSource {
    Url(String),
    ProviderId(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Media {
    pub kind: MediaKind,
    pub source: MediaSource,
    pub name: Option<String>,
    pub content_type: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mention {
    pub id: String,
    pub label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum ButtonAction {
    OpenUrl(String),
    Callback(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Button {
    pub label: String,
    pub action: ButtonAction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Card {
    pub title: String,
    pub text: String,
    #[serde(default)]
    pub buttons: Vec<Button>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Content {
    pub text: String,
    pub media: Vec<Media>,
    pub attachments: Vec<Media>,
    pub mentions: Vec<Mention>,
    pub buttons: Vec<Button>,
    pub cards: Vec<Card>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InboundEvent {
    pub channel: ChannelKind,
    pub event_id: String,
    pub sender_id: String,
    pub address: Address,
    pub content: Content,
    /// Explicit user interaction; never treated as a delivery effect.
    pub action_id: Option<String>,
    /// Context alone must not trigger an automatic answer.
    pub context_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutboundMessage {
    pub address: Address,
    pub content: Content,
}

#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);
impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
    fn nonempty(&self) -> Option<&str> {
        (!self.0.trim().is_empty()).then_some(&self.0)
    }
}
impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

/// Bound reads before allocating the body. Pass original request bytes and a
/// trusted local clock, not a timestamp chosen by the remote sender.
#[derive(Clone, Copy)]
pub struct WebhookRequest<'a> {
    pub method: &'a str,
    pub headers: &'a [(&'a str, &'a str)],
    pub query: &'a [(&'a str, &'a str)],
    pub body: &'a [u8],
    pub unix_seconds: i64,
}
impl<'a> WebhookRequest<'a> {
    fn header(&self, name: &str) -> Option<&'a str> {
        unique(self.headers, |k| k.eq_ignore_ascii_case(name))
    }
    fn param(&self, name: &str) -> Option<&'a str> {
        unique(self.query, |k| k == name)
    }
}
fn unique<'a>(pairs: &[(&'a str, &'a str)], matches: impl Fn(&str) -> bool) -> Option<&'a str> {
    let mut values = pairs.iter().filter(|(key, _)| matches(key));
    let value = values.next()?.1;
    values.next().is_none().then_some(value)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngressError {
    Unauthorized,
    Malformed,
    TooLarge,
    UnsupportedMethod,
}
impl fmt::Display for IngressError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "callback: {self:?}")
    }
}
impl std::error::Error for IngressError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebhookResponse {
    Empty,
    Text(String),
    Json(Value),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookOutcome {
    pub events: Vec<InboundEvent>,
    pub response: WebhookResponse,
}

/// One inbound protocol per connection, not selected from request headers.
/// Email takes a signed mailbox relay event, not direct SMTP or raw MIME.
#[derive(Debug, Clone)]
pub enum InboundAuth {
    Telegram {
        webhook_token: Secret,
        bot: crate::telegram::BotIdentity,
    },
    Slack {
        signing_secret: Secret,
        team_id: String,
        bot_user_id: Option<String>,
    },
    Discord {
        public_key: [u8; 32],
        application_id: String,
    },
    Email {
        signing_key: Secret,
    },
    /// The exact externally registered URL, never reconstructed from headers.
    Sms {
        auth_token: Secret,
        callback_url: String,
        destination: String,
    },
    Webhook {
        signing_key: Secret,
    },
    Feishu {
        encrypt_key: Secret,
        verification_token: Secret,
        app_id: String,
    },
    Dingtalk {
        app_secret: Secret,
        robot_code: String,
    },
    Wecom {
        token: Secret,
        encoding_aes_key: Secret,
        receiver_id: String,
        agent_id: u64,
    },
    WechatOfficial {
        token: Secret,
        encoding_aes_key: Secret,
        app_id: String,
    },
    Messenger {
        app_secret: Secret,
        verify_token: Secret,
        page_id: String,
    },
    Custom {
        signing_key: Secret,
    },
}

/// Provider endpoints are selected by the host, not by inbound payloads.
#[derive(Debug, Clone)]
pub enum OutboundAuth {
    Telegram {
        bot_token: Secret,
    },
    Slack {
        bot_token: Secret,
    },
    Discord {
        bot_token: Secret,
    },
    Email {
        from: String,
        signing_key: Secret,
    },
    Sms {
        account_sid: String,
        auth_token: Secret,
        from: String,
    },
    Webhook {
        signing_key: Secret,
    },
    Feishu {
        access_token: Secret,
    },
    Dingtalk {
        access_token: Secret,
        robot_code: String,
        group: bool,
    },
    Wecom {
        access_token: Secret,
        agent_id: u64,
    },
    WechatOfficial {
        access_token: Secret,
    },
    Messenger {
        page_access_token: Secret,
        page_id: String,
    },
    Custom {
        signing_key: Secret,
    },
}
macro_rules! auth_kind {
    ($ty:ty; $($variant:ident),+ $(,)?) => {
        impl $ty {
            pub fn kind(&self) -> ChannelKind {
                match self { $(Self::$variant { .. } => ChannelKind::$variant),+ }
            }
        }
    };
}
auth_kind!(InboundAuth; Telegram, Slack, Discord, Email, Sms, Webhook, Feishu, Dingtalk, Wecom, WechatOfficial, Messenger, Custom);
auth_kind!(OutboundAuth; Telegram, Slack, Discord, Email, Sms, Webhook, Feishu, Dingtalk, Wecom, WechatOfficial, Messenger, Custom);

/// Keep the delivery ID stable across retries; supply the current signing time.
#[derive(Debug, Clone, Copy)]
pub struct DeliveryContext<'a> {
    pub delivery_id: &'a str,
    pub unix_seconds: i64,
}

/// POST relative to a trusted configured base; an empty path uses that URL
/// verbatim. Never log this request or forward credentials on a redirect.
pub struct DeliveryRequest {
    pub channel: ChannelKind,
    pub path: String,
    pub headers: Vec<(&'static str, Secret)>,
    pub query: Vec<(&'static str, Secret)>,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

pub struct DeliveryResponse {
    pub status: u16,
    pub retry_after: Option<Duration>,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryError {
    InvalidMessage,
    UnsupportedContent,
    InvalidConfiguration,
    /// A definite refusal; unchanged input is not expected to succeed.
    Permanent,
    /// A definite refusal that may be retried after a wait.
    Retryable {
        retry_after: Option<Duration>,
    },
    /// Delivery may have happened. Reconcile before retrying.
    UnknownOutcome,
}
impl fmt::Display for DeliveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "delivery: {self:?}")
    }
}
impl std::error::Error for DeliveryError {}
impl DeliveryError {
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Retryable { .. })
    }
}

#[async_trait]
pub trait ChannelTransport: Send + Sync {
    /// Host owns timeouts, TLS, disabled redirects, bounds and egress policy.
    /// Use UnknownOutcome for errors after possible submission.
    async fn post(&self, request: DeliveryRequest) -> Result<DeliveryResponse, DeliveryError>;
}

#[async_trait]
pub trait ChannelConnector: Send + Sync {
    fn kind(&self) -> ChannelKind;
    fn capabilities(&self) -> ChannelCapabilities;
    fn ingest(&self, request: WebhookRequest<'_>) -> Result<WebhookOutcome, IngressError>;
    fn prepare(
        &self,
        message: &OutboundMessage,
        context: DeliveryContext<'_>,
    ) -> Result<DeliveryRequest, DeliveryError>;
    fn check_response(&self, response: &DeliveryResponse) -> Result<(), DeliveryError>;
    /// Exactly one explicit attempt. No implicit retries, reactions, or presence.
    async fn deliver(
        &self,
        transport: &dyn ChannelTransport,
        message: &OutboundMessage,
        context: DeliveryContext<'_>,
    ) -> Result<(), DeliveryError> {
        let request = self.prepare(message, context)?;
        self.check_response(&transport.post(request).await?)
    }
}

pub struct Connector {
    kind: ChannelKind,
    inbound: Option<InboundAuth>,
    outbound: Option<OutboundAuth>,
    max_body: usize,
}
impl Connector {
    pub fn new(
        kind: ChannelKind,
        inbound: Option<InboundAuth>,
        outbound: Option<OutboundAuth>,
    ) -> Result<Self, DeliveryError> {
        if (inbound.is_none() && outbound.is_none())
            || inbound.as_ref().is_some_and(|i| i.kind() != kind)
            || outbound.as_ref().is_some_and(|o| o.kind() != kind)
        {
            return Err(DeliveryError::InvalidConfiguration);
        }
        Ok(Self {
            kind,
            inbound,
            outbound,
            max_body: MAX_BODY_BYTES,
        })
    }
    /// Lower the callback limit for a connection. Cannot disable the hard cap.
    pub fn with_body_limit(mut self, bytes: usize) -> Self {
        self.max_body = bytes.min(MAX_BODY_BYTES);
        self
    }
}

pub const MAX_BODY_BYTES: usize = 1024 * 1024;

impl ChannelConnector for Connector {
    fn kind(&self) -> ChannelKind {
        self.kind
    }
    fn capabilities(&self) -> ChannelCapabilities {
        outbound::capabilities(self.kind)
    }
    fn ingest(&self, request: WebhookRequest<'_>) -> Result<WebhookOutcome, IngressError> {
        if request.body.len() > self.max_body
            || request.headers.len() > 128
            || request.query.len() > 64
            || request
                .headers
                .iter()
                .chain(request.query)
                .any(|(k, v)| k.len() > 256 || v.len() > 8192)
        {
            return Err(IngressError::TooLarge);
        }
        inbound::ingest(
            self.inbound.as_ref().ok_or(IngressError::Unauthorized)?,
            request,
        )
    }
    fn prepare(
        &self,
        message: &OutboundMessage,
        context: DeliveryContext<'_>,
    ) -> Result<DeliveryRequest, DeliveryError> {
        outbound::prepare(
            self.outbound
                .as_ref()
                .ok_or(DeliveryError::InvalidConfiguration)?,
            message,
            context,
        )
    }
    fn check_response(&self, response: &DeliveryResponse) -> Result<(), DeliveryError> {
        outbound::check_response(self.kind, response)
    }
}
