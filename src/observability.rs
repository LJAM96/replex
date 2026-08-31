use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Default)]
pub struct Metrics {
    policy_rejects: AtomicU64,
    cache_hits: AtomicU64,
    cache_misses: AtomicU64,
    redirects: AtomicU64,
    upstream_failures: AtomicU64,
    upstream_requests: AtomicU64,
    upstream_latency_ms: AtomicU64,
    task_failures: AtomicU64,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct MetricsSnapshot {
    pub policy_rejects: u64,
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub redirects: u64,
    pub upstream_failures: u64,
    pub upstream_requests: u64,
    pub upstream_latency_ms: u64,
    pub task_failures: u64,
}

impl Metrics {
    pub fn policy_reject(&self) {
        self.policy_rejects.fetch_add(1, Ordering::Relaxed);
    }
    pub fn cache_hit(&self) {
        self.cache_hits.fetch_add(1, Ordering::Relaxed);
    }
    pub fn cache_miss(&self) {
        self.cache_misses.fetch_add(1, Ordering::Relaxed);
    }
    pub fn redirect(&self) {
        self.redirects.fetch_add(1, Ordering::Relaxed);
    }
    pub fn upstream_failure(&self) {
        self.upstream_failures.fetch_add(1, Ordering::Relaxed);
    }
    pub fn observe_upstream(&self, elapsed_ms: u64, success: bool) {
        self.upstream_requests.fetch_add(1, Ordering::Relaxed);
        self.upstream_latency_ms
            .fetch_add(elapsed_ms, Ordering::Relaxed);
        if !success {
            self.upstream_failure();
        }
    }
    pub fn task_failure(&self) {
        self.task_failures.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            policy_rejects: self.policy_rejects.load(Ordering::Relaxed),
            cache_hits: self.cache_hits.load(Ordering::Relaxed),
            cache_misses: self.cache_misses.load(Ordering::Relaxed),
            redirects: self.redirects.load(Ordering::Relaxed),
            upstream_failures: self.upstream_failures.load(Ordering::Relaxed),
            upstream_requests: self.upstream_requests.load(Ordering::Relaxed),
            upstream_latency_ms: self
                .upstream_latency_ms
                .load(Ordering::Relaxed),
            task_failures: self.task_failures.load(Ordering::Relaxed),
        }
    }

    pub fn prometheus(&self) -> String {
        let s = self.snapshot();
        format!(
            "# TYPE replex_policy_rejects_total counter\nreplex_policy_rejects_total {}\n# TYPE replex_cache_hits_total counter\nreplex_cache_hits_total {}\n# TYPE replex_cache_misses_total counter\nreplex_cache_misses_total {}\n# TYPE replex_redirects_total counter\nreplex_redirects_total {}\n# TYPE replex_upstream_failures_total counter\nreplex_upstream_failures_total {}\n# TYPE replex_upstream_requests_total counter\nreplex_upstream_requests_total {}\n# TYPE replex_upstream_latency_milliseconds_total counter\nreplex_upstream_latency_milliseconds_total {}\n# TYPE replex_task_failures_total counter\nreplex_task_failures_total {}\n",
            s.policy_rejects, s.cache_hits, s.cache_misses, s.redirects,
            s.upstream_failures, s.upstream_requests, s.upstream_latency_ms,
            s.task_failures
        )
    }
}
