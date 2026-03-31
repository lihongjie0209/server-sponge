use log::{info, warn};
use std::time::Instant;

use crate::config::Config;
use crate::memory::MemoryPool;
use crate::metrics::{self, MetricsStore};
use crate::pid::PidController;
use crate::psi::{PressureLevel, PsiMonitor};
use crate::sysinfo;

/// Operating mode of the controller state machine
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Normal operation: PID makes small adjustments
    Steady,
    /// Moderate pressure detected: stop allocating, release proportionally
    Responsive,
    /// Critical pressure: emergency release, enter cooldown
    Panic,
    /// Post-panic cooldown: no allocations allowed
    Cooldown,
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Mode::Steady => write!(f, "STEADY"),
            Mode::Responsive => write!(f, "RESPONSIVE"),
            Mode::Panic => write!(f, "PANIC"),
            Mode::Cooldown => write!(f, "COOLDOWN"),
        }
    }
}

pub struct Controller {
    config: Config,
    pool: MemoryPool,
    pid: PidController,
    psi: PsiMonitor,
    mode: Mode,
    cooldown_start: Option<Instant>,
    cycle_count: u64,
    metrics: Option<MetricsStore>,
    last_pid_output: f64,
    last_pid_p: f64,
    last_pid_i: f64,
    last_pid_d: f64,
}

impl Controller {
    pub fn new(config: Config, metrics: Option<MetricsStore>) -> Self {
        let pool = MemoryPool::new(config.chunk_size_bytes());
        let pid = PidController::new(config.kp, config.ki, config.kd, 100.0);

        let psi = if config.no_psi {
            warn!("PSI monitoring disabled by user");
            PsiMonitor::new(0, 0) // Will gracefully degrade
        } else {
            // Trigger when stalled > 70ms in any 1s window
            PsiMonitor::new(70_000, 1_000_000)
        };

        Self {
            config,
            pool,
            pid,
            psi,
            mode: Mode::Steady,
            cooldown_start: None,
            cycle_count: 0,
            metrics,
            last_pid_output: 0.0,
            last_pid_p: 0.0,
            last_pid_i: 0.0,
            last_pid_d: 0.0,
        }
    }

    /// Run one control cycle. Returns false if the program should exit.
    pub fn tick(&mut self) -> bool {
        self.cycle_count += 1;

        // ── Step 1: Read system memory state ──
        let mem = match sysinfo::get_memory_info() {
            Ok(m) => m,
            Err(e) => {
                warn!("Failed to read meminfo: {}, releasing all memory", e);
                self.pool.release_all();
                return true;
            }
        };

        let usage = mem.usage_percent();
        let available = mem.available_percent();

        info!(
            "[#{:>5}] ── Memory status: total={} MB, used={} MB, avail={} MB (usage={:.1}%, avail={:.1}%) | pool={} chunks ({} MB) | source={}",
            self.cycle_count,
            mem.total / (1024 * 1024),
            (mem.total - mem.available) / (1024 * 1024),
            mem.available / (1024 * 1024),
            usage,
            available,
            self.pool.len(),
            self.pool.total_bytes() / (1024 * 1024),
            if mem.is_cgroup { "cgroup" } else { "/proc/meminfo" },
        );

        // ── Step 2: Check PSI pressure ──
        let pressure = if !self.config.no_psi && self.psi.is_available() {
            let p = self.psi.poll(0);
            info!(
                "[#{:>5}]    PSI probe: level={:?}",
                self.cycle_count, p
            );
            p
        } else {
            PressureLevel::None
        };

        // ── Step 3: Determine mode ──
        let prev_mode = self.mode;
        self.mode = self.determine_mode(available, pressure);

        if self.mode != prev_mode {
            info!(
                "[#{:>5}]    Mode transition: {} -> {} (reason: {})",
                self.cycle_count,
                prev_mode,
                self.mode,
                self.mode_transition_reason(available, pressure),
            );
            if self.mode == Mode::Steady {
                info!(
                    "[#{:>5}]    Resetting PID integral accumulator (entering steady mode)",
                    self.cycle_count
                );
                self.pid.reset_integral();
            }
        } else {
            info!(
                "[#{:>5}]    Mode: {} (unchanged)",
                self.cycle_count, self.mode
            );
        }

        // ── Step 4: Execute action based on mode ──
        match self.mode {
            Mode::Steady => self.action_steady(usage),
            Mode::Responsive => self.action_responsive(),
            Mode::Panic => self.action_panic(),
            Mode::Cooldown => self.action_cooldown(),
        }

        // ── Step 5: Swap safety check ──
        if mem.swap_in_use() && mem.swap_usage_percent() > 10.0 {
            warn!(
                "[#{:>5}]    Swap safety: swap usage at {:.1}% (>{:.0}% threshold), proactively releasing 30% of pool",
                self.cycle_count,
                mem.swap_usage_percent(),
                10.0
            );
            let released = self.pool.release_fraction(0.3);
            info!(
                "[#{:>5}]    Swap safety: released {} chunks, pool now {} chunks ({} MB)",
                self.cycle_count,
                released,
                self.pool.len(),
                self.pool.total_bytes() / (1024 * 1024),
            );
        }

        info!(
            "[#{:>5}] ── Cycle complete. Next check in {} ms ──",
            self.cycle_count,
            self.sleep_interval_ms(),
        );

        // Publish metrics to monitoring store
        if let Some(ref m) = self.metrics {
            m.update_memory(metrics::MemoryMetrics {
                total_mb: mem.total as f64 / (1024.0 * 1024.0),
                used_mb: (mem.total - mem.available) as f64 / (1024.0 * 1024.0),
                available_mb: mem.available as f64 / (1024.0 * 1024.0),
                usage_pct: usage,
                pool_chunks: self.pool.len(),
                pool_mb: self.pool.total_bytes() as f64 / (1024.0 * 1024.0),
                mode: format!("{}", self.mode),
                pid_output: self.last_pid_output,
                pid_p: self.last_pid_p,
                pid_i: self.last_pid_i,
                pid_d: self.last_pid_d,
            });
        }

        true
    }

