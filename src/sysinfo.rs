use log::debug;
use std::fs;
use std::io;

#[derive(Debug, Clone, Copy)]
pub struct MemInfo {
    /// Total physical memory in bytes
    pub total: u64,
    /// Available memory in bytes
    pub available: u64,
    /// Swap total in bytes
    pub swap_total: u64,
    /// Swap free in bytes
    pub swap_free: u64,
    /// Whether we are reading from cgroup (container) limits
    pub is_cgroup: bool,
}

impl MemInfo {
    /// Current memory usage as a percentage (0.0 - 100.0)
    pub fn usage_percent(&self) -> f64 {
        if self.total == 0 {
            return 0.0;
        }
        ((self.total - self.available) as f64 / self.total as f64) * 100.0
    }

    /// Available memory as a percentage (0.0 - 100.0)
    pub fn available_percent(&self) -> f64 {
        if self.total == 0 {
            return 100.0;
        }
        (self.available as f64 / self.total as f64) * 100.0
    }

    /// Whether swap is being actively used
    pub fn swap_in_use(&self) -> bool {
        self.swap_total > 0 && self.swap_free < self.swap_total
    }

    /// Swap usage as a percentage
    pub fn swap_usage_percent(&self) -> f64 {
        if self.swap_total == 0 {
            return 0.0;
        }
        ((self.swap_total - self.swap_free) as f64 / self.swap_total as f64) * 100.0
    }
}

/// Read memory info, preferring cgroup limits when running in a container.
pub fn get_memory_info() -> io::Result<MemInfo> {
    // Try cgroup v2 first, then v1, then fall back to /proc/meminfo
    if let Ok(info) = get_cgroup_v2_memory() {
        return Ok(info);
    }
    if let Ok(info) = get_cgroup_v1_memory() {
        return Ok(info);
    }
    get_proc_memory()
}

/// cgroup v2: /sys/fs/cgroup/memory.max + memory.current
fn get_cgroup_v2_memory() -> io::Result<MemInfo> {
    let max_str = fs::read_to_string("/sys/fs/cgroup/memory.max")?;
    let max_str = max_str.trim();
    // "max" means no limit — fall through to /proc/meminfo
    if max_str == "max" {
        return Err(io::Error::new(io::ErrorKind::Other, "cgroup v2: no memory limit set"));
    }
    let total: u64 = max_str
        .parse()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("parse memory.max: {}", e)))?;

    let current: u64 = fs::read_to_string("/sys/fs/cgroup/memory.current")?
        .trim()
        .parse()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("parse memory.current: {}", e)))?;

    let available = total.saturating_sub(current);
    debug!("cgroup v2: total={}, current={}, available={}", total, current, available);

    // Read swap from cgroup if available
    let (swap_total, swap_free) = read_cgroup_v2_swap(total);

    Ok(MemInfo {
        total,
        available,
        swap_total,
        swap_free,
        is_cgroup: true,
    })
}

fn read_cgroup_v2_swap(mem_max: u64) -> (u64, u64) {
    // memory.swap.max and memory.swap.current
    let swap_max = fs::read_to_string("/sys/fs/cgroup/memory.swap.max")
        .ok()
        .and_then(|s| {
            let s = s.trim();
            if s == "max" { None } else { s.parse::<u64>().ok() }
        })
        .unwrap_or(0);
    let swap_current = fs::read_to_string("/sys/fs/cgroup/memory.swap.current")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0);
    let _ = mem_max; // not needed for swap calc
    (swap_max, swap_max.saturating_sub(swap_current))
}

/// cgroup v1: /sys/fs/cgroup/memory/memory.limit_in_bytes + memory.usage_in_bytes
fn get_cgroup_v1_memory() -> io::Result<MemInfo> {
    let limit: u64 = fs::read_to_string("/sys/fs/cgroup/memory/memory.limit_in_bytes")?
        .trim()
        .parse()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("parse v1 limit: {}", e)))?;

    // Very large values (near u64::MAX / page-aligned) mean "no limit"
    if limit > (1u64 << 62) {
        return Err(io::Error::new(io::ErrorKind::Other, "cgroup v1: no memory limit set"));
    }

    let usage: u64 = fs::read_to_string("/sys/fs/cgroup/memory/memory.usage_in_bytes")?
        .trim()
        .parse()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("parse v1 usage: {}", e)))?;

    let available = limit.saturating_sub(usage);
    debug!("cgroup v1: limit={}, usage={}, available={}", limit, usage, available);

    Ok(MemInfo {
        total: limit,
        available,
        swap_total: 0,
        swap_free: 0,
        is_cgroup: true,
    })
}

