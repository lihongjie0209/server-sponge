use std::process::{Child, Command};

use log::{info, warn};

use crate::metrics::{MetricsStore, StressStatus};

/// Manages simulated external stress via stress-ng to demonstrate sponge yielding.
/// Uses the stress-ng tool for precise CPU load and memory allocation control.
pub struct StressManager {
    cpu_proc: Option<Child>,
    mem_proc: Option<Child>,
    cpu_workers: usize,
    cpu_load: usize,
    mem_mb: usize,
    metrics: Option<MetricsStore>,
}

impl StressManager {
    pub fn new(metrics: Option<MetricsStore>) -> Self {
        Self {
            cpu_proc: None,
            mem_proc: None,
            cpu_workers: 0,
            cpu_load: 0,
            mem_mb: 0,
            metrics,
        }
    }

    /// Start CPU stress via stress-ng.
    /// `workers`: number of CPU stressor threads.
    /// `load_pct`: target CPU load per worker (1-100%).
    /// `timeout`: auto-stop after N seconds (0 = no timeout).
    pub fn start_cpu(&mut self, workers: usize, load_pct: usize, timeout: usize) {
        self.stop_cpu();
        let workers = workers.max(1).min(64);
        let load = load_pct.max(1).min(100);

        let mut cmd = Command::new("stress-ng");
        cmd.args(["--cpu", &workers.to_string()])
            .args(["--cpu-load", &load.to_string()]);
        if timeout > 0 {
            cmd.args(["--timeout", &format!("{}s", timeout)]);
        }

        match cmd.spawn() {
            Ok(child) => {
                info!(
                    "🔥 压力模拟: 启动 CPU 压力 (workers={}, load={}%, timeout={}s, pid={})",
                    workers,
                    load,
                    timeout,
                    child.id()
                );
                self.cpu_proc = Some(child);
                self.cpu_workers = workers;
                self.cpu_load = load;
                self.publish_status();
            }
            Err(e) => {
                warn!("压力模拟: 启动 stress-ng 失败: {} (是否已安装 stress-ng?)", e);
            }
        }
    }

    pub fn stop_cpu(&mut self) {
        if let Some(mut child) = self.cpu_proc.take() {
            let pid = child.id();
            let _ = child.kill();
            let _ = child.wait();
            info!("🔥 压力模拟: CPU 压力已停止 (pid={})", pid);
        }
        self.cpu_workers = 0;
        self.cpu_load = 0;
        self.publish_status();
    }

    /// Start memory stress via stress-ng.
    /// `mb`: memory to allocate and hold in MB.
    /// `timeout`: auto-stop after N seconds (0 = no timeout).
    pub fn start_mem(&mut self, mb: usize, timeout: usize) {
        self.stop_mem();
        let mb = mb.max(1).min(10000);

        let mut cmd = Command::new("stress-ng");
        cmd.args(["--vm", "1"])
            .args(["--vm-bytes", &format!("{}M", mb)])
            .arg("--vm-keep");
        if timeout > 0 {
            cmd.args(["--timeout", &format!("{}s", timeout)]);
        }

        match cmd.spawn() {
            Ok(child) => {
                info!(
                    "🔥 压力模拟: 启动内存压力 (size={}MB, timeout={}s, pid={})",
                    mb,
                    timeout,
                    child.id()
                );
                self.mem_proc = Some(child);
                self.mem_mb = mb;
                self.publish_status();
            }
            Err(e) => {
                warn!("压力模拟: 启动 stress-ng 失败: {} (是否已安装 stress-ng?)", e);
            }
        }
    }

    pub fn stop_mem(&mut self) {
        if let Some(mut child) = self.mem_proc.take() {
            let pid = child.id();
            let _ = child.kill();
            let _ = child.wait();
            info!("🔥 压力模拟: 内存压力已停止 (pid={})", pid);
        }
        self.mem_mb = 0;
        self.publish_status();
    }

    pub fn stop_all(&mut self) {
        self.stop_cpu();
        self.stop_mem();
    }

