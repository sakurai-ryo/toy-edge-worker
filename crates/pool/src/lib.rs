//! Isolate プール（warm/cold 管理、LRU）とメトリクス。
//!
//! プールは worker スレッドごとに持つ（Isolate はスレッドを跨げないため）。
//! 容量はプロセス全体の isolates_max をスレッド数で割って分配する。
//! エントリ数は小さい想定なので、LRU は線形探索の自前実装で十分。

use std::collections::HashMap;
use std::sync::Mutex;

use serde::Serialize;

/// テナント名 → warm エントリの LRU プール。
/// W は runtime::Worker を想定したジェネリクス（pool crate を v8 非依存に保つ）。
pub struct Pool<W> {
    capacity: usize,
    entries: HashMap<String, Entry<W>>,
    /// LRU 用の論理時計。get/insert のたびに進む
    tick: u64,
}

struct Entry<W> {
    worker: W,
    last_used: u64,
}

impl<W> Pool<W> {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            entries: HashMap::new(),
            tick: 0,
        }
    }

    /// warm エントリを取得して最終使用時刻を更新する。
    pub fn get(&mut self, tenant: &str) -> Option<&mut W> {
        self.tick += 1;
        let tick = self.tick;
        self.entries.get_mut(tenant).map(|entry| {
            entry.last_used = tick;
            &mut entry.worker
        })
    }

    /// エントリを追加する。容量超過なら最も古いエントリを evict して返す。
    pub fn insert(&mut self, tenant: String, worker: W) -> Option<(String, W)> {
        self.tick += 1;
        let evicted = if self.entries.len() >= self.capacity {
            self.evict_lru()
        } else {
            None
        };
        self.entries.insert(
            tenant,
            Entry {
                worker,
                last_used: self.tick,
            },
        );
        evicted
    }

    /// terminate 後の破棄などで明示的に取り除く。
    pub fn remove(&mut self, tenant: &str) -> Option<W> {
        self.entries.remove(tenant).map(|entry| entry.worker)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn evict_lru(&mut self) -> Option<(String, W)> {
        let oldest = self
            .entries
            .iter()
            .min_by_key(|(_, entry)| entry.last_used)
            .map(|(tenant, _)| tenant.clone())?;
        let entry = self.entries.remove(&oldest)?;
        Some((oldest, entry.worker))
    }
}

/// テナント別の累積メトリクス。`GET /__admin/metrics` でそのまま JSON になる。
#[derive(Debug, Default, Clone, Serialize)]
pub struct TenantMetrics {
    pub requests: u64,
    pub errors: u64,
    pub cold_starts: u64,
    pub warm_hits: u64,
    pub evictions: u64,
    pub cold_start_ms_last: f64,
    pub cold_start_ms_max: f64,
    pub cold_start_ms_sum: f64,
    pub terminations_cpu: u64,
    pub terminations_mem: u64,
}

impl TenantMetrics {
    pub fn record_cold_start(&mut self, ms: f64) {
        self.cold_starts += 1;
        self.cold_start_ms_last = ms;
        self.cold_start_ms_max = self.cold_start_ms_max.max(ms);
        self.cold_start_ms_sum += ms;
    }
}

/// 全スレッドから更新される共有メトリクス。
#[derive(Default)]
pub struct Metrics {
    inner: Mutex<HashMap<String, TenantMetrics>>,
}

impl Metrics {
    pub fn with<F: FnOnce(&mut TenantMetrics)>(&self, tenant: &str, f: F) {
        let mut inner = self.inner.lock().unwrap();
        f(inner.entry(tenant.to_string()).or_default());
    }

    pub fn snapshot(&self) -> HashMap<String, TenantMetrics> {
        self.inner.lock().unwrap().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lru_evicts_oldest() {
        let mut pool: Pool<u32> = Pool::new(2);
        assert!(pool.insert("a".into(), 1).is_none());
        assert!(pool.insert("b".into(), 2).is_none());
        // a に触れて b を最古にする
        assert_eq!(pool.get("a"), Some(&mut 1));
        let evicted = pool.insert("c".into(), 3);
        assert_eq!(evicted, Some(("b".into(), 2)));
        assert_eq!(pool.len(), 2);
        assert!(pool.get("a").is_some());
        assert!(pool.get("c").is_some());
    }

    #[test]
    fn capacity_at_least_one() {
        let mut pool: Pool<u32> = Pool::new(0);
        assert!(pool.insert("a".into(), 1).is_none());
        let evicted = pool.insert("b".into(), 2);
        assert_eq!(evicted, Some(("a".into(), 1)));
    }

    #[test]
    fn remove_returns_worker() {
        let mut pool: Pool<u32> = Pool::new(4);
        pool.insert("a".into(), 1);
        assert_eq!(pool.remove("a"), Some(1));
        assert!(pool.is_empty());
    }

    #[test]
    fn metrics_aggregate() {
        let metrics = Metrics::default();
        metrics.with("t", |m| m.record_cold_start(5.0));
        metrics.with("t", |m| m.record_cold_start(3.0));
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot["t"].cold_starts, 2);
        assert_eq!(snapshot["t"].cold_start_ms_max, 5.0);
        assert_eq!(snapshot["t"].cold_start_ms_last, 3.0);
    }
}
