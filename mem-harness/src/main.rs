//! mem-harness — peak-RSS benchmark for a single gix clone.
//!
//! Purpose: ground the bounded-memory work in measurement. Before we claim
//! to have bounded anything, we need to know what the current baseline is
//! on real inputs. Run this against a handful of repos (small, medium,
//! large, "known pathological") and record the numbers. Those are the
//! before-numbers. Every subsequent change in this fork should be judged
//! by moving those numbers, not by how it reads.
//!
//! Deliberately minimal:
//!   - one clone per invocation
//!   - no statistical aggregation, no baselines, no parallelism
//!   - peak RSS read once at end via `ru_maxrss` (Linux kernel tracks this
//!     for us; no polling needed)
//!   - output is a single machine-parseable line so a shell wrapper can
//!     loop over URLs
//!
//! The clone path mirrors what `git-vault-server` does: bare clone via
//! `gix::clone::PrepareFetch::fetch_then_checkout` with isolated open
//! options. If git-vault's call pattern changes, this harness should
//! change with it.

use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

#[derive(Parser, Debug)]
#[command(
    name = "mem-harness",
    about = "Clone a repo via gix and report peak RSS."
)]
struct Args {
    /// Remote URL or file:// path to clone from.
    url: String,

    /// Destination for the bare clone. Must not exist (will be created).
    dest: PathBuf,

    /// Hard memory cap applied to the clone via
    /// `gix::open::Options::with_memory_budget`. When exceeded, the
    /// instrumented memory-hot sites (currently: the decoded-object
    /// caches — see commits a5b21a1 and 91248d1) will refuse new
    /// allocations cleanly rather than grow past the cap. A clone that
    /// would have OOM-killed the host may return `OutOfBudget`
    /// instead; the harness reports that as a clean error line. Not
    /// yet enforced at pack-indexing or pack-receive sites (steps 5/6
    /// of the bounded-memory plan); those paths remain unbounded
    /// until those commits land.
    #[arg(long, value_name = "MB")]
    budget_mb: Option<u64>,
}

fn main() {
    let args = Args::parse();
    // Build the budget up-front so we can thread it into the clone
    // AND keep a clone of the handle for post-operation peak readout.
    // `MemoryBudget` is cheap to clone — both handles share the same
    // Arc-backed counter, so the peak we read here reflects everything
    // every cloned handle saw.
    let budget = match args.budget_mb {
        Some(mb) => gix::budget::MemoryBudget::bytes(mb * 1024 * 1024),
        None => gix::budget::MemoryBudget::unlimited(),
    };

    // Start a background thread that polls /proc/self/status every 50
    // ms and tracks the high-water mark of RssAnon — the process's
    // anonymous (heap + stack) resident memory, distinct from mmap'd
    // file pages. This is the metric that actually matters for OOM
    // prevention: file-backed pages (like the mmap'd pack) can be
    // evicted by the kernel under pressure and re-faulted later, so
    // they inflate `ru_maxrss` without causing OOM kills. Anonymous
    // memory cannot; it's backed only by the page file and counts
    // directly against physical RAM + swap.
    //
    // The poller runs for the whole duration of `run()` and stops
    // immediately after. 50 ms is fast enough to catch most transient
    // peaks during a clone (pack-receive, traversal, sort) without
    // being expensive — reading /proc/self/status costs microseconds.
    let stop_poll = Arc::new(AtomicBool::new(false));
    let peak_rss_anon_kb = Arc::new(AtomicU64::new(0));
    let poller = {
        let stop = Arc::clone(&stop_poll);
        let peak = Arc::clone(&peak_rss_anon_kb);
        thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                if let Some(kb) = read_rss_anon_kb() {
                    peak.fetch_max(kb, Ordering::AcqRel);
                }
                thread::sleep(Duration::from_millis(50));
            }
        })
    };

    let status = run(&args, budget.clone());

    stop_poll.store(true, Ordering::Relaxed);
    let _ = poller.join();
    let peak_anon_mb = peak_rss_anon_kb.load(Ordering::Acquire) / 1024;

    let budget_peak_mb = budget.peak_used() / (1024 * 1024);
    // Always emit a line, even on error. Keeps shell wrappers simple.
    match status {
        Ok(report) => println!(
            "{} budget_peak_mb={} peak_rss_anon_mb={}",
            report, budget_peak_mb, peak_anon_mb
        ),
        Err(e) => {
            // Peak RSS may still be meaningful on a partial/failed clone —
            // the allocation that caused the failure still counted. Print
            // what we can.
            let peak = peak_rss_mb().unwrap_or(0);
            println!(
                "url={:?} status=err:{} peak_rss_mb={} elapsed_s=? on_disk_bytes=? budget_mb={} budget_peak_mb={} peak_rss_anon_mb={}",
                args.url,
                // Keep the error on one line — this is a machine-parseable report.
                format!("{:#}", e).replace('\n', " "),
                peak,
                args.budget_mb.map(|n| n.to_string()).unwrap_or_else(|| "none".into()),
                budget_peak_mb,
                peak_anon_mb,
            );
            std::process::exit(1);
        }
    }
}

