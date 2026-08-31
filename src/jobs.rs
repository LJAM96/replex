use crate::observability::Metrics;
use moka::future::Cache;
use serde::Serialize;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

pub struct JobSupervisor {
    inflight: Mutex<HashSet<String>>,
    cooldown: Cache<String, ()>,
    metrics: Arc<Metrics>,
}

#[derive(Debug, Serialize)]
pub struct JobSnapshot {
    pub inflight: usize,
    pub cooling_down: u64,
}

impl JobSupervisor {
    pub fn new(metrics: Arc<Metrics>) -> Self {
        Self {
            inflight: Mutex::new(HashSet::new()),
            cooldown: Cache::builder()
                .time_to_live(Duration::from_secs(60 * 60))
                .max_capacity(10_000)
                .build(),
            metrics,
        }
    }

    /// Claim an account/job key once. Successful and failed attempts both
    /// cool down, preventing request storms from becoming external write loops.
    pub async fn try_start(&self, key: &str) -> bool {
        if self.cooldown.get(key).await.is_some() {
            return false;
        }
        let mut inflight = self.inflight.lock().await;
        if !inflight.insert(key.to_string()) {
            return false;
        }
        self.cooldown.insert(key.to_string(), ()).await;
        true
    }

    pub async fn finish(&self, key: &str, success: bool) {
        self.inflight.lock().await.remove(key);
        if !success {
            self.metrics.task_failure();
        }
    }

    pub async fn snapshot(&self) -> JobSnapshot {
        JobSnapshot {
            inflight: self.inflight.lock().await.len(),
            cooling_down: self.cooldown.entry_count(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_job_key_is_single_flight_and_rate_limited() {
        let jobs = JobSupervisor::new(Arc::new(Metrics::default()));
        assert!(jobs.try_start("account:notifications").await);
        assert!(!jobs.try_start("account:notifications").await);
        jobs.finish("account:notifications", false).await;
        assert!(!jobs.try_start("account:notifications").await);
    }
}
