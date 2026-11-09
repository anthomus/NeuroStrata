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
//! the growth may belong to a neighboring call and the line says
//! `"approximate": true`.
//!
//! Opt-in: the log and the sampler run only with `neurostrata-mcp daemon
//! --profile` or `NEUROSTRATA_PROFILE=1`. The resident figure in
//! `storage_status` and the advice level are always on.

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

const MB: u64 = 1024 * 1024;
const DEFAULT_THRESHOLD_MB: u64 = 50;
/// The log is rewritten from empty once it passes this size, keeping the
/// previous file as `memory-growth.log.1`.
const MAX_LOG_BYTES: u64 = 4 * MB;

/// Seconds between time-series rows when the environment does not say.
const DEFAULT_SAMPLE_SECS: u64 = 600;

static IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);
/// Tool calls begun since the last time-series row.
static CALLS: AtomicU64 = AtomicU64::new(0);

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

/// Marker the daemon puts in an error when the database buffer pool is full.
/// Harness plugins look for it to remind the agent to tell the user.
pub const ADVICE_MARKER_BUFFER_POOL: &str = "NEUROSTRATA_ADVICE[buffer_pool]";

/// Resident memory (MB) at which the daemon starts to mention it. Idle is about
/// 860 MB and one large search used to take it to about 1.9 GB, so these are
/// starting points; `advice` lines in the growth log show when they fire.
const DEFAULT_WATCH_MB: u64 = 1536;
/// Resident memory (MB) at which the agent is asked to raise it with the user.
const DEFAULT_ACT_MB: u64 = 2560;

/// Pool-full failures since the process started.
static POOL_FULL_ERRORS: AtomicU64 = AtomicU64::new(0);
/// The buffer pool size in force, in MB; 0 means the engine sized it.
static POOL_MB: AtomicU64 = AtomicU64::new(0);
/// The advice level last seen, so the growth log gets one line per change.
static LAST_LEVEL: AtomicUsize = AtomicUsize::new(0);

/// Called when the engine reports that its buffer pool is full.
pub fn note_pool_full() {
    POOL_FULL_ERRORS.fetch_add(1, Ordering::Relaxed);
}

/// Called once the daemon knows which pool size it is running with.
pub fn set_pool_mb(mb: u64) {
    POOL_MB.store(mb, Ordering::Relaxed);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Ok,
    Watch,
    Act,
}

impl Level {
    fn as_str(self) -> &'static str {
        match self {
            Level::Ok => "ok",
            Level::Watch => "watch",
            Level::Act => "act",
        }
    }
    fn index(self) -> usize {
        match self {
            Level::Ok => 0,
            Level::Watch => 1,
            Level::Act => 2,
        }
    }
}

fn env_mb(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(default)
}

/// How worried to be, and why. Pure, so the thresholds are testable. A pool-full
/// failure is always `Act`: queries are failing. Memory alone is `Watch` at the
/// first threshold and `Act` at the second. An unknown figure is never a reason.
pub fn assess(rss_mb: Option<u64>, pool_full_errors: u64, watch_mb: u64, act_mb: u64) -> (Level, String) {
    if pool_full_errors > 0 {
        return (
            Level::Act,
            format!("the database buffer pool filled up {pool_full_errors} time(s) since the daemon started, and the query that hit it failed"),
        );
    }
    match rss_mb {
        Some(r) if r >= act_mb => (Level::Act, format!("the daemon is using {r} MB of memory (act threshold {act_mb} MB)")),
        Some(r) if r >= watch_mb => (Level::Watch, format!("the daemon is using {r} MB of memory (watch threshold {watch_mb} MB)")),
        _ => (Level::Ok, String::new()),
    }
}

/// What the agent should do at this level. Empty when nothing is wrong.
fn suggestions(level: Level, pool_full_errors: u64, pool_mb: u64) -> Vec<String> {
    if level == Level::Ok {
        return Vec::new();
    }
    let mut out = vec!["Tell the user what you saw and ask before doing anything about it.".to_string()];
    if pool_full_errors > 0 {
        let now = if pool_mb == 0 { "sized by the engine".to_string() } else { format!("{pool_mb} MB") };
        out.push(format!(
            "Raise storage_hygiene.buffer_pool_mb in ~/.config/neurostrata/config.json (now {now}) or set NEUROSTRATA_BUFFER_POOL_MB; it takes effect when the daemon restarts."
        ));
    } else if level == Level::Act {
        out.push(
            "If it keeps growing, the buffer pool cap can be lowered (storage_hygiene.buffer_pool_mb, or NEUROSTRATA_BUFFER_POOL_MB) at some cost in query speed; a restart frees the memory now."
                .to_string(),
        );
    }
    out.push(
        "Restarting is the user's to do: `neurostrata-mcp shutdown` (graceful, can take minutes), then the next MCP connection starts a new daemon. It disconnects every IDE that shares the daemon."
            .to_string(),
    );
    out.push("Never kill the daemon process: memories not yet checkpointed are lost.".to_string());
    out
}

/// The `advice` object in `neurostrata_storage_status`. Also writes one `advice`
/// line to the growth log whenever the level changes, so the thresholds can be
/// tuned from real use.
fn advice_json(s: Sample) -> serde_json::Value {
    let rss = s.rss_bytes.map(|b| b / MB);
    let (watch, act) = (env_mb("NEUROSTRATA_RSS_WATCH_MB", DEFAULT_WATCH_MB), env_mb("NEUROSTRATA_RSS_ACT_MB", DEFAULT_ACT_MB));
    let pool_full = POOL_FULL_ERRORS.load(Ordering::Relaxed);
    let pool_mb = POOL_MB.load(Ordering::Relaxed);
    let (level, reason) = assess(rss, pool_full, watch, act);
    let previous = LAST_LEVEL.swap(level.index(), Ordering::Relaxed);
    // Not in tests: they must not write into the real growth log.
    if !cfg!(test) && previous != level.index() && profile_enabled() {
        append(
            &log_path(),
            &serde_json::json!({
                "kind": "advice",
                "ts": now_secs(),
                "level": level.as_str(),
                "reason": reason,
                "rss_mb": rss,
                "pool_full_errors": pool_full,
            })
            .to_string(),
        );
    }
    serde_json::json!({
        "level": level.as_str(),
        "reason": reason,
        "pool_full_errors": pool_full,
        "buffer_pool_mb": pool_mb,
        "watch_mb": watch,
        "act_mb": act,
        "suggestions": suggestions(level, pool_full, pool_mb),
    })
}

