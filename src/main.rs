mod config;
mod controller;
mod cpu_stat;
mod cpu_worker;
mod install;
mod log_capture;
mod memory;
mod metrics;
mod pid;
mod psi;
mod server;
mod stress;
mod sysinfo;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use clap::{Parser, Subcommand};
use log::{debug, error, info, warn};

use config::Config;
use controller::Controller;

#[derive(Parser)]
#[command(
    name = "server-sponge",
    about = "动态资源占位与自动避险系统 (Dynamic Resource Sponge)",
    long_about = "基于 PID 控制与 PSI/SCHED_IDLE 的动态资源占位系统。\n支持内存占位（PID + PSI 监听）与 CPU 占位（PID + PWM 占空比 + SCHED_IDLE 调度）。",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// 运行 Server Sponge（直接前台执行）
    Run(Config),

    /// 安装为 systemd 系统服务（需要 root 权限）
    Install {
        #[command(flatten)]
        config: Config,

        /// 可执行文件安装路径
        #[arg(long, default_value = "/usr/local/bin/server-sponge")]
        bin_path: String,

        /// 安装后立即启动服务
        #[arg(long)]
        start: bool,
    },

    /// 卸载 systemd 服务（需要 root 权限）
    Uninstall,
}

fn main() {
    let cli = Cli::parse();

    // Initialize logger based on command
    match &cli.command {
        Commands::Run(config) => {
            // Dry-run: stderr only, no file logging
            let log_dir = if config.dry_run {
                "".into()
            } else {
                config.log_dir.clone()
            };
            log_capture::init(&log_capture::LogConfig {
                log_dir,
                log_retention_days: config.log_retention,
                log_compress: config.log_compress,
            });
        }
        _ => {
            // Install/Uninstall: stderr-only logging
            log_capture::init(&log_capture::LogConfig {
                log_dir: "".into(),
                log_retention_days: 7,
                log_compress: false,
            });
        }
    }

    match cli.command {
        Commands::Run(mut config) => {
            // If --config was specified, load file as base then apply CLI overrides
            if let Some(ref config_path) = config.config.clone() {
                match std::fs::read_to_string(config_path) {
                    Ok(content) => {
                        match toml::from_str::<Config>(&content) {
                            Ok(file_config) => {
                                // Start with file values, then apply CLI overrides on top
                                let mut merged = file_config;
                                merged.config = config.config.clone();
                                merged.apply_cli_overrides(&config);
                                config = merged;
                                // Re-validate with merged config
                                if let Err(e) = config.validate() {
                                    error!("Configuration invalid: {}", e);
                                    std::process::exit(1);
                                }
                            }
                            Err(e) => {
                                error!("Failed to parse config file '{}': {}", config_path, e);
                                std::process::exit(1);
                            }
                        }
                    }
                    Err(e) => {
                        error!("Cannot read config file '{}': {}", config_path, e);
                        std::process::exit(1);
                    }
                }
            }
            run_sponge(config);
        }
        Commands::Install {
            config,
            bin_path,
            start,
        } => {
            if let Err(e) = config.validate() {
                eprintln!("配置校验失败: {}", e);
                std::process::exit(1);
            }
            if let Err(e) = install::install(&config, &bin_path, start) {
                error!("安装失败: {}", e);
                std::process::exit(1);
            }
        }
        Commands::Uninstall => {
            if let Err(e) = install::uninstall() {
                error!("卸载失败: {}", e);
                std::process::exit(1);
            }
        }
    }
}

// prctl PR_SET_MM constants (not yet in libc's Linux module)
const PR_SET_MM: libc::c_int = 35;
const PR_SET_MM_ARG_START: libc::c_int = 8;
const PR_SET_MM_ARG_END: libc::c_int = 9;

