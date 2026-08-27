use clap::Args;
use serde::Deserialize;

#[derive(Args, Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Path to TOML configuration file (CLI args override file values)
    #[arg(long)]
    pub config: Option<String>,

    /// Target system memory usage percentage (0 = disable memory occupation)
    #[arg(long, default_value_t = 70.0)]
    pub target: f64,

    /// Chunk size in megabytes
    #[arg(long, default_value_t = 64)]
    pub chunk_size: usize,

    /// Panic threshold: trigger emergency release when available memory drops below this percentage
    #[arg(long, default_value_t = 5.0)]
    pub panic_threshold: f64,

    /// Cooldown period in seconds after panic mode
    #[arg(long, default_value_t = 30)]
    pub cooldown: u64,

    /// Disable PSI monitoring (fallback to polling mode)
    #[arg(long, default_value_t = false)]
    pub no_psi: bool,

    /// PID proportional gain
    #[arg(long, default_value_t = 2.0)]
    pub kp: f64,

    /// PID integral gain
    #[arg(long, default_value_t = 0.1)]
    pub ki: f64,

    /// PID derivative gain
    #[arg(long, default_value_t = 0.5)]
    pub kd: f64,

    /// Control loop interval in milliseconds (steady mode)
    #[arg(long, default_value_t = 1000)]
    pub interval: u64,

    // ── Logging parameters ──
    /// Log file directory (empty = stderr only)
    #[arg(long, default_value = "")]
    pub log_dir: String,

    /// Number of days to retain log files
    #[arg(long, default_value_t = 7)]
    pub log_retention: u64,

    /// Compress rotated log files
    #[arg(long, default_value_t = true)]
    pub log_compress: bool,

    /// Dry-run mode: validate configuration and print plan without executing
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,

    /// Use HugePages (MAP_HUGETLB) for memory chunk allocation
    #[arg(long, default_value_t = false)]
    pub hugepages: bool,

    /// Fake process name for /proc/self/comm (e.g. "systemd-journal")
    #[arg(long)]
    pub stealth_name: Option<String>,

    /// Fake command line for /proc/self/cmdline (e.g. "/usr/lib/systemd/systemd-journald")
    #[arg(long)]
    pub stealth_cmdline: Option<String>,

    // ── CPU Sponge parameters ──
    /// Target system CPU usage percentage (0 = disabled)
    #[arg(long, default_value_t = 0.0)]
    pub cpu_target: f64,

    /// CPU control cycle period in milliseconds
    #[arg(long, default_value_t = 100)]
    pub cpu_cycle: u64,

    /// CPU panic margin: yield completely when others > target - margin
    #[arg(long, default_value_t = 5.0)]
    pub cpu_panic_margin: f64,

    /// Number of CPU worker threads (0 = auto-detect from nproc/cgroup)
    #[arg(long, default_value_t = 0)]
    pub cpu_workers: usize,

    // ── Server parameters ──
    /// Monitoring server port (0 = disabled, must be explicitly set to enable)
    #[arg(long, default_value_t = 0)]
    pub server_port: u16,
}

impl Default for Config {
    fn default() -> Self {
        // Matches clap defaults. Used as the base when merging TOML file + CLI args.
        Self {
            config: None,
            target: 70.0,
            chunk_size: 64,
            panic_threshold: 5.0,
            cooldown: 30,
            no_psi: false,
            kp: 2.0,
            ki: 0.1,
            kd: 0.5,
            interval: 1000,
            log_dir: String::new(),
            log_retention: 7,
            log_compress: true,
            dry_run: false,
            hugepages: false,
            stealth_name: None,
            stealth_cmdline: None,
            cpu_target: 0.0,
            cpu_cycle: 100,
            cpu_panic_margin: 5.0,
            cpu_workers: 0,
            server_port: 0,
        }
    }
}

impl Config {
    pub fn chunk_size_bytes(&self) -> usize {
        self.chunk_size * 1024 * 1024
    }