    /// Check if child processes have exited (e.g., timeout expired).
    pub fn refresh_status(&mut self) {
        if let Some(ref mut child) = self.cpu_proc {
            if let Ok(Some(_status)) = child.try_wait() {
                info!("🔥 压力模拟: CPU 压力已自动结束 (超时)");
                self.cpu_proc = None;
                self.cpu_workers = 0;
                self.cpu_load = 0;
            }
        }
        if let Some(ref mut child) = self.mem_proc {
            if let Ok(Some(_status)) = child.try_wait() {
                info!("🔥 压力模拟: 内存压力已自动结束 (超时)");
                self.mem_proc = None;
                self.mem_mb = 0;
            }
        }
        self.publish_status();
    }

    pub fn cpu_active(&self) -> bool {
        self.cpu_proc.is_some()
    }

    pub fn mem_active(&self) -> bool {
        self.mem_proc.is_some()
    }

    pub fn status(&self) -> StressStatus {
        StressStatus {
            cpu_active: self.cpu_active(),
            mem_active: self.mem_active(),
            cpu_threads: self.cpu_workers,
            cpu_load: self.cpu_load,
            mem_mb: self.mem_mb,
        }
    }

    fn publish_status(&self) {
        if let Some(ref m) = self.metrics {
            m.update_stress(self.status());
        }
    }
}

impl Drop for StressManager {
    fn drop(&mut self) {
        self.stop_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_manager() {
        let mgr = StressManager::new(None);
        assert!(!mgr.cpu_active());
        assert!(!mgr.mem_active());
        assert_eq!(mgr.status().cpu_threads, 0);
        assert_eq!(mgr.status().mem_mb, 0);
    }

    #[test]
    fn test_status_default() {
        let mgr = StressManager::new(None);
        let s = mgr.status();
        assert!(!s.cpu_active);
        assert!(!s.mem_active);
        assert_eq!(s.cpu_load, 0);
    }

    #[test]
    fn test_double_stop_safe() {
        let mut mgr = StressManager::new(None);
        mgr.stop_cpu();
        mgr.stop_mem();
        mgr.stop_all();
    }

    #[test]
    fn test_refresh_when_no_process() {
        let mut mgr = StressManager::new(None);
        mgr.refresh_status(); // should not panic
    }

    #[test]
    fn test_metrics_integration() {
        let store = MetricsStore::new(70.0, 50.0);
        let mgr = StressManager::new(Some(store.clone()));
        let snap = store.snapshot();
        assert!(!snap.stress.cpu_active);
        assert!(!snap.stress.mem_active);
        drop(mgr);
    }

    // Integration tests requiring stress-ng (Linux + Docker only)
    #[cfg(target_os = "linux")]
    mod linux_tests {
        use super::*;
        use std::time::Duration;

        fn has_stress_ng() -> bool {
            Command::new("stress-ng")
                .arg("--version")
                .output()
                .is_ok()
        }

        #[test]
        fn test_cpu_stress_start_stop() {
            if !has_stress_ng() {
                return;
            }
            let mut mgr = StressManager::new(None);
            mgr.start_cpu(1, 50, 30);
            assert!(mgr.cpu_active());
            assert_eq!(mgr.status().cpu_threads, 1);
            assert_eq!(mgr.status().cpu_load, 50);
            std::thread::sleep(Duration::from_millis(200));
            mgr.stop_cpu();
            assert!(!mgr.cpu_active());
        }

        #[test]
        fn test_mem_stress_start_stop() {
            if !has_stress_ng() {
                return;
            }
            let mut mgr = StressManager::new(None);
            mgr.start_mem(10, 30);
            assert!(mgr.mem_active());
            assert_eq!(mgr.status().mem_mb, 10);
            std::thread::sleep(Duration::from_millis(200));
            mgr.stop_mem();
            assert!(!mgr.mem_active());
        }

        #[test]
        fn test_stop_all_with_processes() {
            if !has_stress_ng() {
                return;
            }
            let mut mgr = StressManager::new(None);
            mgr.start_cpu(1, 30, 30);
            mgr.start_mem(5, 30);
            assert!(mgr.cpu_active());
            assert!(mgr.mem_active());
            mgr.stop_all();
            assert!(!mgr.cpu_active());
            assert!(!mgr.mem_active());
        }
    }
}
