//! Sender awareness and default-open ingress policy, with no network or model calls.
//!
//! The host authenticates the event, resolves sender relationships independently
//! of message content, and scopes the store to the receiving agent and account
//! (or supplies channel keys that already include the account namespace). Run
//! the policy before queueing or waking the agent. Default-open means no active
//! match after a successful lookup; it does not turn store errors into allows.
//!
//! Use [`SenderIdentity`] for structured sender awareness, not as a source of
//! Instructions from the sender. The host decides what an allowed event can do.
//!
//! ```
//! use agent_ingress::{
//!     BlockMatch, BlockScope, ChatType, IngressBlockRule, IngressDecision,
//!     SenderIdentity, SenderRelationship, evaluate_ingress,
//! };
//!
//! let sender = SenderIdentity {
//!     channel: "telegram:demo".into(),
//!     sender_id: "42".into(),
//!     chat_id: Some("-1001".into()),
//!     chat_type: ChatType::Group,
//!     username: None,
//!     display_name: Some("Moderator".into()),
//!     relationship: SenderRelationship::Stranger, // From trusted host state.
//! };
//! let awareness = sender.format_awareness_header();
//! assert!(awareness.contains(r#""relationship":"stranger""#));
//! assert_eq!(evaluate_ingress(&sender, &[], 100), IngressDecision::Allow);
//!
//! // Stored rule, admitted earlier by the host with ValidatedBlockRule.
//! let rule = IngressBlockRule {
//!     rule_id: "rule-1".into(),
//!     target: BlockMatch {
//!         channel: sender.channel.clone(),
//!         scope: BlockScope::Sender { sender_id: sender.sender_id.clone() },
//!     },
//!     reason: None,
//!     blocked_by: "operator".into(),
//!     created_at_ms: 10,
//!     expires_at_ms: None,
//! };
//! assert!(matches!(evaluate_ingress(&sender, &[rule], 100), IngressDecision::Drop { .. }));
//! ```

mod identity;
mod policy;
mod store;
mod tool;

pub use identity::{ChatType, SenderIdentity, SenderRelationship};
pub use policy::{
    BlockMatch, BlockScope, IngressBlockRule, IngressDecision, PolicyError, ValidatedBlockRule,
    evaluate_ingress, validate_block_target,
};
pub use store::{InMemoryIngressBlockStore, IngressBlockStore, StoreError};
pub use tool::{
    BlockWriteContext, IngressGuardToolRequest, IngressGuardToolResponse, SuggestedChannelAction,
    ToolError,
};
