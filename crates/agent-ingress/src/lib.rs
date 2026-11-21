//! Sender awareness and default-open ingress policy, with no network or model calls.
//!
//! The host authenticates the event, resolves sender relationships independently
//! of message content, and scopes the store to the receiving agent and account
//! (or supplies channel keys that already include the account namespace). Run
//! the policy before queueing or waking the agent. Default-open means no active
//! match after a successful lookup; it does not turn store errors into allows.

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