fn run(args: &Args, budget: gix::budget::MemoryBudget) -> Result<String> {
    if args.dest.exists() {
        anyhow::bail!("dest already exists: {}", args.dest.display());
    }

    let t0 = Instant::now();

    // Attach the configured budget to the Options we pass into
    // PrepareFetch. The budget rides along via the Options that get
    // handed to `ThreadSafeRepository::init_opts` internally, so every
    // budget-aware cache this clone constructs (pack cache, object
    // cache) will consult it. When `budget_mb` is `None` the caller
    // has already passed `MemoryBudget::unlimited()` — same behaviour
    // the harness had before the peak-reporting change.
    let open_opts = gix::open::Options::isolated().with_memory_budget(budget);

    let mut prepare = gix::clone::PrepareFetch::new(
        args.url.as_str(),
        &args.dest,
        gix::create::Kind::Bare,
        gix::create::Options::default(),
        open_opts,
    )
    .context("clone preparation failed")?;

    // git-vault today calls fetch_then_checkout followed by main_worktree.
    // That sequence is actually broken: main_worktree on a bare repo returns
    // 'Repository at ... is a bare repository and cannot have a main worktree
    // checkout'. The only test that would catch it (test_backup_clone_from_local)
    // is marked #[ignore]. This harness deliberately does not reproduce the
    // bug — we skip main_worktree on bare clones so we can measure the
    // interesting part (fetch + pack index build). git-vault has a TODO to
    // match: either stop calling main_worktree in the bare path, or switch
    // to a non-bare clone upstream.
    let (_checkout, _fetch_outcome) = prepare
        .fetch_then_checkout(
            gix::progress::Discard,
            &gix::interrupt::IS_INTERRUPTED,
        )
        .context("clone fetch failed")?;

    let elapsed_s = t0.elapsed().as_secs_f64();
    let peak = peak_rss_mb().unwrap_or(0);
    let on_disk_bytes = dir_size_bytes(&args.dest).unwrap_or(0);

    Ok(format!(
        "url={:?} status=ok peak_rss_mb={} elapsed_s={:.2} on_disk_bytes={} budget_mb={}",
        args.url,
        peak,
        elapsed_s,
        on_disk_bytes,
        args.budget_mb.map(|n| n.to_string()).unwrap_or_else(|| "none".into()),
    ))
}

/// Peak RSS of this process, in MiB, via getrusage. Linux reports this in
/// kilobytes (contrary to what POSIX says); on macOS and some BSDs it's
/// in bytes. This harness targets Linux — that's where the OOM-kill
/// problem actually bites — so we assume the Linux units.
fn peak_rss_mb() -> Option<u64> {
    // SAFETY: getrusage with RUSAGE_SELF and a valid out-pointer is safe.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    if rc != 0 {
        return None;
    }
    // ru_maxrss is in KB on Linux.
    let kb = usage.ru_maxrss as u64;
    Some(kb / 1024)
}

/// Read the current `RssAnon` field from `/proc/self/status`, in KiB.
///
/// `RssAnon` is the subset of resident set size backed by anonymous
/// memory — heap, stack, and anonymous mmaps. It excludes file-backed
/// pages (like the mmap'd pack file), which can be evicted by the
/// kernel under pressure and therefore don't cause OOM kills.
/// Available since Linux 4.5.
///
/// Returns None if the file isn't present (non-Linux) or the field
/// isn't found (very old kernels). Callers treat that as "unknown,
/// skip the data point" — the harness's peak tracker simply won't
/// update, which is the correct behaviour on platforms where this
/// metric isn't meaningful.
fn read_rss_anon_kb() -> Option<u64> {
    let contents = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in contents.lines() {
        if let Some(rest) = line.strip_prefix("RssAnon:") {
            let kb: u64 = rest.trim().split_whitespace().next()?.parse().ok()?;
            return Some(kb);
        }
    }
    None
}

/// Sum of file sizes under `root`, in bytes. Skips anything we can't stat
/// (permissions errors, broken symlinks, etc.) — this is a harness, not
/// an integrity checker.
fn dir_size_bytes(root: &std::path::Path) -> Result<u64> {
    let mut total: u64 = 0;
    for entry in walkdir::WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
        if entry.file_type().is_file() {
            if let Ok(md) = entry.metadata() {
                total = total.saturating_add(md.len());
            }
        }
    }
    Ok(total)
}
