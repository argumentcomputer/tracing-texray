//! Process-tree resident-set sampler.
//!
//! The per-span `track_ram` reads in [`crate::tracker`] sample
//! `/proc/self/status` — only THIS process. Workloads that fan out into child
//! processes hide most of their memory from that view: Zisk's ASM
//! microservices, for instance, mmap large ROMs in separate PIDs that come and
//! go within a single span, so enter/exit reads of the parent miss them
//! entirely.
//!
//! This sampler runs a background thread that periodically sums `VmRSS` across
//! the whole process subtree (self + all transitive descendants) and records
//! the high-water mark in a global atomic, so a consumer can report an accurate
//! peak regardless of where the memory actually lives. Linux-only; on other
//! platforms the peak stays `0`.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

static PEAK_TREE_RSS_BYTES: AtomicU64 = AtomicU64::new(0);
static SAMPLER_STARTED: AtomicBool = AtomicBool::new(false);

/// Peak total resident-set size (bytes) observed across this process's tree
/// since the sampler started (or since the last [`reset_peak_tree_rss`]). `0`
/// if the sampler was never started or on a non-Linux platform.
pub fn peak_tree_rss_bytes() -> u64 {
    PEAK_TREE_RSS_BYTES.load(Ordering::Relaxed)
}

/// Reset the recorded peak to the current tree RSS. Call at the start of a phase
/// to measure that phase's peak in isolation.
pub fn reset_peak_tree_rss() {
    PEAK_TREE_RSS_BYTES.store(current_tree_rss_bytes(), Ordering::Relaxed);
}

/// Start the background tree-RSS sampler (idempotent — extra calls are no-ops).
/// `interval` is the sampling period: shorter catches briefer spikes at higher
/// CPU cost. Runs on a daemon thread that exits with the process. Takes an
/// immediate sample first so a consumer that reads back quickly still sees a
/// value.
pub fn start(interval: Duration) {
    if SAMPLER_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    bump_peak(current_tree_rss_bytes());
    let _ = std::thread::Builder::new()
        .name("texray-rss".into())
        .spawn(move || {
            loop {
                bump_peak(current_tree_rss_bytes());
                std::thread::sleep(interval);
            }
        });
}

fn bump_peak(sample: u64) {
    PEAK_TREE_RSS_BYTES.fetch_max(sample, Ordering::Relaxed);
}

/// Sum `VmRSS` (bytes) over this process and every transitive descendant, from
/// a single `/proc` scan. Each `/proc/<pid>/status` yields both `PPid:` (to
/// rebuild the tree) and `VmRSS:` (its resident size), so no page-size or libc
/// plumbing is needed.
#[cfg(target_os = "linux")]
fn current_tree_rss_bytes() -> u64 {
    let mut ppid_of: HashMap<u32, u32> = HashMap::new();
    let mut rss_of: HashMap<u32, u64> = HashMap::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return 0;
    };
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        // A process can exit mid-scan; a failed read just drops that PID.
        let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status"))
        else {
            continue;
        };
        let mut ppid = 0u32;
        let mut rss = 0u64;
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("PPid:") {
                ppid = rest.trim().parse().unwrap_or(0);
            } else if let Some(rest) = line.strip_prefix("VmRSS:") {
                rss = parse_kb(rest).saturating_mul(1024);
            }
        }
        ppid_of.insert(pid, ppid);
        rss_of.insert(pid, rss);
    }

    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for (&pid, &ppid) in &ppid_of {
        children.entry(ppid).or_default().push(pid);
    }

    // Depth-first over self + descendants; `seen` guards against a PID-reuse
    // cycle in the snapshot.
    let mut total = 0u64;
    let mut seen: HashSet<u32> = HashSet::new();
    let mut stack = vec![std::process::id()];
    while let Some(pid) = stack.pop() {
        if !seen.insert(pid) {
            continue;
        }
        total = total.saturating_add(rss_of.get(&pid).copied().unwrap_or(0));
        if let Some(kids) = children.get(&pid) {
            stack.extend(kids);
        }
    }
    total
}

#[cfg(not(target_os = "linux"))]
fn current_tree_rss_bytes() -> u64 {
    0
}

/// Parse the leading integer (kilobytes) from a `/proc/<pid>/status` value line
/// like `\t 12345 kB`.
#[cfg(target_os = "linux")]
fn parse_kb(s: &str) -> u64 {
    s.split_ascii_whitespace()
        .next()
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    /// The tree peak must include memory resident in a *child* process — the
    /// property `/proc/self/status` alone can't provide and the reason this
    /// sampler exists.
    #[test]
    fn tree_peak_includes_child_process() {
        start(Duration::from_millis(20));
        // A child that touches ~200 MB so the pages are actually resident, then
        // lingers long enough to be sampled. `python3` is available on CI/dev.
        let mut child = std::process::Command::new("python3")
            .args([
                "-c",
                "b=bytearray(200*1024*1024)\n\
                 for i in range(0,len(b),4096): b[i]=1\n\
                 import time; time.sleep(1.5)",
            ])
            .spawn()
            .expect("spawn child");
        std::thread::sleep(Duration::from_millis(800));
        let peak = peak_tree_rss_bytes();
        let _ = child.wait();
        assert!(
            peak >= 150 * 1024 * 1024,
            "tree peak {peak} bytes did not reflect the child's ~200 MB",
        );
    }
}
