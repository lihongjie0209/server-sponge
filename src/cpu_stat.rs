use std::fs;
use std::io;
use std::time::Instant;

/// Raw CPU time sample from /proc/stat aggregate "cpu" line.
/// All values are in jiffies (clock ticks) summed across all CPUs.
#[derive(Debug, Clone)]
pub struct CpuSample {
    pub user: u64,
    pub nice: u64,
    pub system: u64,
    pub idle: u64,
    pub iowait: u64,
    pub irq: u64,
    pub softirq: u64,
    pub steal: u64,
    pub total: u64,
}

/// Process CPU time from /proc/self/stat (jiffies)
#[derive(Debug, Clone)]
pub struct ProcessCpuSample {
    pub utime: u64,
    pub stime: u64,
}

/// Cgroup CPU accounting sample (microseconds)
#[derive(Debug, Clone)]
pub struct CgroupCpuSample {
    pub usage_usec: u64,
    pub timestamp: Instant,
}

/// Computed CPU usage between two sample pairs
#[derive(Debug, Clone)]
pub struct CpuUsage {
    /// Overall system CPU usage (0–100%)
    pub total_pct: f64,
    /// Our process CPU usage (0–100%)
    pub self_pct: f64,
    /// Other processes' CPU usage (0–100%)
    pub others_pct: f64,
}

impl std::fmt::Display for CpuUsage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "total={:.1}%, self={:.1}%, others={:.1}%",
            self.total_pct, self.self_pct, self.others_pct
        )
    }
}

/// Read aggregate CPU times from /proc/stat
pub fn read_proc_stat() -> io::Result<CpuSample> {
    let content = fs::read_to_string("/proc/stat")?;
    parse_proc_stat(&content)
}

/// Parse the first "cpu" aggregate line from /proc/stat content
pub fn parse_proc_stat(content: &str) -> io::Result<CpuSample> {
    for line in content.lines() {
        if line.starts_with("cpu ") {
            let fields: Vec<u64> = line
                .split_whitespace()
                .skip(1) // skip "cpu"
                .filter_map(|s| s.parse().ok())
                .collect();

            if fields.len() < 4 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "insufficient fields in /proc/stat cpu line",
                ));
            }

            let user = fields[0];
            let nice = fields[1];
            let system = fields[2];
            let idle = fields[3];
            let iowait = fields.get(4).copied().unwrap_or(0);
            let irq = fields.get(5).copied().unwrap_or(0);
            let softirq = fields.get(6).copied().unwrap_or(0);
            let steal = fields.get(7).copied().unwrap_or(0);
            let total = user + nice + system + idle + iowait + irq + softirq + steal;

            return Ok(CpuSample {
                user,
                nice,
                system,
                idle,
                iowait,
                irq,
                softirq,
                steal,
                total,
            });
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "no 'cpu' line found in /proc/stat",
    ))
}

/// Read process CPU time from /proc/self/stat
pub fn read_self_stat() -> io::Result<ProcessCpuSample> {
    let content = fs::read_to_string("/proc/self/stat")?;
    parse_self_stat(&content)
}

/// Parse utime and stime from /proc/self/stat content.
/// The comm field may contain spaces/parens, so we find the last ')' first.
pub fn parse_self_stat(content: &str) -> io::Result<ProcessCpuSample> {
    let after_comm = content
        .rfind(')')
        .map(|i| &content[i + 2..])
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "malformed /proc/self/stat")
        })?;

    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    // After "(comm) ", fields are: state ppid pgrp session tty tpgid flags
    //   minflt cminflt majflt cmajflt utime stime ...
    // utime = index 11, stime = index 12 (zero-indexed)
    if fields.len() < 13 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "insufficient fields in /proc/self/stat",
        ));
    }

    let utime: u64 = fields[11]
        .parse()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "cannot parse utime"))?;
    let stime: u64 = fields[12]
        .parse()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "cannot parse stime"))?;

    Ok(ProcessCpuSample { utime, stime })
}

