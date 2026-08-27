use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use log::{debug, info, warn};

use crate::config::Config;
use crate::cpu_stat::{self, CpuUsage, ProcessCpuSample};
use crate::metrics::{self, MetricsStore};
use crate::pid::PidController;

/// CPU load controller: spawns worker threads that generate load via PWM duty cycle,
/// and a control thread that adjusts duty via PID feedback.
pub struct CpuController {
    worker_handles: Vec<JoinHandle<()>>,
    control_handle: Option<JoinHandle<()>>,
}

impl CpuController {
    /// Start the CPU controller with N worker threads + 1 control thread.
    pub fn start(config: &Config, running: Arc<AtomicBool>, metrics: Option<MetricsStore>) -> Self {
        let num_cpus = if config.cpu_workers > 0 {
            config.cpu_workers
        } else {
            cpu_stat::detect_num_cpus()
        };

        info!(
            "CPU Sponge starting: target={:.0}%, cycle={}ms, panic_margin={:.0}%, workers={}",
            config.cpu_target, config.cpu_cycle, config.cpu_panic_margin, num_cpus,
        );

        // Shared duty cycle (0.0–1.0), encoded as AtomicU64 via f64::to_bits
        let duty = Arc::new(AtomicU64::new(0.0f64.to_bits()));
        let cycle_ns = config.cpu_cycle * 1_000_000;

        // Spawn worker threads
        let mut worker_handles = Vec::with_capacity(num_cpus);
        for i in 0..num_cpus {
            let duty = duty.clone();
            let running = running.clone();
            let handle = thread::Builder::new()
                .name(format!("cpu-worker-{}", i))
                .spawn(move || worker_loop(i, duty, running, cycle_ns))
                .expect("failed to spawn CPU worker thread");
            worker_handles.push(handle);
        }

        // Spawn control thread
        let ctrl_config = config.clone();
        let ctrl_running = running.clone();
        let ctrl_duty = duty;
        let control_handle = thread::Builder::new()
            .name("cpu-control".into())
            .spawn(move || control_loop(ctrl_config, ctrl_duty, ctrl_running, num_cpus, metrics))
            .expect("failed to spawn CPU control thread");

        CpuController {
            worker_handles,
            control_handle: Some(control_handle),
        }
    }

    /// Wait for all threads to finish (called after setting running=false).
    pub fn join(mut self) {
        if let Some(h) = self.control_handle.take() {
            let _ = h.join();
        }
        for h in self.worker_handles.drain(..) {
            let _ = h.join();
        }
        info!("CPU Sponge stopped");
    }
}

// ── Worker thread ──

fn worker_loop(id: usize, duty: Arc<AtomicU64>, running: Arc<AtomicBool>, cycle_ns: u64) {
    set_sched_idle();
    info!("CPU worker {} started (SCHED_IDLE)", id);

    while running.load(Ordering::Relaxed) {
        let d = f64::from_bits(duty.load(Ordering::Relaxed));
        let work_ns = (cycle_ns as f64 * d) as u64;
        let sleep_ns = cycle_ns.saturating_sub(work_ns);

        if work_ns > 0 {
            busy_work(Duration::from_nanos(work_ns));
        }
        if sleep_ns > 0 {
            thread::sleep(Duration::from_nanos(sleep_ns));
        }
    }
}

/// Burn CPU for the given duration with a tight compute loop.
fn busy_work(duration: Duration) {
    let start = Instant::now();
    let mut acc: u64 = 0;
    loop {
        // Batch iterations to amortise the cost of Instant::now()
        for _ in 0..1000 {
            acc = acc
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
        }
        std::hint::black_box(acc);
        if start.elapsed() >= duration {
            break;
        }
    }
}

/// Set current thread to SCHED_IDLE (priority 5) — lowest scheduling class in Linux.
fn set_sched_idle() {
    #[cfg(target_os = "linux")]
    {
        const SCHED_IDLE: libc::c_int = 5;
        unsafe {
            let param: libc::sched_param = std::mem::zeroed();
            let ret = libc::sched_setscheduler(0, SCHED_IDLE, &param);
            if ret != 0 {
                warn!(
                    "Failed to set SCHED_IDLE: {} (may need CAP_SYS_NICE)",
                    std::io::Error::last_os_error()
                );
            }
        }
    }
}

