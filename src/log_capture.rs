use std::collections::VecDeque;
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

fn push(level: &str, message: String) {
    if let Ok(mut b) = buf().lock() {
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

/// Custom logger that wraps env_logger and captures log messages
/// into a ring buffer for the monitoring dashboard.
pub struct CaptureLogger {
    inner: env_logger::Logger,
}

impl CaptureLogger {
    pub fn init() {
        let logger = env_logger::Builder::from_env(
            env_logger::Env::default().default_filter_or("info"),
        )
        .format_timestamp_millis()
        .build();

        let max_level = logger.filter();
        let capture = CaptureLogger { inner: logger };

        log::set_boxed_logger(Box::new(capture)).ok();
        log::set_max_level(max_level);
    }
}

impl log::Log for CaptureLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        self.inner.enabled(metadata)
    }

    fn log(&self, record: &log::Record) {
        if self.inner.enabled(record.metadata()) {
            // Forward to env_logger for console output
            self.inner.log(record);

            // Capture into ring buffer for WebUI
            let level = match record.level() {
                log::Level::Error => "error",
                log::Level::Warn => "warn",
                log::Level::Info => "info",
                log::Level::Debug => "debug",
                log::Level::Trace => "trace",
            };
            push(level, format!("{}", record.args()));
        }
    }

    fn flush(&self) {
        self.inner.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Note: tests run in the same process, sharing the global buffer.
    // Use get_logs_since to isolate test observations.

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
        // Millis timestamp should be > 1_000_000_000_000 (year ~2001)
        assert!(e.timestamp > 1_000_000_000_000);
    }

    #[test]
    fn test_buffer_capped() {
        // Push more than MAX_ENTRIES to verify cap
        let (_, baseline) = get_logs_since(0);
        for i in 0..1100 {
            push("info", format!("flood {}", i));
        }
        let (all, _) = get_logs_since(0);
        assert!(all.len() <= MAX_ENTRIES);
        // We should still see the latest entries
        let (recent, _) = get_logs_since(baseline);
        assert!(!recent.is_empty());
    }

    #[test]
    fn test_empty_since_latest() {
        let (_, latest) = get_logs_since(0);
        let (entries, seq) = get_logs_since(latest);
        // Other tests may push concurrently, so only assert monotonicity
        assert!(seq >= latest);
        // All returned entries must have seq > latest
        for e in &entries {
            assert!(e.seq > latest);
        }
    }
}
