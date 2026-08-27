use std::collections::VecDeque;
use std::io::Write;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

const MAX_ENTRIES: usize = 1000;

#[derive(Clone, Serialize, Debug)]
pub struct LogEntry {
    pub seq: u64,
    pub timestamp: u64,
    pub level: String,
    pub message: String,
}

struct Buffer {
    entries: VecDeque<LogEntry>,
    next_seq: u64,
}

static BUFFER: OnceLock<Mutex<Buffer>> = OnceLock::new();

fn buf() -> &'static Mutex<Buffer> {
    BUFFER.get_or_init(|| {
        Mutex::new(Buffer {
            entries: VecDeque::with_capacity(MAX_ENTRIES),
            next_seq: 1,
        })
    })
}

/// Push a log entry into the ring buffer (deduplicates consecutive identical entries).
fn push(level: &str, message: String) {
    if let Ok(mut b) = buf().lock() {
        // Dedup: skip if last entry has same level and message
        if let Some(last) = b.entries.back() {
            if last.level == level && last.message == message {
                return;
            }
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let seq = b.next_seq;
        b.next_seq += 1;
        b.entries.push_back(LogEntry {
            seq,
            timestamp: now,
            level: level.into(),
            message,
        });
        while b.entries.len() > MAX_ENTRIES {
            b.entries.pop_front();
        }
    }
}

/// Get log entries with seq > since_seq. Returns (entries, latest_seq).
pub fn get_logs_since(since_seq: u64) -> (Vec<LogEntry>, u64) {
    if let Ok(b) = buf().lock() {
        let latest = b.next_seq.saturating_sub(1);
        let entries: Vec<_> = b
            .entries
            .iter()
            .filter(|e| e.seq > since_seq)
            .cloned()
            .collect();
        (entries, latest)
    } else {
        (Vec::new(), since_seq)
    }
}

/// Configuration for log output.
pub struct LogConfig {
    pub log_dir: String,
    pub log_retention_days: u64,
    pub log_compress: bool,
}

/// Initialize the logging system.
///
/// When `log_dir` is set, logs are written to both stderr and a rolling file
/// in the specified directory. When empty, only stderr output is produced.
///
/// Respects the `RUST_LOG` environment variable for log level filtering.
pub fn init(config: &LogConfig) {
    let filter_str = std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into());

    let mut logger = flexi_logger::Logger::try_with_str(&filter_str)
        .expect("Failed to create flexi_logger")
        .format_for_stderr(format_stderr)
        .print_message(); // Print a note to stderr on startup

    if !config.log_dir.is_empty() {
        let cleanup: Box<dyn Fn(usize) -> flexi_logger::Cleanup> = if config.log_compress {
            Box::new(flexi_logger::Cleanup::KeepCompressedFiles)
        } else {
            Box::new(flexi_logger::Cleanup::KeepLogFiles)
        };

        logger = logger
            .log_to_file(
                flexi_logger::FileSpec::default()
                    .directory(&config.log_dir)
                    .basename("server-sponge")
                    .suffix("log"),
            )
            .format_for_files(format_file)
            .rotate(
                flexi_logger::Criterion::Age(flexi_logger::Age::Day),
                flexi_logger::Naming::Timestamps,
                cleanup(config.log_retention_days.max(1) as usize),
            );
    }

    // Build and register as global logger
    let (boxed_logger, _handle) = logger.build().expect("Failed to build flexi_logger");

    // Determine max log level from the filter string
    let max_level = parse_max_level(&filter_str);
    log::set_boxed_logger(Box::new(FlexiCaptureLogger {
        inner: boxed_logger,
    }))
    .expect("Failed to register logger");
    log::set_max_level(max_level);

    if !config.log_dir.is_empty() {
        let _ = std::io::stderr().write_all(
            format!(
                "[server-sponge] Logging to: {}/server-sponge.log (retention={}d, compress={})\n",
                config.log_dir, config.log_retention_days, config.log_compress
            )
            .as_bytes(),
        );
    }
}

/// Parse the max log level from a RUST_LOG-style filter string.
/// The level is taken from the first segment (before any comma or `=`).
fn parse_max_level(filter: &str) -> log::LevelFilter {
    // Take only the part before any comma (module-specific overrides)
    let primary = filter.split(',').next().unwrap_or(filter).trim();
    // Remove any "=level" suffix if present
    let primary = primary.split('=').next().unwrap_or(primary).trim();

    match primary.to_lowercase().as_str() {
        "trace" => log::LevelFilter::Trace,
        "debug" => log::LevelFilter::Debug,
        "warn" => log::LevelFilter::Warn,
        "error" => log::LevelFilter::Error,
        "off" => log::LevelFilter::Off,
        _ => log::LevelFilter::Info,
    }
}

/// A logger that captures messages into the ring buffer for the dashboard,
/// and forwards to flexi_logger for file/console output.
struct FlexiCaptureLogger {
    inner: Box<dyn log::Log>,
}

impl log::Log for FlexiCaptureLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        self.inner.enabled(metadata)
    }

    fn log(&self, record: &log::Record) {
        if self.inner.enabled(record.metadata()) {
            // Capture into ring buffer for WebUI
            push(
                match record.level() {
                    log::Level::Error => "error",
                    log::Level::Warn => "warn",
                    log::Level::Info => "info",
                    log::Level::Debug => "debug",
                    log::Level::Trace => "trace",
                },
                format!("{}", record.args()),
            );

            // Forward to flexi_logger for actual output
            self.inner.log(record);
        }
    }

    fn flush(&self) {
        self.inner.flush();
    }
}

