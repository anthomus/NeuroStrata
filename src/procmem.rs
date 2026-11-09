//! The daemon's own resident memory, and a log of which tool calls grew it.
//!
//! One daemon serves every console, so its memory is the sum of everything
//! every client has asked of it. `neurostrata_storage_status` reports the
//! current and peak figures; `CallGuard` wraps one MCP tool call and appends a
//! JSON line to `memory-growth.log` when the call left the process at least
//! `NEUROSTRATA_RSS_LOG_MB` (default 50) larger than it found it. After a day
//! of normal use that file names the calls that drive growth.
//!
//! Calls run concurrently, so a line records `in_flight`: when it is above 1,
//! the growth may belong to a neighbouring call.

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

const MB: u64 = 1024 * 1024;
const DEFAULT_THRESHOLD_MB: u64 = 50;
/// The log is rewritten from empty once it passes this size, keeping the
/// previous file as `memory-growth.log.1`.
const MAX_LOG_BYTES: u64 = 4 * MB;

static IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sample {
    /// Resident set now. `None` where the platform gives no current figure.
    pub rss_bytes: Option<u64>,
    /// Highest resident set since the process started.
    pub peak_bytes: Option<u64>,
}

#[cfg(windows)]
pub fn sample() -> Sample {
    #[repr(C)]
    struct ProcessMemoryCounters {
        cb: u32,
        page_fault_count: u32,
        peak_working_set_size: usize,
        working_set_size: usize,
        quota_peak_paged_pool_usage: usize,
        quota_paged_pool_usage: usize,
        quota_peak_non_paged_pool_usage: usize,
        quota_non_paged_pool_usage: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentProcess() -> isize;
        fn K32GetProcessMemoryInfo(
            process: isize,
            counters: *mut ProcessMemoryCounters,
            cb: u32,
        ) -> i32;
    }
    let mut counters = ProcessMemoryCounters {
        cb: std::mem::size_of::<ProcessMemoryCounters>() as u32,
        page_fault_count: 0,
        peak_working_set_size: 0,
        working_set_size: 0,
        quota_peak_paged_pool_usage: 0,
        quota_paged_pool_usage: 0,
        quota_peak_non_paged_pool_usage: 0,
        quota_non_paged_pool_usage: 0,
        pagefile_usage: 0,
        peak_pagefile_usage: 0,
    };
    // SAFETY: GetCurrentProcess returns a pseudo-handle that needs no closing,
    // and `counters` is a correctly sized, writable struct whose `cb` says so.
    let ok = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) };
    if ok == 0 {
        return Sample::default();
    }
    Sample {
        rss_bytes: Some(counters.working_set_size as u64),
        peak_bytes: Some(counters.peak_working_set_size as u64),
    }
}

#[cfg(target_os = "linux")]
pub fn sample() -> Sample {
    let Ok(status) = std::fs::read_to_string("/proc/self/status") else {
        return Sample::default();
    };
    let kb = |key: &str| -> Option<u64> {
        status
            .lines()
            .find_map(|l| l.strip_prefix(key))
            .and_then(|rest| rest.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
            .map(|v| v * 1024)
    };
    Sample { rss_bytes: kb("VmRSS:"), peak_bytes: kb("VmHWM:") }
}

#[cfg(all(unix, not(target_os = "linux")))]
pub fn sample() -> Sample {
    // No current figure without platform-specific calls; the peak is what
    // getrusage offers. ru_maxrss is bytes on macOS and kilobytes elsewhere.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: `usage` is a valid, writable rusage.
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) } != 0 {
        return Sample::default();
    }
    let raw = usage.ru_maxrss as u64;
    let peak = if cfg!(target_os = "macos") { raw } else { raw * 1024 };
    Sample { rss_bytes: None, peak_bytes: Some(peak) }
}

#[cfg(not(any(windows, unix)))]
pub fn sample() -> Sample {
    Sample::default()
}

fn mb(bytes: Option<u64>) -> serde_json::Value {
    bytes.map_or(serde_json::Value::Null, |b| serde_json::json!(b / MB))
}

/// The figures `neurostrata_storage_status` reports. Null where the platform
/// has none, never a made-up zero.
pub fn status_json() -> serde_json::Value {
    let s = sample();
    serde_json::json!({
        "rss_mb": mb(s.rss_bytes),
        "peak_rss_mb": mb(s.peak_bytes),
        "growth_log": log_path().to_string_lossy(),
        "growth_log_threshold_mb": threshold_mb(),
    })
}

/// The storage status plus a `process` object: the daemon's resident and peak
/// memory, and where the per-call growth log is written.
pub fn status_with_process_memory<T: serde::Serialize>(status: &T) -> String {
    let mut value = match serde_json::to_value(status) {
        Ok(v) => v,
        Err(e) => return serde_json::json!({"ok": false, "error": e.to_string()}).to_string(),
    };
    if let Some(obj) = value.as_object_mut() {
        obj.insert("process".to_string(), status_json());
    }
    value.to_string()
}

fn threshold_mb() -> u64 {
    std::env::var("NEUROSTRATA_RSS_LOG_MB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_THRESHOLD_MB)
}

fn log_path() -> PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    home.join(".config").join("neurostrata").join("memory-growth.log")
}