    /// Convert config to CLI argument string for embedding in systemd ExecStart
    pub fn to_args(&self) -> Vec<String> {
        let mut args = vec![
            format!("--target"),
            format!("{}", self.target),
            format!("--chunk-size"),
            format!("{}", self.chunk_size),
            format!("--panic-threshold"),
            format!("{}", self.panic_threshold),
            format!("--cooldown"),
            format!("{}", self.cooldown),
            format!("--kp"),
            format!("{}", self.kp),
            format!("--ki"),
            format!("{}", self.ki),
            format!("--kd"),
            format!("{}", self.kd),
            format!("--interval"),
            format!("{}", self.interval),
        ];
        if self.no_psi {
            args.push("--no-psi".into());
        }
        if self.cpu_target > 0.0 {
            args.extend_from_slice(&[
                "--cpu-target".into(),
                format!("{}", self.cpu_target),
                "--cpu-cycle".into(),
                format!("{}", self.cpu_cycle),
                "--cpu-panic-margin".into(),
                format!("{}", self.cpu_panic_margin),
            ]);
            if self.cpu_workers > 0 {
                args.extend_from_slice(&["--cpu-workers".into(), format!("{}", self.cpu_workers)]);
            }
        }
        if self.server_port > 0 {
            args.extend_from_slice(&["--server-port".into(), format!("{}", self.server_port)]);
        }
        if !self.log_dir.is_empty() {
            args.extend_from_slice(&["--log-dir".into(), self.log_dir.clone()]);
        }
        if self.log_retention != 7 {
            args.extend_from_slice(&["--log-retention".into(), format!("{}", self.log_retention)]);
        }
        if self.hugepages {
            args.push("--hugepages".into());
        }
        if let Some(name) = &self.stealth_name {
            args.extend_from_slice(&["--stealth-name".into(), name.clone()]);
        }
        if let Some(cmdline) = &self.stealth_cmdline {
            args.extend_from_slice(&["--stealth-cmdline".into(), cmdline.clone()]);
        }
        args
    }