/// Format function for stderr: colored output.
/// Capture is handled upstream by `FlexiCaptureLogger::log()`.
fn format_stderr(
    w: &mut dyn std::io::Write,
    now: &mut flexi_logger::DeferredNow,
    record: &log::Record,
) -> std::io::Result<()> {
    flexi_logger::colored_opt_format(w, now, record)
}

/// Format function for file output: plain timestamped output.
fn format_file(
    w: &mut dyn std::io::Write,
    now: &mut flexi_logger::DeferredNow,
    record: &log::Record,
) -> std::io::Result<()> {
    flexi_logger::default_format(w, now, record)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_push_and_retrieve() {
        let (_, baseline) = get_logs_since(0);
        push("info", "test message alpha".into());
        let (logs, seq) = get_logs_since(baseline);
        assert!(seq > baseline);
        assert!(!logs.is_empty());
        assert!(logs.iter().any(|e| e.message == "test message alpha"));
    }

    #[test]
    fn test_incremental_retrieval() {
        let (_, seq0) = get_logs_since(0);
        push("warn", "batch1_unique_xyz".into());
        let (logs1, seq1) = get_logs_since(seq0);
        assert!(logs1.iter().any(|e| e.message == "batch1_unique_xyz"));
        assert!(seq1 > seq0);

        push("error", "batch2a_unique_xyz".into());
        push("info", "batch2b_unique_xyz".into());
        let (logs2, seq2) = get_logs_since(seq1);
        assert!(logs2.iter().any(|e| e.message == "batch2a_unique_xyz"));
        assert!(logs2.iter().any(|e| e.message == "batch2b_unique_xyz"));
        assert!(seq2 > seq1);
    }

    #[test]
    fn test_level_stored() {
        let (_, baseline) = get_logs_since(0);
        push("error", "critical".into());
        let (logs, _) = get_logs_since(baseline);
        let e = logs.iter().find(|e| e.message == "critical").unwrap();
        assert_eq!(e.level, "error");
    }

    #[test]
    fn test_timestamp_is_millis() {
        let (_, baseline) = get_logs_since(0);
        push("info", "ts check".into());
        let (logs, _) = get_logs_since(baseline);
        let e = logs.iter().find(|e| e.message == "ts check").unwrap();
        assert!(e.timestamp > 1_000_000_000_000);
    }

    #[test]
    fn test_buffer_capped() {
        let (_, baseline) = get_logs_since(0);
        for i in 0..1100 {
            push("info", format!("flood {}", i));
        }
        let (all, _) = get_logs_since(0);
        assert!(all.len() <= MAX_ENTRIES);
        let (recent, _) = get_logs_since(baseline);
        assert!(!recent.is_empty());
    }

    #[test]
    fn test_empty_since_latest() {
        let (_, latest) = get_logs_since(0);
        let (entries, seq) = get_logs_since(latest);
        assert!(seq >= latest);
        for e in &entries {
            assert!(e.seq > latest);
        }
    }

    #[test]
    fn test_dedup_consecutive_identical() {
        let (_, baseline) = get_logs_since(0);
        // Keep this sample isolated from the process-global buffer when tests
        // are run in parallel or multiple test binaries run concurrently.
        let message = format!("same message {:?}", std::thread::current().id());
        push("info", message.clone());
        push("info", message.clone());
        push("info", message.clone());
        let (logs, _) = get_logs_since(baseline);
        let same: Vec<_> = logs.iter().filter(|e| e.message == message).collect();
        assert_eq!(
            same.len(),
            1,
            "dedup should collapse identical consecutive entries"
        );
    }

    #[test]
    fn test_dedup_allows_different_interleaved() {
        let (_, baseline) = get_logs_since(0);
        push("info", "msg A".into());
        push("info", "msg B".into());
        push("info", "msg A".into());
        let (logs, _) = get_logs_since(baseline);
        let count_a = logs.iter().filter(|e| e.message == "msg A").count();
        assert_eq!(count_a, 2, "non-consecutive duplicates should be kept");
    }

    #[test]
    fn test_dedup_different_levels_kept() {
        // This test relies on seq-based isolation, but the global buffer
        // is shared across all tests. Run with --test-threads=1 for reliability.
        let (_, baseline) = get_logs_since(0);
        push("info", "same text".into());
        push("warn", "same text".into());
        let (logs, _) = get_logs_since(baseline);
        let infos = logs
            .iter()
            .filter(|e| e.message == "same text" && e.level == "info")
            .count();
        let warns = logs
            .iter()
            .filter(|e| e.message == "same text" && e.level == "warn")
            .count();
        // In concurrent test runs, other entries may also be present,
        // but our specific pushes should each appear exactly once
        assert_eq!(infos, 1, "info-level entry missing");
        assert_eq!(warns, 1, "warn-level entry missing");
    }

    #[test]
    fn test_parse_max_level() {
        assert_eq!(parse_max_level("info"), log::LevelFilter::Info);
        assert_eq!(parse_max_level("debug"), log::LevelFilter::Debug);
        assert_eq!(parse_max_level("warn"), log::LevelFilter::Warn);
        assert_eq!(parse_max_level("error"), log::LevelFilter::Error);
        assert_eq!(parse_max_level("trace"), log::LevelFilter::Trace);
        assert_eq!(
            parse_max_level("info,my_module=debug"),
            log::LevelFilter::Info
        );
    }
}
