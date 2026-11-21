//! Channel primitives shared by agent hosts: inbound parsing, reply routing,
//! and the outbound speech gate. No network, storage, or model calls.
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
//! A typical host pipeline:
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
//! ```

pub mod reply_target;
pub mod speech_gate;
pub mod telegram;