/// Apply stealth measures: change process name and command line visible in /proc.
///
/// Techniques used:
///   1. `prctl(PR_SET_NAME, name)` — changes `/proc/self/comm` (what `ps`/`top` show).
///      Works for any process — no privileges needed.
///   2. `prctl(PR_SET_MM, PR_SET_MM_ARG_START/END, ...)` — changes `/proc/self/cmdline`.
///      Requires `CAP_SYS_RESOURCE` (typically root). Silently skipped when unavailable.
///
/// Limitations:
///   - `/proc/self/exe` cannot be faked from user space.
///     For full hiding, rename the binary or use `mount --bind`.
///   - Without root, only the process name (comm) is changed.
///     Use `exec -a <fake_name> ./server-sponge` at launch for full cmdline spoofing.
fn stealth_init(name: &str, cmdline: &str) {
    // ── 1. Change process name (comm) — limited to 15 chars ──
    let comm_name = comm_name_bytes(name);
    unsafe {
        libc::prctl(
            libc::PR_SET_NAME,
            comm_name.as_ptr() as *const libc::c_void,
            0,
            0,
            0,
        );
    }

    // ── 2. Try to replace /proc/self/cmdline via prctl (requires CAP_SYS_RESOURCE) ──
    // Build a NUL-separated fake cmdline buffer: "cmd\0arg1\0arg2\0\0"
    let mut fake_buf = Vec::with_capacity(cmdline.len() + 2);
    for part in cmdline.split(' ') {
        if !fake_buf.is_empty() {
            fake_buf.push(0); // NUL separator between args
        }
        fake_buf.extend_from_slice(part.as_bytes());
    }
    fake_buf.push(0); // trailing NUL
    fake_buf.push(0); // double NUL = end of argv

    // pin the buffer to a stable heap address
    let buf = fake_buf.into_boxed_slice();
    let start = buf.as_ptr() as usize;
    let end = start + buf.len();

    unsafe {
        let ret = libc::prctl(PR_SET_MM, PR_SET_MM_ARG_START, start, 0, 0);
        if ret == 0 {
            libc::prctl(PR_SET_MM, PR_SET_MM_ARG_END, end, 0, 0);
            // Prevent the buffer from being freed — keep it alive for the kernel
            std::mem::forget(buf);
            info!("Stealth: full cmdline spoofed (CAP_SYS_RESOURCE available)");
        } else {
            debug!("Stealth: cmdline overwrite requires CAP_SYS_RESOURCE, only comm changed");
        }
    }

    info!("Stealth: name='{}' cmdline='{}'", name, cmdline);
}

/// Set the process to the lowest resource priority to protect other services:
///   1. OOM score +800 → kernel prefers to kill us before business services
///   2. Nice +10 → lower CPU scheduling priority
fn lower_process_priority() {
    // OOM score: positive = more likely to be killed by OOM killer
    match std::fs::write("/proc/self/oom_score_adj", b"800\n") {
        Ok(_) => debug!("OOM score set to +800"),
        Err(e) => warn!(
            "Cannot set OOM score: {} (run as root for OOM protection)",
            e
        ),
    }

    // Nice value: higher = lower CPU priority
    unsafe {
        let ret = libc::setpriority(libc::PRIO_PROCESS, 0, 10);
        if ret == 0 {
            debug!("Process nice value set to 10");
        } else {
            warn!(
                "Cannot set nice value: {} (may need CAP_SYS_NICE)",
                std::io::Error::last_os_error()
            );
        }
    }

    info!("Process priority lowered: OOM score +800, nice 10");
}

