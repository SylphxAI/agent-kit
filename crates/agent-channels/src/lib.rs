//! Authenticated channel connectors, reply routing, and an outbound speech gate.
//! The crate never opens network connections; explicit delivery uses a host transport.
//! See [connector] for the normalized protocol surface.
//!
//! - [`telegram`] turns a Bot API `Message` object into a
//!   [`TelegramInboundMeta`](telegram::TelegramInboundMeta): the forum topic
//!   (only when the message is really in one), whether it is addressed to this
//!   bot, and whether it is context only.
//! - [`reply_target`] resolves one [`ReplyTarget`](reply_target::ReplyTarget)
//!   from structured metadata and an optional session key, accepting every
//!   spelling of the thread id, and builds Telegram send payloads that keep
//!   the reply in its topic.
//! - [`speech_gate`] decides whether an agent's terminal text may be delivered
//!   to an external channel. It fails closed.
//!
//! Channel metadata is not Instructions. Authenticate the event in the host,
//! check ingress policy before wake, and check the speech gate before sending.
//!
//! ## Parse a forum message and prepare a reply
//!
//! ```
//! use agent_channels::reply_target::{ReplyTarget, resolve_reply_target};
//! use agent_channels::speech_gate::{
//!     OutboundSpeechMode, TerminalDeliveryDecision, TerminalDeliveryInput,
//!     decide_terminal_text_delivery,
//! };
//! use agent_channels::telegram::{BotIdentity, TelegramInboundMeta};
//! use serde_json::json;
//!
//! let message = json!({
//!     "message_id": 10,
//!     "message_thread_id": 54007,
//!     "is_topic_message": true,
//!     "chat": {"id": -1001, "type": "supergroup", "is_forum": true},
//!     "from": {"id": 7, "is_bot": false},
//!     "text": "hello @example_bot",
//!     "entities": [{"type": "mention", "offset": 6, "length": 12}]
//! });
//! let bot = BotIdentity::new(Some("example_bot"), None);
//! let meta = TelegramInboundMeta::from_message(&message, &bot).unwrap();
//! assert_eq!(meta.thread_id, Some(54007));
//! assert!(meta.addressed && !meta.context_only);
//!
//! // The host stores `meta.reply_metadata()` with the queued item and later
//! // resolves the route from it.
//! let target = resolve_reply_target(Some(&meta.reply_metadata()), None).unwrap();
//! assert_eq!(target, ReplyTarget::telegram(-1001, Some(54007)));
//!
//! // Terminal text is only delivered when every gate allows it.
//! let decision = decide_terminal_text_delivery(&TerminalDeliveryInput {
//!     mode: OutboundSpeechMode::DirectTerminalText,
//!     context_only: meta.context_only,
//!     already_sent_via_tool: false,
//!     text: "Hi!",
//! });
//! assert_eq!(decision, TerminalDeliveryDecision::Deliver);
//! let payload = target.telegram_send_message_payload("Hi!", Some(10)).unwrap();
//! assert_eq!(payload["message_thread_id"], 54007);
//! assert_eq!(payload["reply_parameters"]["message_id"], 10);
//! ```

pub mod connector;
pub mod reply_target;
pub mod speech_gate;
pub mod telegram;