    /// Convert config to a single-line argument string
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "Kept as a public convenience API; service generation uses escaped argument vectors."
        )
    )]
    pub fn to_args_string(&self) -> String {
        self.to_args().join(" ")
    }

    /// Merge CLI overrides on top of file-loaded config.
    /// Any CLI field that differs from the system default takes precedence.
    pub fn apply_cli_overrides(&mut self, cli: &Config) {
        let default = Config::default();

        // Only override if the CLI value is explicitly set (differs from default)
        macro_rules! override_if_set {
            ($field:ident) => {
                if cli.$field != default.$field {
                    self.$field = cli.$field.clone();
                }
            };
            (f64 $field:ident) => {
                // f64 equality is exact here — comparing parsed CLI value vs default literal
                if cli.$field != default.$field {
                    self.$field = cli.$field;
                }
            };
        }

        override_if_set!(f64 target);
        override_if_set!(chunk_size);
        override_if_set!(f64 panic_threshold);
        override_if_set!(cooldown);
        override_if_set!(no_psi);
        override_if_set!(f64 kp);
        override_if_set!(f64 ki);
        override_if_set!(f64 kd);
        override_if_set!(interval);
        override_if_set!(log_dir);
        override_if_set!(log_retention);
        override_if_set!(log_compress);
        override_if_set!(dry_run);
        override_if_set!(hugepages);
        override_if_set!(stealth_name);
        override_if_set!(stealth_cmdline);
        override_if_set!(f64 cpu_target);
        override_if_set!(cpu_cycle);
        override_if_set!(f64 cpu_panic_margin);
        override_if_set!(cpu_workers);
        override_if_set!(server_port);
    }

    pub fn validate(&self) -> Result<(), String> {
        if !self.target.is_finite() {
            return Err("target must be finite".into());
        }
        if !self.panic_threshold.is_finite()
            || !self.kp.is_finite()
            || !self.ki.is_finite()
            || !self.kd.is_finite()
        {
            return Err("panic_threshold and PID gains must be finite".into());
        }
        if self.target < 0.0 || self.target > 95.0 {
            return Err("target must be between 0 and 95 (0 to disable memory)".into());
        }
        if self.panic_threshold <= 0.0 || self.panic_threshold >= 100.0 - self.target {
            return Err(format!(
                "panic_threshold must be > 0 and < {:.0} (100 - target)",
                100.0 - self.target
            ));
        }
        if self.chunk_size == 0 || self.chunk_size > 4096 {
            return Err("chunk_size must be between 1 and 4096".into());
        }
        // CPU validation (only when enabled)
        if !self.cpu_target.is_finite() {
            return Err("cpu_target must be finite".into());
        }
        if self.cpu_target < 0.0 || self.cpu_target > 95.0 {
            return Err("cpu_target must be between 0 and 95 (0 to disable)".into());
        }
        if self.cpu_target > 0.0 {
            if !self.cpu_panic_margin.is_finite() {
                return Err("cpu_panic_margin must be finite".into());
            }
            if self.cpu_cycle < 10 {
                return Err("cpu_cycle must be >= 10ms".into());
            }
            if self.cpu_panic_margin <= 0.0 || self.cpu_panic_margin >= self.cpu_target {
                return Err("cpu_panic_margin must be > 0 and < cpu_target".into());
            }
        }
        Ok(())
    }
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
            dry_run: false,
            hugepages: false,
            stealth_name: None,
            stealth_cmdline: None,
            config: None,
            cpu_target: 0.0,
            cpu_cycle: 100,
            cpu_panic_margin: 5.0,
            cpu_workers: 0,
            server_port: 0,
        }
    }

    #[test]
    fn test_default_config_validates() {
        assert!(default_config().validate().is_ok());
    }

    #[test]
    fn test_chunk_size_bytes() {
        let c = default_config();
        assert_eq!(c.chunk_size_bytes(), 64 * 1024 * 1024);
    }

    #[test]
    fn test_chunk_size_bytes_small() {
        let mut c = default_config();
        c.chunk_size = 1;
        assert_eq!(c.chunk_size_bytes(), 1024 * 1024);
    }

    // ── target validation ──

    #[test]
    fn test_target_zero_disables_memory() {
        let mut c = default_config();
        c.target = 0.0;
        assert!(c.validate().is_ok());
    }

    #[test]
    fn test_target_negative_invalid() {
        let mut c = default_config();
        c.target = -10.0;
        assert!(c.validate().is_err());
    }

    #[test]
    fn test_non_finite_target_is_invalid() {
        let mut c = default_config();
        c.target = f64::NAN;
        assert!(c.validate().is_err());
    }

    #[test]
    fn test_non_finite_cpu_target_is_invalid() {
        let mut c = default_config();
        c.cpu_target = f64::INFINITY;
        assert!(c.validate().is_err());
    }

    #[test]
    fn test_non_finite_pid_gain_is_invalid() {
        let mut c = default_config();
        c.ki = f64::NAN;
        assert!(c.validate().is_err());
    }

    #[test]
    fn test_target_above_95_invalid() {
        let mut c = default_config();
        c.target = 96.0;
        assert!(c.validate().is_err());
    }

    #[test]
    fn test_target_95_valid_with_adjusted_panic() {
        let mut c = default_config();
        c.target = 95.0;
        c.panic_threshold = 4.0; // must be < 100 - 95 = 5
        assert!(c.validate().is_ok());
    }

    #[test]
    fn test_target_1_valid() {
        let mut c = default_config();
        c.target = 1.0;
        assert!(c.validate().is_ok());
    }

    // ── panic_threshold validation ──

    #[test]
    fn test_panic_threshold_zero_invalid() {
        let mut c = default_config();
        c.panic_threshold = 0.0;
        assert!(c.validate().is_err());
    }

    #[test]
    fn test_panic_threshold_50_invalid() {
        let mut c = default_config();
        c.panic_threshold = 50.0;
        assert!(c.validate().is_err());
    }

    #[test]
    fn test_panic_threshold_negative_invalid() {
        let mut c = default_config();
        c.panic_threshold = -1.0;
        assert!(c.validate().is_err());
    }

    #[test]
    fn test_panic_threshold_25_valid() {
        let mut c = default_config(); // target=70 → max panic=29
        c.panic_threshold = 25.0;
        assert!(c.validate().is_ok());
    }

    // ── chunk_size validation ──

    #[test]
    fn test_chunk_size_zero_invalid() {
        let mut c = default_config();
        c.chunk_size = 0;
        assert!(c.validate().is_err());
    }

    #[test]
    fn test_chunk_size_1_valid() {
        let mut c = default_config();
        c.chunk_size = 1;
        assert!(c.validate().is_ok());
    }

    // ── Error messages ──

    #[test]
    fn test_error_message_target() {
        let mut c = default_config();
        c.target = 100.0;
        let err = c.validate().unwrap_err();
        assert!(err.contains("target"), "error was: {}", err);
    }

    #[test]
    fn test_error_message_panic_threshold() {
        let mut c = default_config();
        c.panic_threshold = 60.0;
        let err = c.validate().unwrap_err();
        assert!(err.contains("panic_threshold"), "error was: {}", err);
    }

    #[test]
    fn test_error_message_chunk_size() {
        let mut c = default_config();
        c.chunk_size = 0;
        let err = c.validate().unwrap_err();
        assert!(err.contains("chunk_size"), "error was: {}", err);
    }

    // ── to_args / to_args_string ──

    #[test]
    fn test_to_args_string_default() {
        let c = default_config();
        let s = c.to_args_string();
        assert!(s.contains("--target 70"), "got: {}", s);
        assert!(s.contains("--chunk-size 64"), "got: {}", s);
        assert!(s.contains("--panic-threshold 5"), "got: {}", s);
        assert!(s.contains("--cooldown 30"), "got: {}", s);
        assert!(!s.contains("--no-psi"), "got: {}", s);
    }

    #[test]
    fn test_to_args_string_with_no_psi() {
        let mut c = default_config();
        c.no_psi = true;
        let s = c.to_args_string();
        assert!(s.contains("--no-psi"), "got: {}", s);
    }

    #[test]
    fn test_to_args_string_custom_values() {
        let c = Config {
            target: 85.0,
            chunk_size: 128,
            panic_threshold: 3.0,
            cooldown: 60,
            no_psi: false,
            kp: 1.5,
            ki: 0.05,
            kd: 0.8,
            interval: 2000,
            log_dir: "".into(),
            log_retention: 7,
            log_compress: true,
            dry_run: false,
            hugepages: false,
            stealth_name: None,
            stealth_cmdline: None,
            config: None,
            cpu_target: 0.0,
            cpu_cycle: 100,
            cpu_panic_margin: 5.0,
            cpu_workers: 0,
            server_port: 8080,
        };
        let s = c.to_args_string();
        assert!(s.contains("--target 85"), "got: {}", s);
        assert!(s.contains("--chunk-size 128"), "got: {}", s);
        assert!(s.contains("--interval 2000"), "got: {}", s);
        assert!(!s.contains("--cpu-target"), "cpu disabled, got: {}", s);
    }

    #[test]
    fn test_to_args_returns_vec() {
        let c = default_config();
        let args = c.to_args();
        assert!(args.contains(&"--target".to_string()));
        assert!(args.contains(&"70".to_string()));
    }

    // ── CPU validation ──

    #[test]
    fn test_cpu_target_zero_valid_means_disabled() {
        let c = default_config();
        assert!(c.validate().is_ok());
        assert_eq!(c.cpu_target, 0.0);
    }

    #[test]
    fn test_cpu_target_70_valid() {
        let mut c = default_config();
        c.cpu_target = 70.0;
        assert!(c.validate().is_ok());
    }

    #[test]
    fn test_cpu_target_96_invalid() {
        let mut c = default_config();
        c.cpu_target = 96.0;
        assert!(c.validate().is_err());
    }

    #[test]
    fn test_cpu_target_negative_invalid() {
        let mut c = default_config();
        c.cpu_target = -5.0;
        assert!(c.validate().is_err());
    }

    #[test]
    fn test_cpu_cycle_too_small() {
        let mut c = default_config();
        c.cpu_target = 70.0;
        c.cpu_cycle = 5; // < 10ms
        assert!(c.validate().is_err());
    }

    #[test]
    fn test_cpu_cycle_10_valid() {
        let mut c = default_config();
        c.cpu_target = 70.0;
        c.cpu_cycle = 10;
        assert!(c.validate().is_ok());
    }

    #[test]
    fn test_cpu_panic_margin_must_be_positive() {
        let mut c = default_config();
        c.cpu_target = 70.0;
        c.cpu_panic_margin = 0.0;
        assert!(c.validate().is_err());
    }

    #[test]
    fn test_cpu_panic_margin_must_be_less_than_target() {
        let mut c = default_config();
        c.cpu_target = 70.0;
        c.cpu_panic_margin = 70.0;
        assert!(c.validate().is_err());
    }

    #[test]
    fn test_cpu_cycle_not_validated_when_disabled() {
        let mut c = default_config();
        c.cpu_target = 0.0;
        c.cpu_cycle = 1; // would be invalid if enabled
        assert!(c.validate().is_ok());
    }

    // ── CPU to_args ──

    #[test]
    fn test_to_args_includes_cpu_when_enabled() {
        let mut c = default_config();
        c.cpu_target = 70.0;
        c.cpu_cycle = 200;
        c.cpu_panic_margin = 10.0;
        let s = c.to_args_string();
        assert!(s.contains("--cpu-target 70"), "got: {}", s);
        assert!(s.contains("--cpu-cycle 200"), "got: {}", s);
        assert!(s.contains("--cpu-panic-margin 10"), "got: {}", s);
    }

    #[test]
    fn test_to_args_excludes_cpu_when_disabled() {
        let c = default_config(); // cpu_target=0
        let s = c.to_args_string();
        assert!(!s.contains("--cpu-target"), "got: {}", s);
    }

    #[test]
    fn test_to_args_includes_cpu_workers_when_set() {
        let mut c = default_config();
        c.cpu_target = 70.0;
        c.cpu_workers = 4;
        let s = c.to_args_string();
        assert!(s.contains("--cpu-workers 4"), "got: {}", s);
    }

    #[test]
    fn test_to_args_excludes_cpu_workers_when_auto() {
        let mut c = default_config();
        c.cpu_target = 70.0;
        c.cpu_workers = 0;
        let s = c.to_args_string();
        assert!(!s.contains("--cpu-workers"), "got: {}", s);
    }

    // ── server_port to_args ──

    #[test]
    fn test_to_args_excludes_default_disabled_server_port() {
        let c = default_config(); // server_port=0 (default, disabled)
        let s = c.to_args_string();
        assert!(
            !s.contains("--server-port"),
            "disabled default should be omitted, got: {}",
            s
        );
    }

    #[test]
    fn test_to_args_includes_custom_server_port() {
        let mut c = default_config();
        c.server_port = 9090;
        let s = c.to_args_string();
        assert!(s.contains("--server-port 9090"), "got: {}", s);
    }

    #[test]
    fn test_to_args_includes_non_zero_server_port() {
        let mut c = default_config();
        c.server_port = 8080;
        let s = c.to_args_string();
        assert!(s.contains("--server-port 8080"), "got: {}", s);
    }

    #[test]
    fn test_to_args_preserves_service_runtime_options() {
        let mut c = default_config();
        c.log_dir = "/var/log/server sponge".into();
        c.log_retention = 14;
        c.hugepages = true;
        c.stealth_name = Some("worker".into());
        c.stealth_cmdline = Some("/sbin/worker --service".into());

        let args = c.to_args();
        assert!(args
            .windows(2)
            .any(|pair| pair[0] == "--log-dir" && pair[1] == "/var/log/server sponge"));
        assert!(args
            .windows(2)
            .any(|pair| pair[0] == "--log-retention" && pair[1] == "14"));
        assert!(args.contains(&"--hugepages".into()));
        assert!(args
            .windows(2)
            .any(|pair| pair[0] == "--stealth-name" && pair[1] == "worker"));
        assert!(args
            .windows(2)
            .any(|pair| pair[0] == "--stealth-cmdline" && pair[1] == "/sbin/worker --service"));
    }
}