// ── Control thread ──

fn control_loop(
    config: Config,
    duty: Arc<AtomicU64>,
    running: Arc<AtomicBool>,
    num_cpus: usize,
    metrics: Option<MetricsStore>,
) {
    let mut pid = PidController::new(
        0.8,  // Kp
        0.05, // Ki: slow integral prevents oscillation
        0.3,  // Kd: dampen fast changes
        50.0, // limit: max ±50% per cycle
    );

    // Determine CPU measurement strategy: cgroup accounting (preferred) or /proc/stat fallback
    let use_cgroup = cpu_stat::read_cgroup_cpu_usage().is_some();
    let cgroup_cpus = cpu_stat::detect_num_cpus();

    let mut prev_cg = cpu_stat::read_cgroup_cpu_usage();
    let mut prev_sys = cpu_stat::read_proc_stat().ok();
    let mut prev_proc =
        cpu_stat::read_self_stat().unwrap_or(ProcessCpuSample { utime: 0, stime: 0 });

    if prev_cg.is_none() && prev_sys.is_none() {
        warn!("Cannot read CPU stats (/proc/stat or cgroup), CPU controller disabled");
        return;
    }

    if use_cgroup {
        info!(
            "CPU measurement: cgroup cpu.stat (container-relative, cgroup_cpus={})",
            cgroup_cpus,
        );
    } else {
        let scale = cpu_stat::cgroup_cpu_scale();
        info!(
            "CPU measurement: /proc/stat (scale={:.2}x, host_cpus={}, cgroup_cpus={})",
            scale,
            cpu_stat::count_online_cpus(),
            cgroup_cpus,
        );
    }

    let cycle = Duration::from_millis(config.cpu_cycle);
    let mut cycle_count: u64 = 0;
    let log_interval = (1000 / config.cpu_cycle).max(1);
    let mut last_info_time = Instant::now();

    thread::sleep(cycle); // first sampling interval

    while running.load(Ordering::Relaxed) {
        cycle_count += 1;

        let curr_proc = cpu_stat::read_self_stat().unwrap_or(prev_proc.clone());

        // Calculate CPU usage using the best available source
        let usage = if use_cgroup {
            if let (Some(ref prev), Some(curr)) = (&prev_cg, cpu_stat::read_cgroup_cpu_usage()) {
                let u = cpu_stat::calculate_cgroup_usage(
                    prev,
                    &curr,
                    &prev_proc,
                    &curr_proc,
                    cgroup_cpus,
                );
                prev_cg = Some(curr);
                u
            } else {
                prev_proc = curr_proc;
                thread::sleep(cycle);
                continue;
            }
        } else {
            let scale = cpu_stat::cgroup_cpu_scale();
            if let (Some(ref prev), Ok(curr)) = (&prev_sys, cpu_stat::read_proc_stat()) {
                let raw = cpu_stat::calculate_usage(prev, &curr, &prev_proc, &curr_proc);
                prev_sys = Some(curr);
                CpuUsage {
                    total_pct: (raw.total_pct * scale).min(100.0),
                    self_pct: (raw.self_pct * scale).min(100.0),
                    others_pct: ((raw.total_pct - raw.self_pct).max(0.0) * scale).min(100.0),
                }
            } else {
                prev_proc = curr_proc;
                thread::sleep(cycle);
                continue;
            }
        };

        let current_duty = f64::from_bits(duty.load(Ordering::Relaxed));
        let should_debug_log = cycle_count.is_multiple_of(log_interval);
        let should_info_log = last_info_time.elapsed() >= Duration::from_secs(1);

        if should_debug_log {
            debug!(
                "[CPU #{:>5}] ── Status: total={:.1}%, self={:.1}%, others={:.1}% ({}) | duty={:.1}% | workers={}",
                cycle_count, usage.total_pct, usage.self_pct, usage.others_pct,
                if use_cgroup { "cgroup" } else { "procstat" },
                current_duty * 100.0, num_cpus,
            );
        }

        // ── Panic check: others already near/above target → full yield ──
        let panic_line = config.cpu_target - config.cpu_panic_margin;
        let action_label;
        if usage.others_pct > panic_line {
            duty.store(0.0f64.to_bits(), Ordering::Relaxed);
            pid.reset_integral();
            action_label = "YIELD";
            if should_debug_log {
                warn!(
                    "[CPU #{:>5}]    ⚠ YIELD: others={:.1}% > panic_line={:.1}%, duty → 0%",
                    cycle_count, usage.others_pct, panic_line,
                );
            }
        } else {
            // ── PID control ──
            let error = config.cpu_target - usage.total_pct;
            let pid_out = pid.update(error, current_duty <= 0.01);

            let duty_delta = pid_out.clamped_output / 100.0;
            let new_duty = (current_duty + duty_delta).clamp(0.0, 1.0);
            duty.store(new_duty.to_bits(), Ordering::Relaxed);
            action_label = "PID";

            if should_debug_log {
                debug!(
                    "[CPU #{:>5}]    PID: {} | duty_delta={:+.3} → new_duty={:.1}%",
                    cycle_count,
                    pid_out,
                    duty_delta,
                    new_duty * 100.0,
                );
            }
        }

        // ── Aggregated info log (at most once per second) ──
        if should_info_log {
            let d = f64::from_bits(duty.load(Ordering::Relaxed));
            info!(
                "[CPU #{:>5}] total={:.1}% self={:.1}% others={:.1}% target={:.1}% duty={:.1}% action={}",
                cycle_count, usage.total_pct, usage.self_pct, usage.others_pct,
                config.cpu_target, d * 100.0, action_label,
            );
            last_info_time = Instant::now();
        }

        prev_proc = curr_proc;

        // Publish CPU metrics
        if let Some(ref m) = metrics {
            let d = f64::from_bits(duty.load(Ordering::Relaxed));
            m.update_cpu(metrics::CpuMetrics {
                total_pct: usage.total_pct,
                others_pct: usage.others_pct,
                self_pct: usage.self_pct,
                duty_cycle: d,
                workers: num_cpus,
            });
        }

        thread::sleep(cycle);
    }

    // Idle all workers on shutdown
    duty.store(0.0f64.to_bits(), Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_duty_atomic_roundtrip() {
        let duty = Arc::new(AtomicU64::new(0.0f64.to_bits()));
        duty.store(0.75f64.to_bits(), Ordering::Relaxed);
        let val = f64::from_bits(duty.load(Ordering::Relaxed));
        assert!((val - 0.75).abs() < f64::EPSILON);
    }

    #[test]
    fn test_duty_zero() {
        let duty = Arc::new(AtomicU64::new(0.0f64.to_bits()));
        assert_eq!(f64::from_bits(duty.load(Ordering::Relaxed)), 0.0);
    }

    #[test]
    fn test_duty_one() {
        let duty = Arc::new(AtomicU64::new(1.0f64.to_bits()));
        assert_eq!(f64::from_bits(duty.load(Ordering::Relaxed)), 1.0);
    }

    #[test]
    fn test_duty_small_value() {
        let duty = Arc::new(AtomicU64::new(0.001f64.to_bits()));
        let val = f64::from_bits(duty.load(Ordering::Relaxed));
        assert!((val - 0.001).abs() < f64::EPSILON);
    }

    #[test]
    fn test_busy_work_runs_at_least_target_duration() {
        let target = Duration::from_millis(20);
        let start = Instant::now();
        busy_work(target);
        assert!(start.elapsed() >= target);
    }

    #[test]
    fn test_busy_work_zero_duration() {
        let start = Instant::now();
        busy_work(Duration::ZERO);
        assert!(start.elapsed() < Duration::from_millis(50));
    }
}
