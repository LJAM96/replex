use crate::config::Config;
use crate::resolution_policy::{PolicyEntry, ResolutionLimit};
use serde::Serialize;
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Debug, Clone, Serialize)]
pub struct PolicySnapshot {
    pub enabled: bool,
    #[serde(skip)]
    pub entries: Vec<PolicyEntry>,
    #[serde(skip)]
    pub default_limit: ResolutionLimit,
    pub fail_closed: bool,
    pub hidden_collections: Vec<String>,
    pub generation: u64,
}

impl PolicySnapshot {
    fn from_config(config: &Config, generation: u64) -> Self {
        Self {
            enabled: config.resolution_policy_enabled,
            entries: config.user_resolution_policies.clone(),
            default_limit: config.resolution_default,
            fail_closed: config.resolution_policy_fail_closed,
            hidden_collections: config
                .hidden_collections
                .clone()
                .unwrap_or_default(),
            generation,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PolicyStore(Arc<RwLock<PolicySnapshot>>);

impl PolicyStore {
    pub fn new(config: &Config) -> Self {
        Self(Arc::new(RwLock::new(PolicySnapshot::from_config(
            config, 1,
        ))))
    }

    pub async fn snapshot(&self) -> PolicySnapshot {
        self.0.read().await.clone()
    }

    pub async fn reload(&self, config: &Config) -> PolicySnapshot {
        let mut current = self.0.write().await;
        let next = PolicySnapshot::from_config(config, current.generation + 1);
        *current = next.clone();
        next
    }
}
