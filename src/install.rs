use std::fs;
use std::io;
use std::path::Path;
use std::process::Command;

use crate::config::Config;

const SERVICE_NAME: &str = "server-sponge";
const SERVICE_PATH: &str = "/etc/systemd/system/server-sponge.service";
const DEFAULT_BIN_PATH: &str = "/usr/local/bin/server-sponge";

/// Install server-sponge as a systemd service
pub fn install(config: &Config, bin_path: &str, start: bool) -> io::Result<()> {
    // Must run as root
    if !is_root() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "install command must be run as root (use sudo)",
        ));
    }

    // 1. Copy binary to target path
    let current_exe = std::env::current_exe()?;
    let target = if bin_path.is_empty() { DEFAULT_BIN_PATH } else { bin_path };

    // Skip copy if source and destination are the same file
    let src_canonical = fs::canonicalize(&current_exe)?;
    let dst_exists = Path::new(target).exists();
    let same_file = dst_exists && fs::canonicalize(target).map(|d| d == src_canonical).unwrap_or(false);

    if same_file {
        println!("📦 Binary already at {}, skipping copy", target);
    } else {
        if let Some(parent) = Path::new(target).parent() {
            fs::create_dir_all(parent)?;
        }
        println!("📦 Copying binary: {} -> {}", current_exe.display(), target);
        fs::copy(&current_exe, target)?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(target, fs::Permissions::from_mode(0o755))?;
        }
    }

    // 2. Generate and write service file
    let service_content = generate_service_file(config, target);
    println!("📝 Writing service file: {}", SERVICE_PATH);
    fs::write(SERVICE_PATH, &service_content)?;
    println!("   Service configuration:");
    for line in service_content.lines() {
        if line.starts_with("ExecStart=") {
            println!("   {}", line);
        }
    }

    // 3. Reload systemd daemon
    println!("🔄 Reloading systemd daemon...");
    run_cmd("systemctl", &["daemon-reload"])?;

    // 4. Enable service
    println!("✅ Enabling service: {}", SERVICE_NAME);
    run_cmd("systemctl", &["enable", SERVICE_NAME])?;

    // 5. Optionally start
    if start {
        println!("🚀 Starting service: {}", SERVICE_NAME);
        run_cmd("systemctl", &["start", SERVICE_NAME])?;
        println!("\n服务已安装并启动。查看状态：");
    } else {
        println!("\n服务已安装但未启动。使用以下命令管理：");
    }

    println!("  systemctl status {}", SERVICE_NAME);
    println!("  systemctl start  {}", SERVICE_NAME);
    println!("  systemctl stop   {}", SERVICE_NAME);
    println!("  journalctl -u {} -f", SERVICE_NAME);

    Ok(())
}

/// Uninstall the systemd service
pub fn uninstall() -> io::Result<()> {
    if !is_root() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "uninstall command must be run as root (use sudo)",
        ));
    }

    // Stop if running (ignore errors — may not be running)
    println!("🛑 Stopping service...");
    let _ = run_cmd("systemctl", &["stop", SERVICE_NAME]);

    println!("🔕 Disabling service...");
    let _ = run_cmd("systemctl", &["disable", SERVICE_NAME]);

    // Remove service file
    if Path::new(SERVICE_PATH).exists() {
        fs::remove_file(SERVICE_PATH)?;
        println!("🗑  Removed {}", SERVICE_PATH);
    } else {
        println!("ℹ  Service file not found: {}", SERVICE_PATH);
    }

    // Remove binary if at default location
    if Path::new(DEFAULT_BIN_PATH).exists() {
        fs::remove_file(DEFAULT_BIN_PATH)?;
        println!("🗑  Removed {}", DEFAULT_BIN_PATH);
    }

    run_cmd("systemctl", &["daemon-reload"])?;
    println!("✅ 服务已完全卸载");

    Ok(())
}

fn generate_service_file(config: &Config, bin_path: &str) -> String {
    let args_str = config.to_args_string();
    let cpu_caps = if config.cpu_target > 0.0 {
        "\nAmbientCapabilities=CAP_SYS_NICE"
    } else {
        ""
    };
    format!(
        r#"[Unit]
Description=Server Sponge - Dynamic Resource Occupation with PID Control
After=network.target

[Service]
Type=simple
ExecStart={bin_path} run {args}
Restart=on-failure
RestartSec=5
Nice=10
LimitMEMLOCK=infinity
Environment=RUST_LOG=info{cpu_caps}

# OOM killer should prefer killing sponge over real services
OOMScoreAdjust=800

[Install]
WantedBy=multi-user.target
"#,
        bin_path = bin_path,
        args = args_str,
        cpu_caps = cpu_caps,
    )
}

fn is_root() -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::geteuid() == 0 }
    }
    #[cfg(not(unix))]
    {
        false
    }
}