/// A short block for `neurostrata_get_snapshot`, which agents read first, or
/// `None` when memory is fine.
pub fn snapshot_notice() -> Option<String> {
    let advice = advice_json(sample());
    let level = advice["level"].as_str().unwrap_or("ok");
    if level == "ok" {
        return None;
    }
    let reason = advice["reason"].as_str().unwrap_or("");
    let mut out = format!("DAEMON MEMORY ADVISORY ({level}): {reason}.");
    if level == "watch" {
        out.push_str(" Nothing is failing. Mention it to the user if it matters to what they are doing.");
        return Some(out);
    }
    for (i, step) in advice["suggestions"].as_array().into_iter().flatten().filter_map(|v| v.as_str()).enumerate() {
        out.push_str(&format!("\n  {}. {}", i + 1, step));
    }
    Some(out)
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
        "advice": advice_json(s),
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

/// Set by `neurostrata-mcp daemon --profile`.
static PROFILE: AtomicBool = AtomicBool::new(false);

/// Turns profiling on for this process: the growth log and the sampler.
pub fn enable_profile() {
    PROFILE.store(true, Ordering::Relaxed);
}

/// The growth log and the sampler are opt-in: `neurostrata-mcp daemon
/// --profile`, or `NEUROSTRATA_PROFILE=1` (also `true`/`on`) for a daemon that
/// an IDE starts. The resident figure in `storage_status` and the advice level
/// stay on: they are one syscall and have nothing to switch.
fn profile_enabled() -> bool {
    PROFILE.load(Ordering::Relaxed)
        || matches!(
            std::env::var("NEUROSTRATA_PROFILE").ok().as_deref().map(str::trim),
            Some("1") | Some("true") | Some("on")
        )
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
        "kind": "call",
        "ts": now_secs(),
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
        // Process-wide RSS: a neighboring call may own part of the delta.
        "approximate": in_flight > 1,
    })
    .to_string()
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// One row of the time series: the process's memory at a moment, and how many
/// tool calls ran since the previous row. Slow growth that no single call
/// accounts for shows up here and nowhere else.
fn sample_line(s: Sample, calls_since_last: u64, in_flight: usize) -> String {
    serde_json::json!({
        "kind": "sample",
        "ts": now_secs(),
        "rss_mb": mb(s.rss_bytes),
        "peak_rss_mb": mb(s.peak_bytes),
        "calls_since_last": calls_since_last,
        "in_flight": in_flight,
    })
    .to_string()
}

