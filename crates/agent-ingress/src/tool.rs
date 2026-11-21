use serde::{Deserialize, Serialize};

use crate::{
    BlockMatch, BlockScope, ChatType, IngressBlockRule, IngressBlockStore, PolicyError,
    SenderRelationship, StoreError, ValidatedBlockRule,
};

/// Model-supplied intent. Authority and audit fields are not part of this wire
/// contract; the host must supply them separately.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum IngressGuardToolRequest {
    /// Select a sender or an entire chat with the target's scope.
    BlockSender {
        target: BlockMatch,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expires_at_ms: Option<i64>,
    },
    UnblockSender {
        target: BlockMatch,
    },
    ListBlocks,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum IngressGuardToolResponse {
    Blocked {
        rule: IngressBlockRule,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        suggested_channel_action: Option<SuggestedChannelAction>,
    },
    Unblocked {
        removed: bool,
    },
    Blocks {
        rules: Vec<IngressBlockRule>,
    },
}

/// Advisory only. Leaving a chat may stop delivery before ingress policy can
/// run. The host must separately authorize the effect and ensure it does not
/// cut off a protected sender; never run it just because a block succeeded.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum SuggestedChannelAction {
    LeaveChat { channel: String, chat_id: String },
}

/// Out-of-band context for a block, with no deserializer. Use the same
/// authorization scope as the store. Resolve the exact target independently
/// of the model and keep its classification current through the write.
#[derive(Clone, Debug)]
pub struct BlockWriteContext<'a> {
    pub target: &'a BlockMatch,
    /// Verified relationship of the targeted principal, or the verified
    /// relationship for a chat target under the host's policy. Do not reuse the
    /// triggering sender's classification to authorize an unrelated target.
    pub relationship: SenderRelationship,
    pub chat_type: ChatType,
    /// Unique within the store's scope, supplied by the host.
    pub rule_id: String,
    pub blocked_by: String,
    pub now_ms: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error(transparent)]
    Policy(#[from] PolicyError),
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl IngressGuardToolRequest {
    /// Execute authorized tool intent using a scoped store. Missing or mismatched
    /// context rejects a block. Context is not needed to remove or list rules.
    pub async fn execute(
        self,
        store: &dyn IngressBlockStore,
        block_context: Option<BlockWriteContext<'_>>,
    ) -> Result<IngressGuardToolResponse, ToolError> {
        Ok(match self {
            Self::BlockSender {
                target,
                reason,
                expires_at_ms,
            } => {
                let ctx = block_context.ok_or(PolicyError::UnresolvedBlockTarget)?;
                let rule = IngressBlockRule {
                    rule_id: ctx.rule_id,
                    target,
                    reason,
                    blocked_by: ctx.blocked_by,
                    created_at_ms: ctx.now_ms,
                    expires_at_ms,
                };
                let validated =
                    ValidatedBlockRule::new(rule.clone(), ctx.target, ctx.relationship)?;
                store.block(validated).await?;
                let suggested_channel_action = match (&rule.target.scope, ctx.chat_type) {
                    (BlockScope::Chat { chat_id }, ChatType::Group) => {
                        Some(SuggestedChannelAction::LeaveChat {
                            channel: rule.target.channel.clone(),
                            chat_id: chat_id.clone(),
                        })
                    }
                    _ => None,
                };
                IngressGuardToolResponse::Blocked {
                    rule,
                    suggested_channel_action,
                }
            }
            Self::UnblockSender { target } => {
                target.validate()?;
                IngressGuardToolResponse::Unblocked {
                    removed: store.unblock(&target).await?,
                }
            }
            Self::ListBlocks => IngressGuardToolResponse::Blocks {
                rules: store.list_blocks().await?,
            },
        })
    }
}