/// Fallback: read /proc/meminfo (bare metal or no cgroup limit)
fn get_proc_memory() -> io::Result<MemInfo> {
    let content = fs::read_to_string("/proc/meminfo")?;
    parse_meminfo(&content)
}

fn parse_meminfo(content: &str) -> io::Result<MemInfo> {
    let mut total: Option<u64> = None;
    let mut available: Option<u64> = None;
    let mut swap_total: Option<u64> = None;
    let mut swap_free: Option<u64> = None;

    for line in content.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 2 {
            continue;
        }
        let value_kb: u64 = match parts[1].parse() {
            Ok(v) => v,
            Err(_) => continue,
        };
        match parts[0] {
            "MemTotal:" => total = Some(value_kb * 1024),
            "MemAvailable:" => available = Some(value_kb * 1024),
            "SwapTotal:" => swap_total = Some(value_kb * 1024),
            "SwapFree:" => swap_free = Some(value_kb * 1024),
            _ => {}
        }
    }

    Ok(MemInfo {
        total: total.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "MemTotal not found"))?,
        available: available.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "MemAvailable not found"))?,
        swap_total: swap_total.unwrap_or(0),
        swap_free: swap_free.unwrap_or(0),
        is_cgroup: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_meminfo tests ──

    #[test]
    fn test_parse_meminfo_basic() {
        let content = "\
MemTotal:        8052444 kB
MemFree:         1234567 kB
MemAvailable:    4026222 kB
Buffers:          123456 kB
Cached:          2345678 kB
SwapTotal:       2097148 kB
SwapFree:        2097148 kB
";
        let info = parse_meminfo(content).unwrap();
        assert_eq!(info.total, 8052444 * 1024);
        assert_eq!(info.available, 4026222 * 1024);
        assert!((info.usage_percent() - 50.0).abs() < 1.0);
        assert!(!info.swap_in_use());
        assert!(!info.is_cgroup);
    }

    #[test]
    fn test_parse_meminfo_missing_mem_total() {
        let content = "MemAvailable:    4026222 kB\n";
        let result = parse_meminfo(content);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_meminfo_missing_mem_available() {
        let content = "MemTotal:        8052444 kB\n";
        let result = parse_meminfo(content);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_meminfo_empty() {
        let result = parse_meminfo("");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_meminfo_no_swap() {
        let content = "\
MemTotal:        4000000 kB
MemAvailable:    2000000 kB
";
        let info = parse_meminfo(content).unwrap();
        assert_eq!(info.swap_total, 0);
        assert_eq!(info.swap_free, 0);
        assert!(!info.swap_in_use());
        assert_eq!(info.swap_usage_percent(), 0.0);
    }

    #[test]
    fn test_parse_meminfo_swap_active() {
        let content = "\
MemTotal:        4000000 kB
MemAvailable:    1000000 kB
SwapTotal:       2000000 kB
SwapFree:         500000 kB
";
        let info = parse_meminfo(content).unwrap();
        assert!(info.swap_in_use());
        assert!((info.swap_usage_percent() - 75.0).abs() < 0.1);
    }

    #[test]
    fn test_parse_meminfo_extra_fields_ignored() {
        let content = "\
MemTotal:        8000000 kB
MemFree:         2000000 kB
MemAvailable:    4000000 kB
Buffers:          100000 kB
Cached:          1000000 kB
SwapCached:        50000 kB
Active:          3000000 kB
Inactive:        1000000 kB
SwapTotal:       1000000 kB
SwapFree:        1000000 kB
Dirty:                 0 kB
";
        let info = parse_meminfo(content).unwrap();
        assert_eq!(info.total, 8000000 * 1024);
        assert_eq!(info.available, 4000000 * 1024);
    }

    #[test]
    fn test_parse_meminfo_malformed_line_skipped() {
        let content = "\
MemTotal:        8000000 kB
this is garbage
MemAvailable:    4000000 kB
another garbage line
SwapTotal:       1000000 kB
SwapFree:        1000000 kB
";
        let info = parse_meminfo(content).unwrap();
        assert_eq!(info.total, 8000000 * 1024);
        assert_eq!(info.available, 4000000 * 1024);
    }

    // ── MemInfo calculation tests ──

    #[test]
    fn test_usage_percent_50() {
        let info = MemInfo {
            total: 1000, available: 500, swap_total: 0, swap_free: 0, is_cgroup: false,
        };
        assert!((info.usage_percent() - 50.0).abs() < 0.01);
    }

    #[test]
    fn test_usage_percent_100() {
        let info = MemInfo {
            total: 1000, available: 0, swap_total: 0, swap_free: 0, is_cgroup: false,
        };
        assert!((info.usage_percent() - 100.0).abs() < 0.01);
    }

    #[test]
    fn test_usage_percent_0() {
        let info = MemInfo {
            total: 1000, available: 1000, swap_total: 0, swap_free: 0, is_cgroup: false,
        };
        assert!((info.usage_percent()).abs() < 0.01);
    }

    #[test]
    fn test_usage_percent_zero_total() {
        let info = MemInfo {
            total: 0, available: 0, swap_total: 0, swap_free: 0, is_cgroup: false,
        };
        assert_eq!(info.usage_percent(), 0.0);
    }

    #[test]
    fn test_available_percent() {
        let info = MemInfo {
            total: 1000, available: 300, swap_total: 0, swap_free: 0, is_cgroup: false,
        };
        assert!((info.available_percent() - 30.0).abs() < 0.01);
    }

    #[test]
    fn test_available_percent_zero_total() {
        let info = MemInfo {
            total: 0, available: 0, swap_total: 0, swap_free: 0, is_cgroup: false,
        };
        assert_eq!(info.available_percent(), 100.0);
    }

    #[test]
    fn test_swap_in_use_false_when_equal() {
        let info = MemInfo {
            total: 1000, available: 500, swap_total: 2000, swap_free: 2000, is_cgroup: false,
        };
        assert!(!info.swap_in_use());
    }

    #[test]
    fn test_swap_in_use_false_when_no_swap() {
        let info = MemInfo {
            total: 1000, available: 500, swap_total: 0, swap_free: 0, is_cgroup: false,
        };
        assert!(!info.swap_in_use());
    }

    #[test]
    fn test_swap_in_use_true() {
        let info = MemInfo {
            total: 1000, available: 500, swap_total: 2000, swap_free: 1000, is_cgroup: false,
        };
        assert!(info.swap_in_use());
        assert!((info.swap_usage_percent() - 50.0).abs() < 0.01);
    }

    #[test]
    fn test_swap_usage_percent_no_swap() {
        let info = MemInfo {
            total: 1000, available: 500, swap_total: 0, swap_free: 0, is_cgroup: false,
        };
        assert_eq!(info.swap_usage_percent(), 0.0);
    }

    // ── usage + available sum to 100% ──

    #[test]
    fn test_usage_plus_available_is_100() {
        let info = MemInfo {
            total: 8192, available: 3000, swap_total: 0, swap_free: 0, is_cgroup: false,
        };
        let sum = info.usage_percent() + info.available_percent();
        assert!((sum - 100.0).abs() < 0.01, "sum={}", sum);
    }

    // ── Large values ──

    #[test]
    fn test_large_memory_values() {
        // 128 GB total, 64 GB available
        let info = MemInfo {
            total: 128 * 1024 * 1024 * 1024,
            available: 64 * 1024 * 1024 * 1024,
            swap_total: 0,
            swap_free: 0,
            is_cgroup: false,
        };
        assert!((info.usage_percent() - 50.0).abs() < 0.01);
    }
}