/// Calculate CPU usage between two sample pairs.
/// `total_pct = (1 - Δidle / Δtotal) × 100`
/// `self_pct = Δ(utime+stime) / Δtotal × 100`
/// `others_pct = total_pct - self_pct`
pub fn calculate_usage(
    prev_sys: &CpuSample,
    curr_sys: &CpuSample,
    prev_proc: &ProcessCpuSample,
    curr_proc: &ProcessCpuSample,
) -> CpuUsage {
    let total_delta = curr_sys.total.saturating_sub(prev_sys.total);
    let idle_delta =
        (curr_sys.idle + curr_sys.iowait).saturating_sub(prev_sys.idle + prev_sys.iowait);

    if total_delta == 0 {
        return CpuUsage {
            total_pct: 0.0,
            self_pct: 0.0,
            others_pct: 0.0,
        };
    }

    let total_pct = (1.0 - idle_delta as f64 / total_delta as f64) * 100.0;

    let self_delta =
        (curr_proc.utime + curr_proc.stime).saturating_sub(prev_proc.utime + prev_proc.stime);
    let self_pct = (self_delta as f64 / total_delta as f64) * 100.0;

    let others_pct = (total_pct - self_pct).max(0.0);

    CpuUsage {
        total_pct: total_pct.clamp(0.0, 100.0),
        self_pct: self_pct.clamp(0.0, 100.0),
        others_pct: others_pct.clamp(0.0, 100.0),
    }
}

// ── Cgroup-based CPU accounting ──

/// Read cgroup v2 cpu.stat usage_usec for accurate container CPU measurement.
pub fn read_cgroup_cpu_usage() -> Option<CgroupCpuSample> {
    let content = fs::read_to_string("/sys/fs/cgroup/cpu.stat").ok()?;
    let usec = parse_cgroup_cpu_usage(&content)?;
    Some(CgroupCpuSample {
        usage_usec: usec,
        timestamp: Instant::now(),
    })
}

/// Parse usage_usec from cgroup v2 cpu.stat content.
pub fn parse_cgroup_cpu_usage(content: &str) -> Option<u64> {
    for line in content.lines() {
        if line.starts_with("usage_usec ") {
            return line.split_whitespace().nth(1)?.parse().ok();
        }
    }
    None
}

/// Get CLK_TCK (clock ticks per second) for converting jiffies to microseconds.
fn clk_tck() -> u64 {
    #[cfg(target_os = "linux")]
    {
        let tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        if tck > 0 { tck as u64 } else { 100 }
    }
    #[cfg(not(target_os = "linux"))]
    { 100 }
}

/// Calculate container-relative CPU usage from cgroup cpu.stat + /proc/self/stat.
/// Returns percentages relative to the container's CPU allocation (100% = all cgroup CPUs busy).
pub fn calculate_cgroup_usage(
    prev_cg: &CgroupCpuSample,
    curr_cg: &CgroupCpuSample,
    prev_proc: &ProcessCpuSample,
    curr_proc: &ProcessCpuSample,
    cgroup_cpus: usize,
) -> CpuUsage {
    let elapsed = curr_cg.timestamp.duration_since(prev_cg.timestamp);
    let elapsed_usec = elapsed.as_micros() as f64;
    if elapsed_usec <= 0.0 || cgroup_cpus == 0 {
        return CpuUsage { total_pct: 0.0, self_pct: 0.0, others_pct: 0.0 };
    }

    // Total container CPU: delta of usage_usec / (elapsed * cgroup_cpus)
    let total_delta_usec = curr_cg.usage_usec.saturating_sub(prev_cg.usage_usec) as f64;
    let total_pct = (total_delta_usec / (elapsed_usec * cgroup_cpus as f64)) * 100.0;

    // Self CPU: delta of (utime+stime) converted from jiffies to microseconds
    let tck = clk_tck() as f64;
    let self_delta_jiffies = (curr_proc.utime + curr_proc.stime)
        .saturating_sub(prev_proc.utime + prev_proc.stime) as f64;
    let self_delta_usec = self_delta_jiffies * (1_000_000.0 / tck);
    let self_pct = (self_delta_usec / (elapsed_usec * cgroup_cpus as f64)) * 100.0;

    let others_pct = (total_pct - self_pct).max(0.0);

    CpuUsage {
        total_pct: total_pct.clamp(0.0, 100.0),
        self_pct: self_pct.clamp(0.0, 100.0),
        others_pct: others_pct.clamp(0.0, 100.0),
    }
}

