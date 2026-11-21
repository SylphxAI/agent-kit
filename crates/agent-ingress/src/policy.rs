use serde::{Deserialize, Serialize};

use crate::{SenderIdentity, SenderRelationship};

/// Block an exact sender or conversation ID, never a display label or pattern.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum BlockScope {
    Sender { sender_id: String },
    Chat { chat_id: String },
}

/// A block target, keyed by channel and by an ID within one scope.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlockMatch {
    pub channel: String,
    pub scope: BlockScope,
}

impl BlockMatch {
    pub(crate) fn validate(&self) -> Result<(), PolicyError> {
        if self.channel.trim().is_empty() {
            return Err(PolicyError::EmptyChannel);
        }
        let id = match &self.scope {
            BlockScope::Sender { sender_id } => sender_id,
            BlockScope::Chat { chat_id } => chat_id,
        };
        if id.trim().is_empty() {
            return Err(PolicyError::EmptyTargetId);
        }
        Ok(())
    }

    fn matches(&self, sender: &SenderIdentity) -> bool {
        self.channel == sender.channel
            && match &self.scope {
                BlockScope::Sender { sender_id } => *sender_id == sender.sender_id,
                BlockScope::Chat { chat_id } => Some(chat_id.as_str()) == sender.chat_id.as_deref(),
            }
    }
}

/// Stored rule data. Deserializing it does not authorize writing it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngressBlockRule {
    pub rule_id: String,
    pub target: BlockMatch,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub blocked_by: String,
    pub created_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum IngressDecision {
    Allow,
    Drop {
        rule_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
}

/// Evaluate before waking the agent. Protected senders bypass all stored rules.
/// An expiry equal to the supplied time is already expired. If several rules
/// match, a sender rule takes precedence over a chat rule, then the smallest
/// rule ID in lexical order wins (first in the slice on an exact tie).
pub fn evaluate_ingress(
    sender: &SenderIdentity,
    rules: &[IngressBlockRule],
    now_ms: i64,
) -> IngressDecision {
    if sender.relationship.is_protected() {
        return IngressDecision::Allow;
    }

    rules
        .iter()
        .filter(|rule| rule.expires_at_ms.is_none_or(|expiry| expiry > now_ms))
        .filter(|rule| rule.target.matches(sender))
        .min_by_key(|rule| {
            (
                matches!(rule.target.scope, BlockScope::Chat { .. }),
                &rule.rule_id,
            )
        })
        .map_or(IngressDecision::Allow, |rule| IngressDecision::Drop {
            rule_id: rule.rule_id.clone(),
            reason: rule.reason.clone(),
        })
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PolicyError {
    #[error("protected relationships cannot be blocked: {0:?}")]
    ProtectedRelationship(SenderRelationship),
    #[error("channel must not be blank")]
    EmptyChannel,
    #[error("target ID must not be blank")]
    EmptyTargetId,
    #[error("block requires trusted context for the exact target")]
    UnresolvedBlockTarget,
    #[error("expiry must be later than creation")]
    InvalidExpiry,
}

/// Check a target using its independently resolved relationship. This value
/// must come from the host, never from agent/tool input or display metadata.
pub fn validate_block_target(
    target: &BlockMatch,
    target_relationship: SenderRelationship,
) -> Result<(), PolicyError> {
    if target_relationship.is_protected() {
        return Err(PolicyError::ProtectedRelationship(target_relationship));
    }
    target.validate()
}

/// A rule admitted at the write boundary. This type cannot be deserialized or
/// constructed directly from stored rows or tool JSON.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidatedBlockRule(IngressBlockRule);

impl ValidatedBlockRule {
    /// Bind trusted relationship data to the exact channel, scope and ID.
    /// The host must keep that classification current through the write.
    pub fn new(
        rule: IngressBlockRule,
        resolved_target: &BlockMatch,
        relationship: SenderRelationship,
    ) -> Result<Self, PolicyError> {
        if &rule.target != resolved_target {
            return Err(PolicyError::UnresolvedBlockTarget);
        }
        validate_block_target(&rule.target, relationship)?;
        if rule
            .expires_at_ms
            .is_some_and(|expiry| expiry <= rule.created_at_ms)
        {
            return Err(PolicyError::InvalidExpiry);
        }
        Ok(Self(rule))
    }

    pub fn into_rule(self) -> IngressBlockRule {
        self.0
    }
}
