use serde::Serialize;
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[derive(Clone, Serialize, Default, Debug)]
pub struct MemoryMetrics {
    pub total_mb: f64,
    pub used_mb: f64,
    pub available_mb: f64,
    pub usage_pct: f64,
    pub pool_chunks: usize,
    pub pool_mb: f64,
    pub mode: String,
    pub pid_output: f64,
    pub pid_p: f64,
    pub pid_i: f64,
    pub pid_d: f64,
}

#[derive(Clone, Serialize, Default, Debug)]
pub struct CpuMetrics {
    pub total_pct: f64,
    pub others_pct: f64,
    pub self_pct: f64,
    pub duty_cycle: f64,
    pub workers: usize,
}

#[derive(Clone, Serialize, Default, Debug)]
pub struct StressStatus {
    pub cpu_active: bool,
    pub mem_active: bool,
    pub cpu_threads: usize,
    pub cpu_load: usize,
    pub mem_mb: usize,
}

#[derive(Clone, Serialize, Debug)]
pub struct MetricsSnapshot {
    pub timestamp: u64,
    pub uptime_secs: u64,
    pub memory: MemoryMetrics,
    pub cpu: CpuMetrics,
    pub stress: StressStatus,
    pub mem_target: f64,
    pub cpu_target: f64,
}

struct Inner {
    start_time: u64,
    memory: MemoryMetrics,
    cpu: CpuMetrics,
    stress: StressStatus,
    mem_target: f64,
    cpu_target: f64,
}

#[derive(Clone)]
pub struct MetricsStore {
    inner: Arc<RwLock<Inner>>,
}

impl MetricsStore {
    pub fn new(mem_target: f64, cpu_target: f64) -> Self {
        Self {
            inner: Arc::new(RwLock::new(Inner {
                start_time: now_secs(),
                memory: MemoryMetrics::default(),
                cpu: CpuMetrics::default(),
                stress: StressStatus::default(),
                mem_target,
                cpu_target,
            })),
        }
    }

    pub fn update_memory(&self, m: MemoryMetrics) {
        if let Ok(mut w) = self.inner.write() {
            w.memory = m;
        }
    }

    pub fn update_cpu(&self, c: CpuMetrics) {
        if let Ok(mut w) = self.inner.write() {
            w.cpu = c;
        }
    }

    pub fn update_stress(&self, s: StressStatus) {
        if let Ok(mut w) = self.inner.write() {
            w.stress = s;
        }
    }

    pub fn snapshot(&self) -> MetricsSnapshot {
        let r = self.inner.read().unwrap();
        let now = now_secs();
        MetricsSnapshot {
            timestamp: now,
            uptime_secs: now.saturating_sub(r.start_time),
            memory: r.memory.clone(),
            cpu: r.cpu.clone(),
            stress: r.stress.clone(),
            mem_target: r.mem_target,
            cpu_target: r.cpu_target,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_store_defaults() {
        let store = MetricsStore::new(70.0, 50.0);
        let snap = store.snapshot();
        assert_eq!(snap.memory.total_mb, 0.0);
        assert_eq!(snap.cpu.total_pct, 0.0);
        assert!(!snap.stress.cpu_active);
    }

    #[test]
    fn test_update_memory() {
        let store = MetricsStore::new(70.0, 50.0);
        store.update_memory(MemoryMetrics {
            total_mb: 512.0,
            used_mb: 300.0,
            available_mb: 212.0,
            usage_pct: 58.6,
            pool_chunks: 3,
            pool_mb: 48.0,
            mode: "STEADY".into(),
            pid_output: 1.5,
            pid_p: 1.0,
            pid_i: 0.3,
            pid_d: 0.2,
        });
        let snap = store.snapshot();
        assert_eq!(snap.memory.total_mb, 512.0);
        assert_eq!(snap.memory.pool_chunks, 3);
        assert_eq!(snap.memory.mode, "STEADY");
    }

    #[test]
    fn test_update_cpu() {
        let store = MetricsStore::new(70.0, 50.0);
        store.update_cpu(CpuMetrics {
            total_pct: 70.0,
            others_pct: 10.0,
            self_pct: 60.0,
            duty_cycle: 0.85,
            workers: 4,
        });
        let snap = store.snapshot();
        assert_eq!(snap.cpu.total_pct, 70.0);
        assert_eq!(snap.cpu.workers, 4);
    }

    #[test]
    fn test_update_stress() {
        let store = MetricsStore::new(70.0, 50.0);
        store.update_stress(StressStatus {
            cpu_active: true,
            mem_active: false,
            cpu_threads: 2,
            cpu_load: 80,
            mem_mb: 0,
        });
        let snap = store.snapshot();
        assert!(snap.stress.cpu_active);
        assert_eq!(snap.stress.cpu_threads, 2);
        assert_eq!(snap.stress.cpu_load, 80);
    }

    #[test]
    fn test_uptime_nonnegative() {
        let store = MetricsStore::new(70.0, 50.0);
        let snap = store.snapshot();
        assert!(snap.uptime_secs < 2);
    }

    #[test]
    fn test_snapshot_clone_isolation() {
        let store = MetricsStore::new(70.0, 50.0);
        store.update_memory(MemoryMetrics {
            total_mb: 100.0,
            ..Default::default()
        });
        let snap1 = store.snapshot();
        store.update_memory(MemoryMetrics {
            total_mb: 200.0,
            ..Default::default()
        });
        let snap2 = store.snapshot();
        assert_eq!(snap1.memory.total_mb, 100.0);
        assert_eq!(snap2.memory.total_mb, 200.0);
    }

    #[test]
    fn test_thread_safety() {
        use std::thread;
        let store = MetricsStore::new(70.0, 50.0);
        let s1 = store.clone();
        let s2 = store.clone();
        let h1 = thread::spawn(move || {
            for i in 0..100 {
                s1.update_memory(MemoryMetrics {
                    total_mb: i as f64,
                    ..Default::default()
                });
            }
        });
        let h2 = thread::spawn(move || {
            for i in 0..100 {
                s2.update_cpu(CpuMetrics {
                    total_pct: i as f64,
                    ..Default::default()
                });
            }
        });
        h1.join().unwrap();
        h2.join().unwrap();
        let _snap = store.snapshot(); // should not panic
    }

    #[test]
    fn test_snapshot_serializes_to_json() {
        let store = MetricsStore::new(70.0, 50.0);
        store.update_memory(MemoryMetrics {
            total_mb: 512.0,
            usage_pct: 70.0,
            mode: "STEADY".into(),
            ..Default::default()
        });
        let snap = store.snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        assert!(json.contains("512"));
        assert!(json.contains("STEADY"));
    }
}