/// Detect effective CPU count, respecting cgroup limits.
/// Priority: cgroup v2 → cgroup v1 → system default.
pub fn detect_num_cpus() -> usize {
    if let Ok(content) = fs::read_to_string("/sys/fs/cgroup/cpu.max") {
        if let Some(cpus) = parse_cgroup_v2_cpu(&content) {
            return cpus;
        }
    }
    if let Ok(quota) = fs::read_to_string("/sys/fs/cgroup/cpu/cpu.cfs_quota_us") {
        if let Ok(period) = fs::read_to_string("/sys/fs/cgroup/cpu/cpu.cfs_period_us") {
            if let Some(cpus) = parse_cgroup_v1_cpu(&quota, &period) {
                return cpus;
            }
        }
    }
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

/// Count online CPUs from /proc/stat (counts "cpuN" lines, always shows host CPUs).
pub fn count_online_cpus() -> usize {
    if let Ok(content) = fs::read_to_string("/proc/stat") {
        count_online_cpus_from(content.as_str())
    } else {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
    }
}

/// Parse online CPU count from /proc/stat content.
pub fn count_online_cpus_from(content: &str) -> usize {
    let count = content
        .lines()
        .filter(|l| {
            l.starts_with("cpu")
                && !l.starts_with("cpu ")
                && l.as_bytes().get(3).map_or(false, |c| c.is_ascii_digit())
        })
        .count();
    if count > 0 { count } else { 1 }
}

/// Compute the scaling factor to convert host-wide /proc/stat percentages
/// to cgroup-relative percentages. Returns host_cpus / cgroup_cpus.
/// When no cgroup limit exists, returns 1.0.
pub fn cgroup_cpu_scale() -> f64 {
    let host = count_online_cpus();
    let cgroup = detect_num_cpus();
    if cgroup < host {
        host as f64 / cgroup as f64
    } else {
        1.0
    }
}

pub fn parse_cgroup_v2_cpu(content: &str) -> Option<usize> {
    let parts: Vec<&str> = content.trim().split_whitespace().collect();
    if parts.len() >= 2 && parts[0] != "max" {
        let max: f64 = parts[0].parse().ok()?;
        let period: f64 = parts[1].parse().ok()?;
        if period > 0.0 {
            let cpus = (max / period).ceil() as usize;
            return if cpus > 0 { Some(cpus) } else { None };
        }
    }
    None
}

pub fn parse_cgroup_v1_cpu(quota_content: &str, period_content: &str) -> Option<usize> {
    let quota: i64 = quota_content.trim().parse().ok()?;
    let period: i64 = period_content.trim().parse().ok()?;
    if quota > 0 && period > 0 {
        let cpus = ((quota as f64) / (period as f64)).ceil() as usize;
        return if cpus > 0 { Some(cpus) } else { None };
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_proc_stat ──

    #[test]
    fn test_parse_proc_stat_basic() {
        let input = "cpu  10132153 290696 3084719 46828483 16683 0 25195 0 0 0\ncpu0 ...\n";
        let s = parse_proc_stat(input).unwrap();
        assert_eq!(s.user, 10132153);
        assert_eq!(s.nice, 290696);
        assert_eq!(s.system, 3084719);
        assert_eq!(s.idle, 46828483);
        assert_eq!(s.iowait, 16683);
        assert_eq!(s.irq, 0);
        assert_eq!(s.softirq, 25195);
        assert_eq!(s.steal, 0);
        let expected_total = 10132153 + 290696 + 3084719 + 46828483 + 16683 + 25195;
        assert_eq!(s.total, expected_total);
    }

    #[test]
    fn test_parse_proc_stat_minimal_4_fields() {
        let input = "cpu  1000 200 300 9500\n";
        let s = parse_proc_stat(input).unwrap();
        assert_eq!(s.user, 1000);
        assert_eq!(s.idle, 9500);
        assert_eq!(s.iowait, 0);
        assert_eq!(s.total, 11000);
    }

    #[test]
    fn test_parse_proc_stat_no_cpu_line() {
        assert!(parse_proc_stat("intr 1234567\nctxt 9876543\n").is_err());
    }

    #[test]
    fn test_parse_proc_stat_empty() {
        assert!(parse_proc_stat("").is_err());
    }

    #[test]
    fn test_parse_proc_stat_insufficient_fields() {
        assert!(parse_proc_stat("cpu  100 200\n").is_err());
    }

    #[test]
    fn test_parse_proc_stat_skips_per_cpu_lines() {
        let input = "cpu  1000 0 0 9000 0 0 0 0 0 0\ncpu0 500 0 0 4500 0 0 0 0 0 0\n";
        let s = parse_proc_stat(input).unwrap();
        assert_eq!(s.total, 10000); // aggregate line, not per-cpu
    }

    // ── parse_self_stat ──

    #[test]
    fn test_parse_self_stat_basic() {
        let input = "1234 (server-sponge) S 1 1234 1234 0 -1 4194304 100 0 0 0 500 120 0 0 20 0 4 0 12345 67890 100 18446744073709551615 0 0 0 0 0 0 0 0 0 0 0 0 17 1 0 0 0 0 0 0 0 0 0 0 0 0 0";
        let s = parse_self_stat(input).unwrap();
        assert_eq!(s.utime, 500);
        assert_eq!(s.stime, 120);
    }

    #[test]
    fn test_parse_self_stat_spaces_in_comm() {
        let input = "1234 (my sponge app) S 1 1234 1234 0 -1 4194304 100 0 0 0 1000 250 0 0 20 0 4 0 12345 67890 100 18446744073709551615 0 0 0 0 0 0 0 0 0 0 0 0 17 1 0 0 0 0 0 0 0 0 0 0 0 0 0";
        let s = parse_self_stat(input).unwrap();
        assert_eq!(s.utime, 1000);
        assert_eq!(s.stime, 250);
    }

    #[test]
    fn test_parse_self_stat_parens_in_comm() {
        let input = "1234 (app(v2)) S 1 1234 1234 0 -1 4194304 100 0 0 0 750 80 0 0 20 0 4 0 12345 67890 100 18446744073709551615 0 0 0 0 0 0 0 0 0 0 0 0 17 1 0 0 0 0 0 0 0 0 0 0 0 0 0";
        let s = parse_self_stat(input).unwrap();
        assert_eq!(s.utime, 750);
        assert_eq!(s.stime, 80);
    }

    #[test]
    fn test_parse_self_stat_malformed() {
        assert!(parse_self_stat("no parens here").is_err());
    }

    #[test]
    fn test_parse_self_stat_too_short() {
        assert!(parse_self_stat("1 (a) S 1 1").is_err());
    }

    // ── calculate_usage ──

    #[test]
    fn test_usage_50_percent() {
        let prev = CpuSample { user: 0, nice: 0, system: 0, idle: 500, iowait: 0, irq: 0, softirq: 0, steal: 0, total: 1000 };
        let curr = CpuSample { user: 500, nice: 0, system: 0, idle: 1000, iowait: 0, irq: 0, softirq: 0, steal: 0, total: 2000 };
        let pp = ProcessCpuSample { utime: 0, stime: 0 };
        let cp = ProcessCpuSample { utime: 100, stime: 50 };
        let u = calculate_usage(&prev, &curr, &pp, &cp);
        assert!((u.total_pct - 50.0).abs() < 0.1, "total={}", u.total_pct);
        assert!((u.self_pct - 15.0).abs() < 0.1, "self={}", u.self_pct);
        assert!((u.others_pct - 35.0).abs() < 0.1, "others={}", u.others_pct);
    }

    #[test]
    fn test_usage_zero_delta() {
        let s = CpuSample { user: 100, nice: 0, system: 50, idle: 850, iowait: 0, irq: 0, softirq: 0, steal: 0, total: 1000 };
        let p = ProcessCpuSample { utime: 10, stime: 5 };
        let u = calculate_usage(&s, &s, &p, &p);
        assert_eq!(u.total_pct, 0.0);
        assert_eq!(u.self_pct, 0.0);
    }

    #[test]
    fn test_usage_100_percent_busy() {
        let prev = CpuSample { user: 0, nice: 0, system: 0, idle: 0, iowait: 0, irq: 0, softirq: 0, steal: 0, total: 0 };
        let curr = CpuSample { user: 800, nice: 100, system: 100, idle: 0, iowait: 0, irq: 0, softirq: 0, steal: 0, total: 1000 };
        let pp = ProcessCpuSample { utime: 0, stime: 0 };
        let cp = ProcessCpuSample { utime: 0, stime: 0 };
        let u = calculate_usage(&prev, &curr, &pp, &cp);
        assert!((u.total_pct - 100.0).abs() < 0.1);
    }

    #[test]
    fn test_usage_0_percent_all_idle() {
        let prev = CpuSample { user: 0, nice: 0, system: 0, idle: 0, iowait: 0, irq: 0, softirq: 0, steal: 0, total: 0 };
        let curr = CpuSample { user: 0, nice: 0, system: 0, idle: 1000, iowait: 0, irq: 0, softirq: 0, steal: 0, total: 1000 };
        let pp = ProcessCpuSample { utime: 0, stime: 0 };
        let cp = ProcessCpuSample { utime: 0, stime: 0 };
        let u = calculate_usage(&prev, &curr, &pp, &cp);
        assert!(u.total_pct.abs() < 0.1);
    }

    #[test]
    fn test_usage_others_nonnegative() {
        // self_pct > total_pct edge case
        let prev = CpuSample { user: 0, nice: 0, system: 0, idle: 500, iowait: 0, irq: 0, softirq: 0, steal: 0, total: 1000 };
        let curr = CpuSample { user: 200, nice: 0, system: 0, idle: 800, iowait: 0, irq: 0, softirq: 0, steal: 0, total: 2000 };
        let pp = ProcessCpuSample { utime: 0, stime: 0 };
        let cp = ProcessCpuSample { utime: 500, stime: 500 };
        let u = calculate_usage(&prev, &curr, &pp, &cp);
        assert!(u.others_pct >= 0.0);
    }

    #[test]
    fn test_usage_iowait_counts_as_idle() {
        let prev = CpuSample { user: 0, nice: 0, system: 0, idle: 0, iowait: 0, irq: 0, softirq: 0, steal: 0, total: 0 };
        let curr = CpuSample { user: 200, nice: 0, system: 0, idle: 600, iowait: 200, irq: 0, softirq: 0, steal: 0, total: 1000 };
        let pp = ProcessCpuSample { utime: 0, stime: 0 };
        let cp = ProcessCpuSample { utime: 0, stime: 0 };
        let u = calculate_usage(&prev, &curr, &pp, &cp);
        // idle+iowait = 800 → 80% idle → 20% usage
        assert!((u.total_pct - 20.0).abs() < 0.1, "total={}", u.total_pct);
    }

    #[test]
    fn test_usage_self_equals_total_when_only_process() {
        let prev = CpuSample { user: 0, nice: 0, system: 0, idle: 500, iowait: 0, irq: 0, softirq: 0, steal: 0, total: 1000 };
        let curr = CpuSample { user: 500, nice: 0, system: 0, idle: 1000, iowait: 0, irq: 0, softirq: 0, steal: 0, total: 2000 };
        let pp = ProcessCpuSample { utime: 0, stime: 0 };
        let cp = ProcessCpuSample { utime: 300, stime: 200 }; // 500 = all non-idle
        let u = calculate_usage(&prev, &curr, &pp, &cp);
        assert!((u.total_pct - 50.0).abs() < 0.1);
        assert!((u.self_pct - 50.0).abs() < 0.1);
        assert!(u.others_pct.abs() < 0.1, "others={}", u.others_pct);
    }

    // ── cgroup CPU detection ──

    #[test]
    fn test_cgroup_v2_limited() {
        assert_eq!(parse_cgroup_v2_cpu("200000 100000\n"), Some(2));
    }

    #[test]
    fn test_cgroup_v2_fractional() {
        assert_eq!(parse_cgroup_v2_cpu("150000 100000\n"), Some(2)); // ceil(1.5)
    }

    #[test]
    fn test_cgroup_v2_unlimited() {
        assert_eq!(parse_cgroup_v2_cpu("max 100000\n"), None);
    }

    #[test]
    fn test_cgroup_v2_single() {
        assert_eq!(parse_cgroup_v2_cpu("100000 100000\n"), Some(1));
    }

    #[test]
    fn test_cgroup_v2_four_cores() {
        assert_eq!(parse_cgroup_v2_cpu("400000 100000\n"), Some(4));
    }

    #[test]
    fn test_cgroup_v1_limited() {
        assert_eq!(parse_cgroup_v1_cpu("200000\n", "100000\n"), Some(2));
    }

    #[test]
    fn test_cgroup_v1_unlimited() {
        assert_eq!(parse_cgroup_v1_cpu("-1\n", "100000\n"), None);
    }

    #[test]
    fn test_cgroup_v1_fractional() {
        assert_eq!(parse_cgroup_v1_cpu("50000\n", "100000\n"), Some(1)); // ceil(0.5)
    }

    #[test]
    fn test_cgroup_v1_zero_period() {
        assert_eq!(parse_cgroup_v1_cpu("100000\n", "0\n"), None);
    }

    // ── Display ──

    #[test]
    fn test_cpu_usage_display() {
        let u = CpuUsage { total_pct: 70.5, self_pct: 40.2, others_pct: 30.3 };
        let s = format!("{}", u);
        assert!(s.contains("70.5") && s.contains("40.2") && s.contains("30.3"), "got: {}", s);
    }

    // ── count_online_cpus_from ──

    #[test]
    fn test_count_online_cpus_basic() {
        let input = "cpu  10000 200 300 9000 0 0 0 0 0 0\ncpu0 5000 100 150 4500\ncpu1 5000 100 150 4500\nintr 123456\n";
        assert_eq!(count_online_cpus_from(input), 2);
    }

    #[test]
    fn test_count_online_cpus_16_cores() {
        let mut input = String::from("cpu  100 0 0 900 0 0 0 0 0 0\n");
        for i in 0..16 {
            input.push_str(&format!("cpu{} 10 0 0 90\n", i));
        }
        input.push_str("intr 0\nctxt 0\n");
        assert_eq!(count_online_cpus_from(&input), 16);
    }

    #[test]
    fn test_count_online_cpus_no_per_cpu_lines() {
        // Only aggregate line, no per-cpu
        assert_eq!(count_online_cpus_from("cpu  100 0 0 900\nintr 0\n"), 1); // fallback
    }

    #[test]
    fn test_count_online_cpus_empty() {
        assert_eq!(count_online_cpus_from(""), 1);
    }

    #[test]
    fn test_count_online_cpus_ignores_cpufreq_like_lines() {
        // "cpuX" lines should match but "cpu " (aggregate) should not
        let input = "cpu  100 0 0 900\ncpu0 50 0 0 450\n";
        assert_eq!(count_online_cpus_from(input), 1);
    }

    // ── parse_cgroup_cpu_usage ──

    #[test]
    fn test_parse_cgroup_cpu_usage_basic() {
        let content = "usage_usec 245973894\nuser_usec 237505414\nsystem_usec 8468479\nnr_periods 2184\n";
        assert_eq!(parse_cgroup_cpu_usage(content), Some(245973894));
    }

    #[test]
    fn test_parse_cgroup_cpu_usage_zero() {
        assert_eq!(parse_cgroup_cpu_usage("usage_usec 0\n"), Some(0));
    }

    #[test]
    fn test_parse_cgroup_cpu_usage_missing() {
        assert_eq!(parse_cgroup_cpu_usage("user_usec 100\nsystem_usec 50\n"), None);
    }

    #[test]
    fn test_parse_cgroup_cpu_usage_empty() {
        assert_eq!(parse_cgroup_cpu_usage(""), None);
    }

    // ── calculate_cgroup_usage ──

    #[test]
    fn test_cgroup_usage_50pct_on_2_cores() {
        let now = Instant::now();
        let prev = CgroupCpuSample { usage_usec: 0, timestamp: now };
        // Simulate 1 second elapsed, 1_000_000 usec used on 2 cores = 50%
        let curr = CgroupCpuSample { usage_usec: 1_000_000, timestamp: now + std::time::Duration::from_secs(1) };
        let pp = ProcessCpuSample { utime: 0, stime: 0 };
        let cp = ProcessCpuSample { utime: 50, stime: 50 }; // 100 jiffies = 1sec at CLK_TCK=100
        let u = calculate_cgroup_usage(&prev, &curr, &pp, &cp, 2);
        assert!((u.total_pct - 50.0).abs() < 1.0, "total={}", u.total_pct);
        assert!((u.self_pct - 50.0).abs() < 1.0, "self={}", u.self_pct);
        assert!(u.others_pct < 1.0, "others={}", u.others_pct);
    }

    #[test]
    fn test_cgroup_usage_100pct_on_2_cores() {
        let now = Instant::now();
        let prev = CgroupCpuSample { usage_usec: 0, timestamp: now };
        // 2 seconds of CPU on 2 cores in 1 wall second = 100%
        let curr = CgroupCpuSample { usage_usec: 2_000_000, timestamp: now + std::time::Duration::from_secs(1) };
        let pp = ProcessCpuSample { utime: 0, stime: 0 };
        let cp = ProcessCpuSample { utime: 100, stime: 100 };
        let u = calculate_cgroup_usage(&prev, &curr, &pp, &cp, 2);
        assert!((u.total_pct - 100.0).abs() < 1.0, "total={}", u.total_pct);
    }

    #[test]
    fn test_cgroup_usage_zero_elapsed() {
        let now = Instant::now();
        let s = CgroupCpuSample { usage_usec: 100, timestamp: now };
        let p = ProcessCpuSample { utime: 10, stime: 5 };
        let u = calculate_cgroup_usage(&s, &s, &p, &p, 2);
        assert_eq!(u.total_pct, 0.0);
    }

    #[test]
    fn test_cgroup_usage_others_nonnegative() {
        let now = Instant::now();
        let prev = CgroupCpuSample { usage_usec: 0, timestamp: now };
        let curr = CgroupCpuSample { usage_usec: 500_000, timestamp: now + std::time::Duration::from_secs(1) };
        let pp = ProcessCpuSample { utime: 0, stime: 0 };
        // self_delta > total: impossible in practice but test clamping
        let cp = ProcessCpuSample { utime: 200, stime: 200 };
        let u = calculate_cgroup_usage(&prev, &curr, &pp, &cp, 2);
        assert!(u.others_pct >= 0.0);
    }
}