fn run_cmd(cmd: &str, args: &[&str]) -> io::Result<()> {
    let output = Command::new(cmd).args(args).output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("{} {} failed: {}", cmd, args.join(" "), stderr.trim()),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_config() -> Config {
        Config {
            target: 70.0,
            chunk_size: 64,
            panic_threshold: 5.0,
            cooldown: 30,
            no_psi: false,
            kp: 2.0,
            ki: 0.1,
            kd: 0.5,
            interval: 1000,
            log_dir: "".into(),
            log_retention: 7,
            log_compress: true,
            cpu_target: 0.0,
            cpu_cycle: 100,
            cpu_panic_margin: 5.0,
            cpu_workers: 0,
            server_port: 0,
        }
    }

    #[test]
    fn test_generate_service_file_contains_exec_start() {
        let config = default_config();
        let content = generate_service_file(&config, "/usr/local/bin/server-sponge");
        assert!(content.contains("ExecStart=/usr/local/bin/server-sponge run"));
    }

    #[test]
    fn test_generate_service_file_preserves_target() {
        let mut config = default_config();
        config.target = 80.0;
        let content = generate_service_file(&config, "/usr/local/bin/server-sponge");
        assert!(content.contains("--target 80"), "content: {}", content);
    }

    #[test]
    fn test_generate_service_file_preserves_chunk_size() {
        let mut config = default_config();
        config.chunk_size = 32;
        let content = generate_service_file(&config, "/usr/local/bin/server-sponge");
        assert!(content.contains("--chunk-size 32"), "content: {}", content);
    }

    #[test]
    fn test_generate_service_file_preserves_no_psi() {
        let mut config = default_config();
        config.no_psi = true;
        let content = generate_service_file(&config, "/usr/local/bin/server-sponge");
        assert!(content.contains("--no-psi"), "content: {}", content);
    }

    #[test]
    fn test_generate_service_file_no_psi_when_false() {
        let config = default_config();
        let content = generate_service_file(&config, "/usr/local/bin/server-sponge");
        assert!(!content.contains("--no-psi"), "content: {}", content);
    }

    #[test]
    fn test_generate_service_file_preserves_pid_params() {
        let mut config = default_config();
        config.kp = 3.5;
        config.ki = 0.2;
        config.kd = 1.0;
        let content = generate_service_file(&config, "/usr/local/bin/server-sponge");
        assert!(content.contains("--kp 3.5"), "content: {}", content);
        assert!(content.contains("--ki 0.2"), "content: {}", content);
        assert!(content.contains("--kd 1"), "content: {}", content);
    }

    #[test]
    fn test_generate_service_file_custom_bin_path() {
        let config = default_config();
        let content = generate_service_file(&config, "/opt/sponge/server-sponge");
        assert!(content.contains("ExecStart=/opt/sponge/server-sponge run"));
    }

    #[test]
    fn test_generate_service_file_has_unit_section() {
        let config = default_config();
        let content = generate_service_file(&config, "/usr/local/bin/server-sponge");
        assert!(content.contains("[Unit]"));
        assert!(content.contains("[Service]"));
        assert!(content.contains("[Install]"));
    }

    #[test]
    fn test_generate_service_file_has_oom_adjust() {
        let config = default_config();
        let content = generate_service_file(&config, "/usr/local/bin/server-sponge");
        assert!(content.contains("OOMScoreAdjust=800"));
    }

    #[test]
    fn test_generate_service_file_has_restart() {
        let config = default_config();
        let content = generate_service_file(&config, "/usr/local/bin/server-sponge");
        assert!(content.contains("Restart=on-failure"));
        assert!(content.contains("RestartSec=5"));
    }

    #[test]
    fn test_generate_service_file_has_rust_log() {
        let config = default_config();
        let content = generate_service_file(&config, "/usr/local/bin/server-sponge");
        assert!(content.contains("Environment=RUST_LOG=info"));
    }

    #[test]
    fn test_generate_service_file_preserves_all_params() {
        let config = Config {
            target: 85.0,
            chunk_size: 128,
            panic_threshold: 3.0,
            cooldown: 60,
            no_psi: true,
            kp: 1.5,
            ki: 0.05,
            kd: 0.8,
            interval: 2000,
            log_dir: "".into(),
            log_retention: 7,
            log_compress: true,
            cpu_target: 70.0,
            cpu_cycle: 200,
            cpu_panic_margin: 10.0,
            cpu_workers: 4,
            server_port: 8080,
        };
        let content = generate_service_file(&config, "/usr/local/bin/server-sponge");
        assert!(content.contains("--target 85"));
        assert!(content.contains("--chunk-size 128"));
        assert!(content.contains("--panic-threshold 3"));
        assert!(content.contains("--cooldown 60"));
        assert!(content.contains("--no-psi"));
        assert!(content.contains("--kp 1.5"));
        assert!(content.contains("--ki 0.05"));
        assert!(content.contains("--kd 0.8"));
        assert!(content.contains("--interval 2000"));
        assert!(content.contains("--cpu-target 70"));
        assert!(content.contains("--cpu-cycle 200"));
        assert!(content.contains("--cpu-panic-margin 10"));
        assert!(content.contains("--cpu-workers 4"));
        assert!(content.contains("AmbientCapabilities=CAP_SYS_NICE"));
    }

    #[test]
    fn test_generate_service_file_no_cap_sys_nice_without_cpu() {
        let config = default_config(); // cpu_target=0
        let content = generate_service_file(&config, "/usr/local/bin/server-sponge");
        assert!(!content.contains("CAP_SYS_NICE"));
    }
}