fn sample_interval() -> Option<std::time::Duration> {
    let secs = std::env::var("NEUROSTRATA_RSS_SAMPLE_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_SAMPLE_SECS);
    (secs > 0).then(|| std::time::Duration::from_secs(secs))
}

/// Starts the time series: one row now, then one every
/// `NEUROSTRATA_RSS_SAMPLE_SECS` (default 600; 0 turns it off) until `stop`
/// changes. The caller awaits the returned handle on shutdown.
pub fn spawn_sampler(stop: tokio::sync::watch::Receiver<bool>) -> tokio::task::JoinHandle<()> {
    match sample_interval().filter(|_| profile_enabled()) {
        Some(every) => tokio::spawn(run_sampler(log_path(), every, stop)),
        None => tokio::spawn(async {}),
    }
}

async fn run_sampler(
    path: PathBuf,
    every: std::time::Duration,
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    loop {
        let calls = CALLS.swap(0, Ordering::Relaxed);
        append(&path, &sample_line(sample(), calls, IN_FLIGHT.load(Ordering::SeqCst)));
        // Evaluates the advice level, so a change is logged even when nobody
        // asked for the status.
        let _ = advice_json(sample());
        tokio::select! {
            _ = tokio::time::sleep(every) => {}
            _ = stop.changed() => break,
        }
    }
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
        CALLS.fetch_add(1, Ordering::Relaxed);
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
        if profile_enabled() && grew(self.before.rss_bytes, after.rss_bytes, threshold_mb()) {
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
    fn advice_levels_follow_memory_and_pool_failures() {
        let (w, a) = (1536, 2560);
        assert_eq!(assess(Some(860), 0, w, a).0, Level::Ok);
        assert_eq!(assess(Some(1535), 0, w, a).0, Level::Ok);
        assert_eq!(assess(Some(1536), 0, w, a).0, Level::Watch);
        assert_eq!(assess(Some(2559), 0, w, a).0, Level::Watch);
        assert_eq!(assess(Some(2560), 0, w, a).0, Level::Act);
        assert_eq!(assess(None, 0, w, a).0, Level::Ok, "an unknown figure is not a reason");
        let (level, reason) = assess(Some(500), 2, w, a);
        assert_eq!(level, Level::Act, "a failed query is always worth raising");
        assert!(reason.contains("2 time(s)"), "{reason}");
    }

    #[test]
    fn ok_has_no_suggestions_and_act_never_tells_the_agent_to_restart_it() {
        assert!(suggestions(Level::Ok, 0, 384).is_empty());
        let act = suggestions(Level::Act, 1, 384).join("\n");
        assert!(act.contains("ask before"), "{act}");
        assert!(act.contains("storage_hygiene.buffer_pool_mb"), "{act}");
        assert!(act.contains("384 MB"), "{act}");
        assert!(act.contains("neurostrata-mcp shutdown"), "{act}");
        assert!(act.contains("user's to do"), "{act}");
        assert!(act.contains("Never kill"), "{act}");
        let sized_by_engine = suggestions(Level::Act, 1, 0).join("\n");
        assert!(sized_by_engine.contains("sized by the engine"), "{sized_by_engine}");
    }

    #[test]
    fn the_status_carries_an_advice_object_that_is_explicit_when_all_is_well() {
        let out = status_with_process_memory(&serde_json::json!({"dirty": false}));
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let advice = &v["process"]["advice"];
        assert!(advice["level"].is_string(), "{advice}");
        assert!(advice["suggestions"].is_array());
        assert!(advice["watch_mb"].as_u64().unwrap() > 0 && advice["act_mb"].as_u64().unwrap() > advice["watch_mb"].as_u64().unwrap());
    }

    #[test]
    fn a_pool_full_failure_is_counted_and_surfaces_in_the_snapshot_notice() {
        let before = POOL_FULL_ERRORS.load(Ordering::Relaxed);
        note_pool_full();
        assert_eq!(POOL_FULL_ERRORS.load(Ordering::Relaxed), before + 1);
        let notice = snapshot_notice().expect("a pool failure must produce a notice");
        assert!(notice.starts_with("DAEMON MEMORY ADVISORY (act)"), "{notice}");
        assert!(notice.contains("1. Tell the user"), "{notice}");
        assert!(ADVICE_MARKER_BUFFER_POOL.starts_with("NEUROSTRATA_ADVICE["));
    }

    #[test]
    fn overlapping_calls_are_marked_approximate() {
        let s = Sample { rss_bytes: Some(MB), peak_bytes: Some(MB) };
        let solo: serde_json::Value = serde_json::from_str(&line("t", None, s, s, 1)).unwrap();
        let busy: serde_json::Value = serde_json::from_str(&line("t", None, s, s, 2)).unwrap();
        assert_eq!(solo["approximate"], false);
        assert_eq!(busy["approximate"], true);
    }

    #[test]
    fn profiling_is_off_unless_asked_for() {
        std::env::remove_var("NEUROSTRATA_PROFILE");
        assert!(!profile_enabled(), "off by default");
        for off in ["0", "off", "false", ""] {
            std::env::set_var("NEUROSTRATA_PROFILE", off);
            assert!(!profile_enabled(), "{off:?} is not a yes");
        }
        for on in ["1", "true", "on"] {
            std::env::set_var("NEUROSTRATA_PROFILE", on);
            assert!(profile_enabled(), "{on:?} turns it on");
        }
        std::env::remove_var("NEUROSTRATA_PROFILE");
        assert!(!profile_enabled());
        // `enable_profile()` is process-wide and one-way, so it is not
        // exercised here: it would leave profiling on for every other test.
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
    fn a_sample_row_is_strict_json_with_the_call_count() {
        let s = Sample { rss_bytes: Some(1800 * MB), peak_bytes: Some(2100 * MB) };
        let v: serde_json::Value = serde_json::from_str(&sample_line(s, 7, 1)).unwrap();
        assert_eq!(v["kind"], "sample");
        assert_eq!(v["rss_mb"], 1800);
        assert_eq!(v["peak_rss_mb"], 2100);
        assert_eq!(v["calls_since_last"], 7);
        let none: serde_json::Value = serde_json::from_str(&sample_line(Sample::default(), 0, 0)).unwrap();
        assert!(none["rss_mb"].is_null(), "an unknown figure is null, not zero");
    }

    #[tokio::test]
    async fn the_sampler_writes_rows_on_a_timer_and_stops_when_told() {
        let dir = std::env::temp_dir().join(format!("procmem-sampler-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("memory-growth.log");
        let _ = std::fs::remove_file(&path);
        let (tx, rx) = tokio::sync::watch::channel(false);
        let handle = tokio::spawn(run_sampler(path.clone(), std::time::Duration::from_millis(40), rx));
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        tx.send(true).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("the sampler stops promptly")
            .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let rows: Vec<serde_json::Value> =
            text.lines().map(|l| serde_json::from_str(l).expect("strict JSON")).collect();
        assert!(rows.len() >= 3, "a row at once and one per interval, got {}", rows.len());
        assert!(rows.iter().all(|r| r["kind"] == "sample"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Cost of the instrumentation itself, not a correctness test. Run:
    ///   cargo test --bin neurostrata-mcp overhead_probe -- --ignored --nocapture
    #[test]
    #[ignore]
    fn overhead_probe() {
        let n = 20_000u32;
        let t = std::time::Instant::now();
        for _ in 0..n {
            std::hint::black_box(sample());
        }
        let per_sample = t.elapsed() / n;
        let t = std::time::Instant::now();
        for _ in 0..n {
            let g = CallGuard::begin("neurostrata_search_memory", Some("example-project"));
            std::hint::black_box(&g);
        }
        let per_guard = t.elapsed() / n;
        println!("OVERHEAD sample() = {per_sample:?}, whole guard (begin + drop) = {per_guard:?}");
    }

    /// What does one embed call cost in resident memory, with the real model?
    /// Run (needs the model in the shared cache):
    ///   cargo test --release --bin neurostrata-mcp embed_call_probe -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn embed_call_probe() {
        use crate::traits::Embedder;
        let mut last = std::time::Instant::now();
        let mut report = |step: &str| {
            let s = sample();
            println!(
                "EMBED | {step:<22} | rss {:>5} MB peak {:>5} MB | {:>6} ms",
                s.rss_bytes.unwrap_or(0) / MB,
                s.peak_bytes.unwrap_or(0) / MB,
                last.elapsed().as_millis()
            );
            last = std::time::Instant::now();
        };
        report("start");
        let embedder = crate::embed::FastEmbedder::new().expect("model");
        report("model loaded");
        let short = "fn probe() { let x = 1; }";
        let long = "let value = compute(input); ".repeat(80);
        let long = &long[..2000];
        for i in 1..=4 {
            embedder.embed(short).await.unwrap();
            report(&format!("short embed {i}"));
        }
        for i in 1..=4 {
            embedder.embed(long).await.unwrap();
            report(&format!("2000-char embed {i}"));
        }
    }

    /// What do the guard's store reads cost, on a COPY of the database? Run:
    ///   NS_PROBE_DB=<dir holding a copy of ladybug.db> \
    ///   cargo test --release --bin neurostrata-mcp guard_scan_probe -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn guard_scan_probe() {
        use crate::traits::VectorStore;
        let dir = std::path::PathBuf::from(std::env::var("NS_PROBE_DB").expect("NS_PROBE_DB"));
        let mut last = std::time::Instant::now();
        let mut report = |step: &str, outcome: String| {
            let s = sample();
            println!(
                "SCAN | {step:<34} | rss {:>5} MB peak {:>5} MB | {:>6} ms | {outcome}",
                s.rss_bytes.unwrap_or(0) / MB,
                s.peak_bytes.unwrap_or(0) / MB,
                last.elapsed().as_millis()
            );
            last = std::time::Instant::now();
        };
        let store = crate::store::LadybugStore::new(&dir.join("ladybug.db"), 768).expect("open");
        report("opened", String::new());
        let types = vec!["rule".to_string(), "guard_rule".to_string()];
        let query = vec![0.05_f32; 768];
        for ns in ["NeuroStrata", "global"] {
            for pass in 1..=3 {
                let r = store.list(ns, None, Some("task")).await;
                report(&format!("{ns} list task p{pass}"), format!("{} rows", r.map(|v| v.len()).unwrap_or(0)));
            }
            for pass in 1..=3 {
                let r = store.search_by_types(ns, query.clone(), 3, &types).await;
                report(&format!("{ns} search_by_types p{pass}"), format!("{} rows", r.map(|v| v.len()).unwrap_or(0)));
            }
            // Does the Rust distance match the engine's array_distance?
            if let Ok(rows) = store.search_by_types(ns, query.clone(), 1, &types).await {
                if let Some(first) = rows.first() {
                    let vec_str = format!(
                        "[{}]",
                        query.iter().map(|f| format!("{f:?}")).collect::<Vec<_>>().join(",")
                    );
                    let q = format!(
                        "MATCH (m:Memory) WHERE m.id = '{}' RETURN array_distance(m.embedding, {})",
                        first.id.replace('\'', "''"),
                        vec_str
                    );
                    let engine = store
                        .with_conn(move |conn| Ok(conn.query(&q)?.next().map(|r| format!("{}", r[0]))))
                        .await;
                    println!("CHECK {ns}: rust {} vs engine {:?} for {}", first.score, engine, first.id);
                }
            }
            for pass in 1..=3 {
                let r = store.list(ns, None, Some("guard_rule")).await;
                report(&format!("{ns} list guard_rule p{pass}"), format!("{} rows", r.map(|v| v.len()).unwrap_or(0)));
            }
            for pass in 1..=2 {
                let r = store.search(ns, query.clone(), 3, true, true).await;
                report(&format!("{ns} OLD search p{pass}"), format!("{} rows", r.map(|v| v.len()).unwrap_or(0)));
            }
        }
    }

    /// Can the engine rank the guard's few rule rows without the memory cost?
    /// One variant per process, since the page cache never shrinks. Run, once
    /// per NS_PROBE_VARIANT in rust | eq | eq_ns | in:
    ///   NS_PROBE_DB=<dir with a COPY of ladybug.db> NS_PROBE_VARIANT=eq \
    ///   cargo test --release --bin neurostrata-mcp guard_db_rank_probe -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn guard_db_rank_probe() {
        let dir = std::path::PathBuf::from(std::env::var("NS_PROBE_DB").expect("NS_PROBE_DB"));
        let variant = std::env::var("NS_PROBE_VARIANT").expect("NS_PROBE_VARIANT");
        let mut last = std::time::Instant::now();
        let mut report = |step: &str, outcome: String| {
            let s = sample();
            println!(
                "RANK {variant:<6} | {step:<24} | rss {:>5} MB peak {:>5} MB | {:>6} ms | {outcome}",
                s.rss_bytes.unwrap_or(0) / MB,
                s.peak_bytes.unwrap_or(0) / MB,
                last.elapsed().as_millis()
            );
            last = std::time::Instant::now();
        };
        let store = crate::store::LadybugStore::new(&dir.join("ladybug.db"), 768).expect("open");
        report("opened", String::new());
        let query = vec![0.05_f32; 768];
        let vec_str = format!("[{}]", query.iter().map(|f| format!("{f:?}")).collect::<Vec<_>>().join(","));
        let variant_c = variant.clone();
        let outcome = store
            .with_conn(move |conn| {
                let mut ids: Vec<String> = Vec::new();
                for row in conn.query(
                    "MATCH (m:Memory) WHERE m.namespace = 'NeuroStrata' AND m.memory_type IN ['rule', 'guard_rule'] RETURN m.id",
                )? {
                    ids.push(format!("{}", row[0]));
                }
                let mut out = format!("{} ids;", ids.len());
                let mut best: Option<(f32, String)> = None;
                let mut note = |d: f32, id: &str| {
                    if best.as_ref().map_or(true, |b| d < b.0) {
                        best = Some((d, id.to_string()));
                    }
                };
                match variant_c.as_str() {
                    "in" => {
                        let list = ids.iter().map(|i| format!("'{i}'")).collect::<Vec<_>>().join(", ");
                        let q = format!("MATCH (m:Memory) WHERE m.id IN [{list}] RETURN m.id, array_distance(m.embedding, {vec_str}) AS dist");
                        for row in conn.query(&q)? {
                            if let lbug::Value::Float(d) = &row[1] { note(*d, &format!("{}", row[0])); }
                        }
                    }
                    "eq" | "eq_ns" => {
                        for id in &ids {
                            let filter = if variant_c == "eq_ns" {
                                format!("m.namespace = 'NeuroStrata' AND m.id = '{id}'")
                            } else {
                                format!("m.id = '{id}'")
                            };
                            let q = format!("MATCH (m:Memory) WHERE {filter} RETURN m.id, array_distance(m.embedding, {vec_str}) AS dist");
                            for row in conn.query(&q)? {
                                if let lbug::Value::Float(d) = &row[1] { note(*d, &format!("{}", row[0])); }
                            }
                        }
                    }
                    "rust" => {
                        for id in &ids {
                            let q = format!("MATCH (m:Memory) WHERE m.id = '{id}' RETURN m.id, m.embedding");
                            for row in conn.query(&q)? {
                                if let lbug::Value::Array(_, v) | lbug::Value::List(_, v) = &row[1] {
                                    let e: Vec<f32> = v.iter().filter_map(|x| if let lbug::Value::Float(f) = x { Some(*f) } else { None }).collect();
                                    let d = e.iter().zip(&query).map(|(a, b)| (a - b) * (a - b)).sum::<f32>().sqrt();
                                    note(d, &format!("{}", row[0]));
                                }
                            }
                        }
                    }
                    other => panic!("unknown variant {other}"),
                }
                out.push_str(&format!(" nearest {:?}", best));
                Ok(out)
            })
            .await
            .unwrap_or_else(|e| format!("ERROR {e}"));
        report("ranked", outcome);
    }

    /// How do peak memory and query time trade off against the buffer pool
    /// cap, on the user-facing search path? One process per cap, on a COPY of
    /// a database. NS_PROBE_POOL_MB=0 leaves the engine default. Run:
    ///   NS_PROBE_DB=<dir with a copy of ladybug.db> NS_PROBE_POOL_MB=512 \
    ///   cargo test --release --bin neurostrata-mcp pool_curve_probe -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn pool_curve_probe() {
        use crate::traits::VectorStore;
        let dir = std::path::PathBuf::from(std::env::var("NS_PROBE_DB").expect("NS_PROBE_DB"));
        let pool_mb: u64 = std::env::var("NS_PROBE_POOL_MB").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
        let path = dir.join("ladybug.db");
        let store = if pool_mb == 0 {
            crate::store::LadybugStore::new(&path, 768).expect("open")
        } else {
            let cfg = lbug::SystemConfig::default()
                .buffer_pool_size(pool_mb * MB)
                .throw_on_wal_replay_failure(true);
            crate::store::LadybugStore::with_system_config(&path, 768, cfg).expect("open")
        };
        let namespaces = store.list_namespaces().await.expect("namespaces");
        // Deterministic, varied unit-ish vectors so passes do not hit identical pages.
        let query = |seed: u32| -> Vec<f32> {
            let mut x = seed.wrapping_mul(2_654_435_761).wrapping_add(12_345);
            (0..768)
                .map(|_| {
                    x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    ((x >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * 0.1
                })
                .collect()
        };
        let types = vec!["rule".to_string(), "guard_rule".to_string()];
        let mut errors = 0usize;
        let mut pass_ms: Vec<u128> = Vec::new();
        for pass in 0..3u32 {
            let t = std::time::Instant::now();
            for (i, ns) in namespaces.iter().enumerate() {
                let q = query(pass * 31 + i as u32);
                if store.search(ns, q.clone(), 5, false, false).await.is_err() { errors += 1; }
                if store.list(ns, None, Some("task")).await.is_err() { errors += 1; }
                if store.search_by_types(ns, q, 3, &types).await.is_err() { errors += 1; }
            }
            pass_ms.push(t.elapsed().as_millis());
        }
        let s = sample();
        println!(
            "POOLCURVE pool={:>4} MB | peak {:>5} MB rss {:>5} MB | pass ms {:?} | errors {}",
            pool_mb,
            s.peak_bytes.unwrap_or(0) / MB,
            s.rss_bytes.unwrap_or(0) / MB,
            pass_ms,
            errors
        );
    }

    /// Where does a daemon-shaped process put its memory? One process, on a
    /// COPY of a database, resident memory after each stage with the real
    /// embedder and the default pool cap (bead neurostrata-bo32). Run:
    ///   NS_PROBE_DB=<dir with a copy of ladybug.db> NS_PROBE_POOL_MB=384 \
    ///   cargo test --release --bin neurostrata-mcp component_breakdown_probe -- --ignored --nocapture
    // Windows-only: the trim stage calls kernel32 heap APIs, which do not link elsewhere.
    #[cfg(windows)]
    #[tokio::test]
    #[ignore]
    async fn component_breakdown_probe() {
        use crate::traits::{Embedder, VectorStore};
        let dir = std::path::PathBuf::from(std::env::var("NS_PROBE_DB").expect("NS_PROBE_DB"));
        let pool_mb: u64 = std::env::var("NS_PROBE_POOL_MB").ok().and_then(|v| v.parse().ok()).unwrap_or(384);
        let mut last = std::time::Instant::now();
        let mut report = |step: &str| {
            let s = sample();
            println!(
                "BREAKDOWN | {step:<34} | rss {:>5} MB peak {:>5} MB | {:>6} ms",
                s.rss_bytes.unwrap_or(0) / MB,
                s.peak_bytes.unwrap_or(0) / MB,
                last.elapsed().as_millis()
            );
            last = std::time::Instant::now();
        };
        report("start");
        let cfg = lbug::SystemConfig::default()
            .buffer_pool_size(pool_mb * MB)
            .throw_on_wal_replay_failure(true);
        let store = crate::store::LadybugStore::with_system_config(&dir.join("ladybug.db"), 768, cfg).expect("open");
        report("store opened");
        let namespaces = store.list_namespaces().await.expect("namespaces");
        report("namespaces listed");
        let query = |seed: u32| -> Vec<f32> {
            let mut x = seed.wrapping_mul(2_654_435_761).wrapping_add(12_345);
            (0..768)
                .map(|_| {
                    x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    ((x >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * 0.1
                })
                .collect()
        };
        for (i, ns) in namespaces.iter().enumerate() {
            let _ = store.search(ns, query(i as u32), 5, false, false).await;
        }
        report("vector search, every namespace");
        for (i, ns) in namespaces.iter().enumerate() {
            let _ = store
                .search_with_domain(ns, query(i as u32 + 99), 5, false, false, &[], Some("needle_not_present"), true)
                .await;
        }
        report("exact-name search, every namespace");
        for ns in &namespaces {
            let _ = store.list(ns, None, Some("task")).await;
        }
        report("task list, every namespace");
        let embedder = crate::embed::FastEmbedder::new().expect("model");
        report("embedder loaded");
        for i in 0..4 {
            embedder.embed(&format!("probe text number {i}")).await.unwrap();
        }
        report("4 embeds");
        for (i, ns) in namespaces.iter().enumerate() {
            let _ = store.search(ns, query(i as u32 + 7), 5, false, false).await;
        }
        report("second vector search pass");
        // Separate "freed but kept by the allocator" from "still live".
        // HeapCompact asks every process heap to decommit free pages;
        // EmptyWorkingSet then trims what is resident but idle. The pages
        // EmptyWorkingSet drops come back on touch, so the pass after it
        // shows what is genuinely in use.
        #[link(name = "kernel32")]
        extern "system" {
            fn GetCurrentProcess() -> isize;
            fn GetProcessHeaps(count: u32, heaps: *mut isize) -> u32;
            fn HeapCompact(heap: isize, flags: u32) -> usize;
            fn K32EmptyWorkingSet(process: isize) -> i32;
        }
        let mut heaps = [0isize; 256];
        // SAFETY: `heaps` holds 256 writable slots and the count passed says so.
        let n = unsafe { GetProcessHeaps(heaps.len() as u32, heaps.as_mut_ptr()) } as usize;
        for h in heaps.iter().take(n.min(heaps.len())) {
            // SAFETY: each handle was just returned by GetProcessHeaps.
            unsafe { HeapCompact(*h, 0) };
        }
        report(&format!("after HeapCompact ({n} heaps)"));
        // SAFETY: the pseudo-handle from GetCurrentProcess needs no closing.
        let ok = unsafe { K32EmptyWorkingSet(GetCurrentProcess()) };
        report(&format!("after EmptyWorkingSet (ok={ok})"));
        for (i, ns) in namespaces.iter().enumerate() {
            let _ = store.search(ns, query(i as u32 + 13), 5, false, false).await;
        }
        report("search pass after trimming");
        let dbf = std::fs::metadata(dir.join("ladybug.db")).map(|m| m.len() / MB).unwrap_or(0);
        println!("BREAKDOWN | db file {dbf} MB, pool cap {pool_mb} MB, namespaces {}", namespaces.len());
    }

    /// What does listing a namespace cost against listing only its task rows
    /// (bead neurostrata-zoz2)? `handle_task_list` calls `list(ns, None, None)`
    /// and filters tasks in Rust. One process per variant, on a COPY of a
    /// database. NS_PROBE_VARIANT is `all` or `tasks`. Run:
    ///   NS_PROBE_DB=<dir> NS_PROBE_NS=<namespace> NS_PROBE_VARIANT=all \
    ///   cargo test --release --bin neurostrata-mcp task_list_variants_probe -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn task_list_variants_probe() {
        use crate::traits::VectorStore;
        let dir = std::path::PathBuf::from(std::env::var("NS_PROBE_DB").expect("NS_PROBE_DB"));
        let ns = std::env::var("NS_PROBE_NS").expect("NS_PROBE_NS");
        let variant = std::env::var("NS_PROBE_VARIANT").unwrap_or_else(|_| "all".to_string());
        let cfg = lbug::SystemConfig::default()
            .buffer_pool_size(384 * MB)
            .throw_on_wal_replay_failure(true);
        let store = crate::store::LadybugStore::with_system_config(&dir.join("ladybug.db"), 768, cfg).expect("open");
        println!("TASKLIST {variant} | start rss {} MB", sample().rss_bytes.unwrap_or(0) / MB);
        for pass in 1..=3 {
            let before = sample().rss_bytes.unwrap_or(0) / MB;
            let t = std::time::Instant::now();
            let rows = if variant == "pinned" {
                // Only rows whose metadata mentions `pinned`, as the golden-rule
                // collector needs (bead neurostrata-zoz2).
                let ns2 = ns.clone();
                let n = store
                    .with_conn(move |conn| {
                        let q = format!(
                            "MATCH (m:Memory) WHERE m.namespace = '{ns2}' AND m.metadata CONTAINS 'pinned' RETURN m.id, m.content, m.metadata"
                        );
                        let mut res = conn.query(&q)?;
                        let mut n = 0usize;
                        while let Some(_row) = res.next() {
                            n += 1;
                        }
                        Ok(n)
                    })
                    .await
                    .expect("pinned query");
                println!("TASKLIST pinned | matched {n}");
                Ok(Vec::new())
            } else if variant == "tasks" {
                store.list(&ns, None, Some("task")).await
            } else if variant == "generations" {
                // Rows per generation, ids only (bead neurostrata-ef8h): how
                // much of a namespace is superseded generations that `list`
                // hides. Counts only; no content.
                let ns2 = ns.clone();
                let (total, by_gen, unscoped) = store
                    .with_conn(move |conn| {
                        let q = format!("MATCH (m:Memory) WHERE m.namespace = '{ns2}' RETURN m.id");
                        let mut res = conn.query(&q)?;
                        let mut by_gen: std::collections::BTreeMap<u64, usize> = std::collections::BTreeMap::new();
                        let (mut total, mut unscoped) = (0usize, 0usize);
                        while let Some(row) = res.next() {
                            total += 1;
                            match crate::parser::ingest::split_scoped_id(&format!("{}", row[0])) {
                                Some((_, g, _)) => *by_gen.entry(g).or_insert(0) += 1,
                                None => unscoped += 1,
                            }
                        }
                        Ok((total, by_gen, unscoped))
                    })
                    .await
                    .expect("ids");
                println!("TASKLIST generations | rows in the store {total} | unscoped {unscoped} | rows per generation {by_gen:?}");
                let live = store.list(&ns, None, None).await.expect("list").len();
                println!("TASKLIST generations | rows `list` returns (reachable) {live}");
                Ok(Vec::new())
            } else if variant == "relink_full" || variant == "relink_lean" {
                // The rows `relink_edges` reads (bead neurostrata-ef8h): all of
                // them with content (`full`), or content only for rows that
                // are not auto-ingested (`lean`). Same columns as list_rows,
                // without its generation filter, so this over-reads slightly
                // for both variants alike.
                let lean = variant == "relink_lean";
                let ns2 = ns.clone();
                let n = store
                    .with_conn(move |conn| {
                        let mut n = 0usize;
                        let mut bytes = 0usize;
                        let queries = if lean {
                            vec![
                                format!("MATCH (m:Memory) WHERE m.namespace = '{ns2}' AND m.user_id = 'auto-ingestor' RETURN m.id, '', m.user_id, m.memory_type, m.agent_name, m.location, m.location_lines, m.metadata"),
                                format!("MATCH (m:Memory) WHERE m.namespace = '{ns2}' AND m.user_id <> 'auto-ingestor' RETURN m.id, m.content, m.user_id, m.memory_type, m.agent_name, m.location, m.location_lines, m.metadata"),
                            ]
                        } else {
                            vec![format!("MATCH (m:Memory) WHERE m.namespace = '{ns2}' RETURN m.id, m.content, m.user_id, m.memory_type, m.agent_name, m.location, m.location_lines, m.metadata")]
                        };
                        let mut held: Vec<Vec<String>> = Vec::new();
                        for q in queries {
                            let mut res = conn.query(&q)?;
                            while let Some(row) = res.next() {
                                let cells: Vec<String> = (0..8).map(|i| format!("{}", row[i])).collect();
                                bytes += cells.iter().map(|c| c.len()).sum::<usize>();
                                held.push(cells);
                                n += 1;
                            }
                        }
                        println!("TASKLIST {} | rows {n} | text held {} MB", if lean { "relink_lean" } else { "relink_full" }, bytes / (1024 * 1024));
                        drop(held);
                        Ok(n)
                    })
                    .await
                    .expect("relink query");
                let _ = n;
                Ok(Vec::new())
            } else if variant == "shape" {
                // Counts only: which row types a namespace holds, and how many
                // carry a generation-scoped id.
                let rows = store.list(&ns, None, None).await.expect("list");
                let mut by_type: std::collections::BTreeMap<String, (usize, usize)> = std::collections::BTreeMap::new();
                let mut gens: std::collections::BTreeMap<u64, usize> = std::collections::BTreeMap::new();
                for r in &rows {
                    let scoped = crate::parser::ingest::split_scoped_id(&r.id);
                    let e = by_type.entry(r.payload.memory_type.clone()).or_insert((0, 0));
                    e.0 += 1;
                    if let Some((_, g, _)) = scoped {
                        e.1 += 1;
                        *gens.entry(g).or_insert(0) += 1;
                    }
                }
                for (t, (n, scoped)) in &by_type {
                    println!("TASKLIST shape | type {t:<14} rows {n:>6} scoped {scoped:>6}");
                }
                println!("TASKLIST shape | scoped rows per generation {gens:?}");
                Ok(Vec::new())
            } else if variant == "generation_summary" {
                // What ingest reads now: one generation, slim rows (bead
                // neurostrata-lyrw). The generation is the highest one among
                // the directory rows `list` returns.
                let dirs = store.list(&ns, None, Some("directory")).await.expect("list dirs");
                let Some(gen) = dirs
                    .iter()
                    .filter_map(|r| crate::parser::ingest::split_scoped_id(&r.id).map(|x| x.1))
                    .max()
                else {
                    // Rows written before generations have no scoped ids, and
                    // ingest never reads them through this path.
                    println!("TASKLIST generation_summary | no generation-scoped directory rows");
                    return;
                };
                let rows = store.generation_summary(&ns, gen).await.expect("summary");
                println!("TASKLIST generation_summary | generation {gen} rows {}", rows.len());
                Ok(Vec::new())
            } else if variant == "ingest_nodes" {
                // The rows ingest's first pass actually reads: directory, file
                // and markdown nodes (bead neurostrata-lyrw).
                let mut rows = Vec::new();
                for t in ["directory", "file", "markdown"] {
                    rows.extend(store.list(&ns, None, Some(t)).await.expect("list by type"));
                }
                Ok(rows)
            } else {
                store.list(&ns, None, None).await
            }
            .expect("list");
            let tasks = rows.iter().filter(|r| r.payload.memory_type == "task").count();
            let after = sample().rss_bytes.unwrap_or(0) / MB;
            println!(
                "TASKLIST {variant} | pass {pass} | rows {} tasks {tasks} | rss {before} -> {after} MB (+{}) | {} ms",
                rows.len(),
                after.saturating_sub(before),
                t.elapsed().as_millis()
            );
        }
        println!("TASKLIST {variant} | peak {} MB", sample().peak_bytes.unwrap_or(0) / MB);
    }

    /// What a whole `ingest_directory` run costs when a generation already
    /// exists (bead neurostrata-va1y, for neurostrata-lyrw). Copies
    /// NS_PROBE_DIR to a temp tree, ingests it, then adds one file and ingests
    /// again NS_PROBE_PASSES-1 times, so every later pass reads the previous
    /// generation twice (hash/stamp map, then carry-forward) with older
    /// generations still in the store. Zero embedder: the reads do not depend on
    /// it. Prints the highest resident set seen inside each pass. One process
    /// per code state. Run:
    ///   NS_PROBE_DIR=<dir> NS_PROBE_PASSES=4 \
    ///   cargo test --release --bin neurostrata-mcp ingest_reads_probe -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn ingest_reads_probe() {
        use crate::traits::{Embedder, VectorStore};
        use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
        use std::sync::Arc;

        struct Zero;
        #[async_trait::async_trait]
        impl Embedder for Zero {
            async fn embed(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
                Ok(vec![0.0; 4])
            }
            fn dimensions(&self) -> usize {
                4
            }
        }

        fn copy_tree(from: &std::path::Path, to: &std::path::Path) -> usize {
            std::fs::create_dir_all(to).expect("create dir");
            let mut files = 0;
            for entry in std::fs::read_dir(from).expect("read dir").flatten() {
                let name = entry.file_name();
                let (src, dst) = (entry.path(), to.join(&name));
                if src.is_dir() {
                    if matches!(name.to_string_lossy().as_ref(), "target" | ".git" | "node_modules") {
                        continue;
                    }
                    files += copy_tree(&src, &dst);
                } else if std::fs::copy(&src, &dst).is_ok() {
                    files += 1;
                }
            }
            files
        }

        let source = std::path::PathBuf::from(std::env::var("NS_PROBE_DIR").expect("NS_PROBE_DIR"));
        let passes: usize = std::env::var("NS_PROBE_PASSES").ok().and_then(|v| v.parse().ok()).unwrap_or(4);
        let pid = std::process::id();
        let tree = std::env::temp_dir().join(format!("ns-reads-probe-tree-{pid}"));
        let db = std::env::temp_dir().join(format!("ns-reads-probe-db-{pid}"));
        std::fs::create_dir_all(&db).expect("create db dir");
        let files = copy_tree(&source, &tree);
        println!("READS | tree copied: {files} files");

        let cfg = lbug::SystemConfig::default()
            .buffer_pool_size(384 * MB)
            .throw_on_wal_replay_failure(true);
        let store: Arc<dyn VectorStore> =
            Arc::new(crate::store::LadybugStore::with_system_config(&db.join("ladybug.db"), 4, cfg).expect("open"));
        store.init("probe").await.expect("init");
        let schema = crate::parser::schema::ParserSchema::load(include_str!("schema.json")).expect("schema");
        let embedder: Arc<dyn Embedder> = Arc::new(Zero);

        println!("READS | start rss {} MB", sample().rss_bytes.unwrap_or(0) / MB);
        for pass in 1..=passes {
            if pass > 1 {
                std::fs::write(tree.join(format!("probe_added_{pass}.rs")), format!("pub fn probe_{pass}() {{}}\n"))
                    .expect("add a file");
            }
            let max = Arc::new(AtomicU64::new(0));
            let stop = Arc::new(AtomicBool::new(false));
            let watcher = {
                let (max, stop) = (max.clone(), stop.clone());
                std::thread::spawn(move || {
                    while !stop.load(Ordering::SeqCst) {
                        max.fetch_max(sample().rss_bytes.unwrap_or(0) / MB, Ordering::SeqCst);
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                })
            };
            let before = sample().rss_bytes.unwrap_or(0) / MB;
            let t = std::time::Instant::now();
            crate::parser::ingest::ingest_directory(&tree, &schema, embedder.clone(), store.clone(), "probe", None, &[])
                .await
                .expect("ingest");
            let elapsed = t.elapsed();
            stop.store(true, Ordering::SeqCst);
            watcher.join().expect("watcher");
            let after = sample().rss_bytes.unwrap_or(0) / MB;
            println!(
                "READS | pass {pass} | rss {before} -> {after} MB | highest in pass {} MB | {} ms",
                max.load(Ordering::SeqCst),
                elapsed.as_millis()
            );
        }
        println!("READS | process peak {} MB", sample().peak_bytes.unwrap_or(0) / MB);
        std::fs::remove_dir_all(&tree).ok();
        std::fs::remove_dir_all(&db).ok();
    }

    /// How long does a writer wait while `repair_namespace_hygiene` runs, and
    /// how long does the repair take? On a COPY of a database; a second task
    /// writes one small row every 50 ms and records each write's latency
    /// (bead neurostrata-w9pr). Run:
    ///   NS_PROBE_DB=<dir with a copy of ladybug.db> NS_PROBE_NS=NeuroStrata \
    ///   cargo test --release --bin neurostrata-mcp hygiene_writer_wait_probe -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn hygiene_writer_wait_probe() {
        use crate::traits::{MemoryPayload, VectorStore};
        let dir = std::path::PathBuf::from(std::env::var("NS_PROBE_DB").expect("NS_PROBE_DB"));
        let ns = std::env::var("NS_PROBE_NS").unwrap_or_else(|_| "NeuroStrata".to_string());
        let cfg = lbug::SystemConfig::default()
            .buffer_pool_size(384 * MB)
            .throw_on_wal_replay_failure(true);
        let store = std::sync::Arc::new(
            crate::store::LadybugStore::with_system_config(&dir.join("ladybug.db"), 768, cfg).expect("open"),
        );
        store.init("hygiene_probe_ns").await.expect("init");
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let writer = {
            let (store, stop) = (store.clone(), stop.clone());
            tokio::spawn(async move {
                let mut lat: Vec<u128> = Vec::new();
                let mut i = 0u32;
                while !stop.load(Ordering::SeqCst) {
                    let payload = MemoryPayload {
                        content: format!("writer probe {i}"),
                        user_id: "probe".to_string(),
                        memory_type: "fact".to_string(),
                        agent_name: Some("probe".to_string()),
                        location: String::new(),
                        location_lines: String::new(),
                        metadata: serde_json::json!({}),
                    };
                    let t = std::time::Instant::now();
                    let r = store.upsert("hygiene_probe_ns", &format!("w-{i}"), vec![0.0; 768], payload).await;
                    lat.push(if r.is_ok() { t.elapsed().as_millis() } else { u128::MAX });
                    i += 1;
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                lat
            })
        };
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let t = std::time::Instant::now();
        let result = store.repair_namespace_hygiene(&ns).await;
        let repair_ms = t.elapsed().as_millis();
        stop.store(true, Ordering::SeqCst);
        let mut lat = writer.await.expect("writer");
        lat.sort_unstable();
        let pct = |p: usize| lat.get((lat.len().saturating_sub(1)) * p / 100).copied().unwrap_or(0);
        println!(
            "HYGIENE | repair {repair_ms} ms result {:?} | writer n={} p50 {} ms p95 {} ms max {} ms (u128::MAX = failed)",
            result.map_err(|e| e.to_string()),
            lat.len(),
            pct(50),
            pct(95),
            lat.last().copied().unwrap_or(0)
        );
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
