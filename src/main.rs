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
use log::{error, info};

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
    log_capture::CaptureLogger::init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Run(config) => run_sponge(config),
        Commands::Install { config, bin_path, start } => {
            if let Err(e) = config.validate() {
                error!("配置校验失败: {}", e);
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

fn run_sponge(config: Config) {
    if let Err(e) = config.validate() {
        error!("Invalid configuration: {}", e);
        std::process::exit(1);
    }

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
            error!("Cannot read /proc/meminfo: {}. Are you running on Linux?", e);
            std::process::exit(1);
        }
    }

    // Set up graceful shutdown via signal handler
    let running = Arc::new(AtomicBool::new(true));
    {
        let r = running.clone();
        RUNNING_FLAG.lock().unwrap().replace(r);
    }
    unsafe {
        libc::signal(libc::SIGINT, signal_handler as libc::sighandler_t);
        libc::signal(libc::SIGTERM, signal_handler as libc::sighandler_t);
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

    // Memory control loop (main thread)
    let mut ctrl = Controller::new(config, Some(metrics_store));

    while running.load(Ordering::Relaxed) {
        if !ctrl.tick() {
            break;
        }
        let sleep_ms = ctrl.sleep_interval_ms();
        thread::sleep(Duration::from_millis(sleep_ms));
    }

    // Join CPU threads
    if let Some(cpu) = cpu_ctrl {
        cpu.join();
    }

    info!("=== Server Sponge shutting down ===");
}

static RUNNING_FLAG: std::sync::Mutex<Option<Arc<AtomicBool>>> = std::sync::Mutex::new(None);

extern "C" fn signal_handler(_sig: libc::c_int) {
    if let Ok(guard) = RUNNING_FLAG.lock() {
        if let Some(flag) = guard.as_ref() {
            flag.store(false, Ordering::Relaxed);
        }
    }
}
