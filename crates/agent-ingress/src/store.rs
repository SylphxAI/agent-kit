use std::{collections::BTreeMap, sync::Mutex};

use async_trait::async_trait;

use crate::{
    BlockMatch, IngressBlockRule, IngressDecision, SenderIdentity, ValidatedBlockRule,
    evaluate_ingress,
};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("block storage is unavailable")]
    Backend(#[source] Box<dyn std::error::Error + Send + Sync>),
}

/// Storage port for one receiving agent and its authorized account scope.
/// Adapters own authorization and namespace isolation, not ingress policy.
#[async_trait]
pub trait IngressBlockStore: Send + Sync {
    /// Atomically insert or replace the rule for the exact target.
    async fn block(&self, rule: ValidatedBlockRule) -> Result<(), StoreError>;

    /// Remove the exact target, returning whether a rule was present.
    async fn unblock(&self, target: &BlockMatch) -> Result<bool, StoreError>;

    /// List stored rules, including expired rules. Order is backend-defined.
    async fn list_blocks(&self) -> Result<Vec<IngressBlockRule>, StoreError>;

    /// Return candidates for a sender. Override to limit the query to both the
    /// sender and chat keys. Do not drop potential matches; the core checks
    /// exact scope, matching and expiration. The default works for small lists.
    async fn matching_rules(
        &self,
        _sender: &SenderIdentity,
    ) -> Result<Vec<IngressBlockRule>, StoreError> {
        self.list_blocks().await
    }

    /// Evaluate before enqueue or wake. Protected relationships bypass even a
    /// failed store. For other relationships propagate read failures, not Allow.
    /// Keep this policy default; optimize only the candidate query.
    async fn is_blocked(
        &self,
        sender: &SenderIdentity,
        now_ms: i64,
    ) -> Result<IngressDecision, StoreError> {
        if sender.relationship.is_protected() {
            return Ok(IngressDecision::Allow);
        }
        let rules = self.matching_rules(sender).await?;
        Ok(evaluate_ingress(sender, &rules, now_ms))
    }
}

/// In-memory, deterministic reference store. Cloning rules never holds the
/// mutex across an await; the store is shared by reference or by an Arc.
#[derive(Debug, Default)]
pub struct InMemoryIngressBlockStore {
    rules: Mutex<BTreeMap<BlockMatch, IngressBlockRule>>,
}

impl InMemoryIngressBlockStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn rules(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, BTreeMap<BlockMatch, IngressBlockRule>>, StoreError> {
        self.rules.lock().map_err(|_| {
            StoreError::Backend(Box::new(std::io::Error::other("block store lock poisoned")))
        })
    }
}

#[async_trait]
impl IngressBlockStore for InMemoryIngressBlockStore {
    async fn block(&self, rule: ValidatedBlockRule) -> Result<(), StoreError> {
        let rule = rule.into_rule();
        self.rules()?.insert(rule.target.clone(), rule);
        Ok(())
    }

    async fn unblock(&self, target: &BlockMatch) -> Result<bool, StoreError> {
        Ok(self.rules()?.remove(target).is_some())
    }

    async fn list_blocks(&self) -> Result<Vec<IngressBlockRule>, StoreError> {
        Ok(self.rules()?.values().cloned().collect())
    }

    async fn matching_rules(
        &self,
        sender: &SenderIdentity,
    ) -> Result<Vec<IngressBlockRule>, StoreError> {
        use crate::BlockScope;

        let rules = self.rules()?;
        let sender_key = BlockMatch {
            channel: sender.channel.clone(),
            scope: BlockScope::Sender {
                sender_id: sender.sender_id.clone(),
            },
        };
        let chat_rule = sender.chat_id.as_ref().and_then(|chat_id| {
            rules.get(&BlockMatch {
                channel: sender.channel.clone(),
                scope: BlockScope::Chat {
                    chat_id: chat_id.clone(),
                },
            })
        });
        Ok(rules
            .get(&sender_key)
            .into_iter()
            .chain(chat_rule)
            .cloned()
            .collect())
    }
}