    fn mode_transition_reason(&self, available_pct: f64, pressure: PressureLevel) -> String {
        if pressure == PressureLevel::Full {
            return format!("PSI full stall detected");
        }
        if available_pct < self.config.panic_threshold {
            return format!(
                "available memory {:.1}% < panic threshold {:.1}%",
                available_pct, self.config.panic_threshold
            );
        }
        if pressure == PressureLevel::Some {
            return format!("PSI some stall detected");
        }
        if let Some(start) = self.cooldown_start {
            let elapsed = start.elapsed().as_secs();
            if elapsed < self.config.cooldown {
                return format!(
                    "cooldown active ({}/{}s elapsed)",
                    elapsed, self.config.cooldown
                );
            } else {
                return format!("cooldown expired after {}s", elapsed);
            }
        }
        "normal operating conditions".to_string()
    }

    fn determine_mode(&self, available_pct: f64, pressure: PressureLevel) -> Mode {
        // Check cooldown expiry
        if self.mode == Mode::Cooldown {
            if let Some(start) = self.cooldown_start {
                if start.elapsed().as_secs() < self.config.cooldown {
                    return Mode::Cooldown;
                }
            }
        }

        // Panic conditions
        if pressure == PressureLevel::Full || available_pct < self.config.panic_threshold {
            return Mode::Panic;
        }

        // Responsive conditions
        if pressure == PressureLevel::Some {
            return Mode::Responsive;
        }

        Mode::Steady
    }