/// Print a detailed dry-run plan showing what Server Sponge would do.
fn print_dry_run_plan(config: &Config) {
    use std::io::Write;

    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    let _ = writeln!(out, "\n═══════════════════════════════════════════");
    let _ = writeln!(out, "  Server Sponge — DRY RUN MODE");
    let _ = writeln!(out, "  No system resources will be touched.");
    let _ = writeln!(out, "═══════════════════════════════════════════\n");

    // Memory section
    let _ = writeln!(out, "📦 MEMORY");
    if config.target > 0.0 {
        let mem = crate::sysinfo::get_memory_info().ok();
        let chunk_mb = config.chunk_size_bytes() / (1024 * 1024);
        let total_mb = mem.as_ref().map(|m| m.total / (1024 * 1024));
        let est_chunks = total_mb.map(|t| {
            let target_bytes = (t as f64 * config.target / 100.0) as usize;
            (target_bytes / config.chunk_size_bytes()).max(1)
        });

        let _ = writeln!(out, "  Target usage : {}%", config.target);
        let _ = writeln!(out, "  Chunk size   : {} MB", chunk_mb);
        if let (Some(t), Some(c)) = (total_mb, est_chunks) {
            let target_mb = (t as f64 * config.target / 100.0) as u64;
            let actual_mb = c as u64 * chunk_mb as u64;
            let _ = writeln!(out, "  System memory: {} MB total", t);
            let _ = writeln!(
                out,
                "  Would hold   : ~{} chunks ≈ {} MB (target ~{} MB)",
                c, actual_mb, target_mb
            );
        }
        let _ = writeln!(
            out,
            "  PID params   : Kp={} Ki={} Kd={}",
            config.kp, config.ki, config.kd
        );
        let _ = writeln!(
            out,
            "  PSI          : {}",
            if config.no_psi { "disabled" } else { "enabled" }
        );
        let _ = writeln!(
            out,
            "  Panic        : {}% available memory threshold",
            config.panic_threshold
        );
        let _ = writeln!(out, "  Cooldown     : {}s after panic", config.cooldown);
        let _ = writeln!(
            out,
            "  HugePages    : {}",
            if config.hugepages {
                "enabled"
            } else {
                "disabled"
            }
        );
    } else {
        let _ = writeln!(out, "  Disabled (--target 0)");
    }

    // CPU section
    let _ = writeln!(out, "\n⚙️  CPU");
    if config.cpu_target > 0.0 {
        let num_workers = if config.cpu_workers > 0 {
            config.cpu_workers
        } else {
            crate::cpu_stat::detect_num_cpus()
        };
        let _ = writeln!(out, "  Target       : {}%", config.cpu_target);
        let _ = writeln!(out, "  Workers      : {} (SCHED_IDLE)", num_workers);
        let _ = writeln!(out, "  Cycle        : {} ms", config.cpu_cycle);
        let _ = writeln!(out, "  Panic margin : {}%", config.cpu_panic_margin);
    } else {
        let _ = writeln!(out, "  Disabled (--cpu-target 0)");
    }

    // Server section
    let _ = writeln!(out, "\n🌐 WEB SERVER");
    if config.server_port > 0 {
        let _ = writeln!(out, "  Enabled on port : {}", config.server_port);
    } else {
        let _ = writeln!(out, "  Disabled");
    }

    // Logging
    let _ = writeln!(out, "\n📝 LOGGING");
    if config.log_dir.is_empty() {
        let _ = writeln!(out, "  Output    : stderr only");
    } else {
        let _ = writeln!(out, "  Directory : {}", config.log_dir);
        let _ = writeln!(out, "  Retention : {} days", config.log_retention);
        let _ = writeln!(out, "  Compress  : {}", config.log_compress);
    }

    let _ = writeln!(out, "\n🛡️  PROCESS PRIORITY");
    let _ = writeln!(out, "  OOM score adj : +800 (preferred OOM kill target)");
    let _ = writeln!(out, "  Nice value    : 10 (low CPU priority)");
    if config.cpu_target > 0.0 {
        let _ = writeln!(out, "  CPU workers   : SCHED_IDLE scheduling");
    }

    let _ = writeln!(out, "\n═══════════════════════════════════════════");
    let _ = writeln!(out, "  To execute: server-sponge run [options]");
    let _ = writeln!(out, "═══════════════════════════════════════════\n");
}