/// True when `after` is at least `threshold_mb` above `before`. Unknown
/// figures never count as growth.
fn grew(before: Option<u64>, after: Option<u64>, threshold_mb: u64) -> bool {
    match (before, after) {
        (Some(b), Some(a)) => a.saturating_sub(b) >= threshold_mb * MB,
        _ => false,
    }
}

fn line(tool: &str, namespace: Option<&str>, before: Sample, after: Sample, in_flight: usize) -> String {
    serde_json::json!({
        "ts": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        "tool": tool,
        "namespace": namespace,
        "rss_before_mb": mb(before.rss_bytes),
        "rss_after_mb": mb(after.rss_bytes),
        "delta_mb": match (before.rss_bytes, after.rss_bytes) {
            (Some(b), Some(a)) => serde_json::json!((a as i64 - b as i64) / MB as i64),
            _ => serde_json::Value::Null,
        },
        "peak_rss_mb": mb(after.peak_bytes),
        "in_flight": in_flight,
    })
    .to_string()
}

fn append(path: &std::path::Path, line: &str) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if std::fs::metadata(path).map(|m| m.len() > MAX_LOG_BYTES).unwrap_or(false) {
        let mut rolled = path.as_os_str().to_owned();
        rolled.push(".1");
        let _ = std::fs::rename(path, PathBuf::from(rolled));
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{line}");
    }
}

/// Wraps one tool call. Created before the handler runs; on drop it samples
/// again and logs the call when the process grew past the threshold.
pub struct CallGuard {
    tool: String,
    namespace: Option<String>,
    before: Sample,
}

impl CallGuard {
    pub fn begin(tool: &str, namespace: Option<&str>) -> Self {
        IN_FLIGHT.fetch_add(1, Ordering::SeqCst);
        Self {
            tool: tool.to_string(),
            namespace: namespace.map(str::to_string),
            before: sample(),
        }
    }
}

impl Drop for CallGuard {
    fn drop(&mut self) {
        let in_flight = IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
        let after = sample();
        if grew(self.before.rss_bytes, after.rss_bytes, threshold_mb()) {
            append(
                &log_path(),
                &line(&self.tool, self.namespace.as_deref(), self.before, after, in_flight),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn growth_needs_both_figures_and_the_threshold() {
        assert!(grew(Some(100 * MB), Some(150 * MB), 50));
        assert!(!grew(Some(100 * MB), Some(149 * MB), 50));
        assert!(!grew(Some(150 * MB), Some(100 * MB), 50), "shrinking is not growth");
        assert!(!grew(None, Some(900 * MB), 50), "an unknown start is not a measurement");
        assert!(!grew(Some(100 * MB), None, 50));
    }

    #[test]
    fn this_platform_reports_a_resident_set() {
        let s = sample();
        #[cfg(any(windows, target_os = "linux"))]
        {
            assert!(s.rss_bytes.unwrap_or(0) > 0, "no resident set reported: {s:?}");
            assert!(s.peak_bytes.unwrap_or(0) >= s.rss_bytes.unwrap_or(0));
        }
        let _ = s;
    }

    #[test]
    fn a_log_line_is_strict_json_naming_the_call() {
        let before = Sample { rss_bytes: Some(1000 * MB), peak_bytes: Some(1000 * MB) };
        let after = Sample { rss_bytes: Some(1300 * MB), peak_bytes: Some(1300 * MB) };
        let v: serde_json::Value =
            serde_json::from_str(&line("neurostrata_search_memory", Some("example-project"), before, after, 2)).unwrap();
        assert_eq!(v["tool"], "neurostrata_search_memory");
        assert_eq!(v["namespace"], "example-project");
        assert_eq!(v["delta_mb"], 300);
        assert_eq!(v["in_flight"], 2);
    }

    #[test]
    fn unknown_figures_are_null_not_zero() {
        let v: serde_json::Value =
            serde_json::from_str(&line("t", None, Sample::default(), Sample::default(), 1)).unwrap();
        assert!(v["rss_after_mb"].is_null());
        assert!(v["delta_mb"].is_null());
        assert!(v["namespace"].is_null());
    }

    #[test]
    fn the_log_rolls_over_instead_of_growing_without_bound() {
        let dir = std::env::temp_dir().join(format!("procmem-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("memory-growth.log");
        std::fs::write(&path, vec![b'x'; (MAX_LOG_BYTES + 1) as usize]).unwrap();
        append(&path, "{\"fresh\":true}");
        assert_eq!(std::fs::read_to_string(&path).unwrap().trim(), "{\"fresh\":true}");
        assert!(dir.join("memory-growth.log.1").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_status_keeps_its_fields_and_gains_a_process_object() {
        let out = status_with_process_memory(&serde_json::json!({"dirty": false, "snapshots": 3}));
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["dirty"], false, "existing fields are untouched");
        assert_eq!(v["snapshots"], 3);
        let p = &v["process"];
        assert!(p.get("rss_mb").is_some() && p.get("peak_rss_mb").is_some());
        assert!(p["growth_log"].as_str().unwrap().ends_with("memory-growth.log"));
        assert!(p["growth_log_threshold_mb"].as_u64().unwrap() > 0);
    }

    #[test]
    fn the_guard_counts_calls_in_flight() {
        let before = IN_FLIGHT.load(Ordering::SeqCst);
        let g = CallGuard::begin("t", None);
        assert_eq!(IN_FLIGHT.load(Ordering::SeqCst), before + 1);
        drop(g);
        assert_eq!(IN_FLIGHT.load(Ordering::SeqCst), before);
    }
}