    fn action_steady(&mut self, current_usage: f64) {
        let error = self.config.target - current_usage;
        let pid_out = self.pid.update(error, self.pool.is_empty());

        // Store PID values for metrics
        self.last_pid_output = pid_out.clamped_output;
        self.last_pid_p = pid_out.p_term;
        self.last_pid_i = pid_out.i_term;
        self.last_pid_d = pid_out.d_term;

        info!(
            "[#{:>5}]    PID compute: target={:.1}% - current={:.1}% = {}",
            self.cycle_count, self.config.target, current_usage, pid_out
        );

        // Convert PID output to chunk count
        let mem = crate::sysinfo::get_memory_info().ok();
        let pct_per_chunk = mem
            .map(|m| (self.config.chunk_size_bytes() as f64 / m.total as f64) * 100.0)
            .unwrap_or(5.0);
        let chunks_f = pid_out.clamped_output / pct_per_chunk.max(0.1);
        let chunks = chunks_f.round() as i64;

        info!(
            "[#{:>5}]    Chunk calc: pid_output={:+.2} / pct_per_chunk={:.2}% = {:.2} chunks (rounded={})",
            self.cycle_count, pid_out.clamped_output, pct_per_chunk, chunks_f, chunks
        );

        if chunks > 0 {
            let max_alloc = (error / pct_per_chunk.max(0.1)).ceil().max(0.0) as usize;
            let n = (chunks as usize).min(max_alloc).min(5).max(1);
            info!(
                "[#{:>5}]    Decision: ALLOCATE {} chunks (wanted={}, max_to_target={}, cap=5) | reason: usage {:.1}% below target {:.1}%",
                self.cycle_count, n, chunks, max_alloc, current_usage, self.config.target
            );
            self.pool.allocate_chunks(n);
            info!(
                "[#{:>5}]    Pool after: {} chunks ({} MB)",
                self.cycle_count,
                self.pool.len(),
                self.pool.total_bytes() / (1024 * 1024),
            );
        } else if chunks < 0 {
            let n = ((-chunks) as usize).min(self.pool.len()).min(self.pool.len() / 2 + 1);
            if n > 0 {
                info!(
                    "[#{:>5}]    Decision: RELEASE {} chunks (wanted={}, pool_cap={}) | reason: usage {:.1}% above target {:.1}%",
                    self.cycle_count, n, -chunks, self.pool.len() / 2 + 1, current_usage, self.config.target
                );
                self.pool.release_chunks(n);
                info!(
                    "[#{:>5}]    Pool after: {} chunks ({} MB)",
                    self.cycle_count,
                    self.pool.len(),
                    self.pool.total_bytes() / (1024 * 1024),
                );
            }
        } else {
            info!(
                "[#{:>5}]    Decision: HOLD (no change) | reason: usage {:.1}% ≈ target {:.1}% (within tolerance)",
                self.cycle_count, current_usage, self.config.target
            );
        }
    }

    fn action_responsive(&mut self) {
        if !self.pool.is_empty() {
            let before = self.pool.len();
            let released = self.pool.release_fraction(0.2);
            info!(
                "[#{:>5}]    Decision: RESPONSIVE RELEASE 20% = {} chunks (pool: {} -> {}) | reason: PSI 'some' pressure detected",
                self.cycle_count, released, before, self.pool.len()
            );
        } else {
            info!(
                "[#{:>5}]    Decision: HOLD (pool empty, nothing to release) | mode=RESPONSIVE",
                self.cycle_count
            );
        }
        self.pid.reset_integral();
        info!(
            "[#{:>5}]    PID integral reset (responsive mode)",
            self.cycle_count
        );
    }

    fn action_panic(&mut self) {
        if !self.pool.is_empty() {
            let before = self.pool.len();
            warn!(
                "[#{:>5}]    ⚠ PANIC: Emergency memory release! Pool has {} chunks ({} MB)",
                self.cycle_count, before, self.pool.total_bytes() / (1024 * 1024)
            );
            let released_80 = self.pool.release_fraction(0.8);
            warn!(
                "[#{:>5}]    PANIC phase 1: released 80% = {} chunks, remaining {} chunks",
                self.cycle_count, released_80, self.pool.len()
            );
            if self.pool.len() > 0 {
                let remaining = self.pool.release_all();
                warn!(
                    "[#{:>5}]    PANIC phase 2: released remaining {} chunks (pool now empty)",
                    self.cycle_count, remaining
                );
            }
        } else {
            info!(
                "[#{:>5}]    PANIC: pool already empty, nothing to release",
                self.cycle_count
            );
        }
        self.pid.reset();
        self.cooldown_start = Some(Instant::now());
        self.mode = Mode::Cooldown;
        info!(
            "[#{:>5}]    Entering COOLDOWN for {}s (PID fully reset)",
            self.cycle_count, self.config.cooldown
        );
    }

    fn action_cooldown(&mut self) {
        let elapsed = self.cooldown_start.map(|s| s.elapsed().as_secs()).unwrap_or(0);
        info!(
            "[#{:>5}]    Decision: HOLD (cooldown {}/{}s) | reason: no allocations allowed during cooldown",
            self.cycle_count, elapsed, self.config.cooldown
        );
    }

    /// Get the current sleep interval based on mode
    pub fn sleep_interval_ms(&self) -> u64 {
        match self.mode {
            Mode::Steady | Mode::Cooldown => self.config.interval,
            Mode::Responsive => self.config.interval / 10, // 10x faster in responsive mode
            Mode::Panic => 50, // Minimal delay in panic
        }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn pool_size(&self) -> usize {
        self.pool.len()
    }
}