fn run_sponge(mut config: Config) {
    if let Err(e) = config.validate() {
        error!("Invalid configuration: {}", e);
        std::process::exit(1);
    }

    // Stealth mode: hide process identity before any monitoring tool reads it
    if config.stealth_name.is_some() || config.stealth_cmdline.is_some() {
        stealth_init(
            config.stealth_name.as_deref().unwrap_or("kworker"),
            config.stealth_cmdline.as_deref().unwrap_or("/sbin/init"),
        );
    }

    // Auto chunk size: if user didn't explicitly set chunk_size (=default 64),
    // calculate based on system memory for better granularity on small systems.
    // This runs before dry-run so the plan shows the actual size that would be used.
    if config.chunk_size == 64 {
        if let Ok(mem) = crate::sysinfo::get_memory_info() {
            let total_mb = mem.total / (1024 * 1024);
            let auto = crate::memory::auto_chunk_size_mb(total_mb);
            if auto != config.chunk_size {
                config.chunk_size = auto;
            }
        }
    }

    // Dry-run: print plan and exit without touching system resources
    if config.dry_run {
        print_dry_run_plan(&config);
        return;
    }

    // Log the resolved chunk size
    info!("Using chunk size: {} MB", config.chunk_size);

    // Lower resource priority to protect other services on the system
    lower_process_priority();

    info!("=== Server Sponge starting ===");
    info!(
        "Memory: target={:.0}% | Chunk={} MB | Panic={:.0}% | Cooldown={}s | PSI={}",
        config.target,
        config.chunk_size,
        config.panic_threshold,
        config.cooldown,
        if config.no_psi { "off" } else { "on" }
    );
    if config.cpu_target > 0.0 {
        info!(
            "CPU: target={:.0}% | Cycle={}ms | Margin={:.0}% | Workers={}",
            config.cpu_target,
            config.cpu_cycle,
            config.cpu_panic_margin,
            if config.cpu_workers > 0 {
                format!("{}", config.cpu_workers)
            } else {
                "auto".into()
            }
        );
    } else {
        info!("CPU: disabled (set --cpu-target to enable)");
    }
    if config.server_port > 0 {
        info!("Server: port={}", config.server_port);
    } else {
        info!("Server: disabled (set --server-port to enable)");
    }

    // Print system memory info
    match sysinfo::get_memory_info() {
        Ok(mem) => {
            info!(
                "System memory: total={} MB, available={} MB, usage={:.1}%",
                mem.total / (1024 * 1024),
                mem.available / (1024 * 1024),
                mem.usage_percent()
            );
        }
        Err(e) => {
            error!(
                "Cannot read /proc/meminfo: {}. Are you running on Linux?",
                e
            );
            std::process::exit(1);
        }
    }

    // Set up graceful shutdown via signal handler
    let running = Arc::new(AtomicBool::new(true));
    SIGNAL_RECEIVED.store(true, Ordering::Relaxed);
    unsafe {
        libc::signal(
            libc::SIGINT,
            signal_handler as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGTERM,
            signal_handler as *const () as libc::sighandler_t,
        );
    }

    // Create shared metrics store
    let metrics_store = metrics::MetricsStore::new(config.target, config.cpu_target);

    // Start monitoring server if enabled
    let _server = if config.server_port > 0 {
        Some(server::start_server(
            config.server_port,
            metrics_store.clone(),
            running.clone(),
        ))
    } else {
        None
    };

    // Start CPU controller (background threads) if enabled
    let cpu_ctrl = if config.cpu_target > 0.0 {
        Some(cpu_worker::CpuController::start(
            &config,
            running.clone(),
            Some(metrics_store.clone()),
        ))
    } else {
        None
    };

    // Memory control loop (main thread) — skip entirely when target=0
    if config.target > 0.0 {
        let mut ctrl = Controller::new(config, Some(metrics_store));
        while is_running(&running) {
            ctrl.tick();
            thread::sleep(Duration::from_millis(ctrl.sleep_interval_ms()));
        }
    } else {
        info!("Memory sponge disabled (--target=0), waiting for shutdown signal");
        while is_running(&running) {
            thread::sleep(Duration::from_secs(1));
        }
    }

    running.store(false, Ordering::Relaxed);

    // Join CPU threads
    if let Some(cpu) = cpu_ctrl {
        cpu.join();
    }

    info!("=== Server Sponge shutting down ===");
}

static SIGNAL_RECEIVED: AtomicBool = AtomicBool::new(true);

fn is_running(running: &AtomicBool) -> bool {
    running.load(Ordering::Relaxed) && SIGNAL_RECEIVED.load(Ordering::Relaxed)
}

fn comm_name_bytes(name: &str) -> Vec<u8> {
    let sanitized = name.replace('\0', "");
    let mut end = sanitized.len().min(15);
    while !sanitized.is_char_boundary(end) {
        end -= 1;
    }
    let mut bytes = sanitized.as_bytes()[..end].to_vec();
    bytes.push(0);
    bytes
}

extern "C" fn signal_handler(_sig: libc::c_int) {
    SIGNAL_RECEIVED.store(false, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::comm_name_bytes;

    #[test]
    fn comm_name_truncation_preserves_utf8_boundaries() {
        let bytes = comm_name_bytes("中文进程名称");
        assert!(bytes.len() <= 16);
        assert_eq!(bytes.last(), Some(&0));
        assert!(std::str::from_utf8(&bytes[..bytes.len() - 1]).is_ok());
    }
}
