//! Launch-phase timing instrumentation.
//!
//! Strictly observational: phases are recorded, rendered, and optionally
//! appended to a JSONL file. Nothing in the launch path reads a timing back, so
//! a wrong measurement can never change what the session does.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use capscode_core::PhaseTiming;

/// Append one JSON record per launch to this path. Opt-in: the record contains
/// the workspace path and file counts, so nothing is written unless asked.
const JSONL_ENV: &str = "CAPSCODE_TIMING_JSONL";
/// Print the phase table without `--verbose`.
const TABLE_ENV: &str = "CAPSCODE_TIMING";

static PROCESS_START: OnceLock<Instant> = OnceLock::new();
/// Steps that run before the driver exists (config load, keychain read, runtime
/// detection). Collected globally because there is no profile to hold them yet.
static PRELUDE: Mutex<Vec<PhaseTiming>> = Mutex::new(Vec::new());
static PRELUDE_MARK: Mutex<Option<Instant>> = Mutex::new(None);

/// Called once from `main` so the launch can be priced from process start.
pub fn mark_process_start() {
    let _ = PROCESS_START.set(Instant::now());
}

/// Close a pre-driver step. Silently does nothing if `mark_process_start` was
/// never called (tests, other subcommands).
pub fn prelude_lap(phase: &str) {
    let Some(start) = PROCESS_START.get() else {
        return;
    };
    let now = Instant::now();
    let mut mark = PRELUDE_MARK.lock().unwrap_or_else(|e| e.into_inner());
    let since = mark.unwrap_or(*start);
    if let Ok(mut phases) = PRELUDE.lock() {
        phases.push(PhaseTiming::new(phase, now.duration_since(since)));
    }
    *mark = Some(now);
}

fn drain_prelude() -> (Vec<PhaseTiming>, Option<Instant>) {
    let phases = PRELUDE
        .lock()
        .map(|mut phases| std::mem::take(&mut *phases))
        .unwrap_or_default();
    let mark = PRELUDE_MARK
        .lock()
        .ok()
        .and_then(|mark| *mark)
        .or_else(|| PROCESS_START.get().copied());
    (phases, mark)
}

pub struct LaunchProfile {
    mark: Instant,
    phases: Vec<PhaseTiming>,
    session: String,
    repo: PathBuf,
    runtime: String,
    net_mode: String,
    ingress_mode: Option<String>,
    copied_bytes: u64,
    copied_files: u64,
}

impl LaunchProfile {
    pub fn new(session: &str, repo: &Path, runtime: &str, net_mode: &str) -> Self {
        let now = Instant::now();
        let (mut phases, mark) = drain_prelude();
        if let Some(mark) = mark {
            phases.push(PhaseTiming::new("driver-setup", now.duration_since(mark)));
        }
        Self {
            mark: now,
            phases,
            session: session.to_string(),
            repo: repo.to_path_buf(),
            runtime: runtime.to_string(),
            net_mode: net_mode.to_string(),
            ingress_mode: None,
            copied_bytes: 0,
            copied_files: 0,
        }
    }

    /// Close the current step under `phase` and open the next one. Every stretch
    /// of wall clock between two laps is attributed, so nothing hides.
    pub fn lap(&mut self, phase: &str) {
        let now = Instant::now();
        self.phases
            .push(PhaseTiming::new(phase, now.duration_since(self.mark)));
        self.mark = now;
    }

    /// Splice the git engine's own sub-phases in under an `ingress/` prefix, and
    /// keep the tree stats the ingress cost model is judged against.
    pub fn absorb_ingress(&mut self, report: &capscode_core::IngressReport) {
        self.ingress_mode = Some(
            match report.receipt {
                capscode_core::IngressReceipt::GitSnapshot { .. } => "GitSnapshot",
                capscode_core::IngressReceipt::Shadow { .. } => "Shadow",
            }
            .to_string(),
        );
        self.copied_bytes = report.copied_bytes;
        self.copied_files = report.copied_files;
        for timing in &report.phases {
            self.phases.push(PhaseTiming::new(
                format!("ingress/{}", timing.phase),
                timing.elapsed,
            ));
        }
    }

    /// Wall clock of the launch. Ingress sub-phases are a breakdown of the
    /// `ingress` phase, so summing them in would double-count it.
    pub fn total(&self) -> Duration {
        self.phases
            .iter()
            .filter(|p| !is_breakdown(&p.phase))
            .map(|p| p.elapsed)
            .sum()
    }

    /// True when the operator asked for the table (`--verbose` or `CAPSCODE_TIMING`).
    pub fn wants_table(verbose: bool) -> bool {
        verbose || std::env::var_os(TABLE_ENV).is_some_and(|value| !value.is_empty())
    }

    pub fn render_table(&self) -> String {
        let total = self.total().as_secs_f64();
        let width = self
            .phases
            .iter()
            .map(|p| p.phase.len())
            .max()
            .unwrap_or(0)
            .max(12);
        let mut out = format!(
            "launch profile — {total:.2}s total ({} files, {} bytes, {}, {}, {})\n",
            self.copied_files,
            self.copied_bytes,
            self.ingress_mode.as_deref().unwrap_or("no-ingress"),
            self.runtime,
            self.net_mode,
        );
        for phase in &self.phases {
            let seconds = phase.elapsed.as_secs_f64();
            let share = if total > 0.0 {
                seconds / total * 100.0
            } else {
                0.0
            };
            let indent = if is_breakdown(&phase.phase) {
                "    "
            } else {
                "  "
            };
            out.push_str(&format!(
                "{indent}{:<width$}  {seconds:>7.2}s  {share:>5.1}%\n",
                phase.phase,
                width = width
            ));
        }
        out.trim_end().to_string()
    }

    /// Append the run to `CAPSCODE_TIMING_JSONL` when set. Best-effort: a
    /// benchmarking aid must never be able to fail a launch.
    pub fn write_jsonl(&self) {
        let Some(path) = std::env::var_os(JSONL_ENV).filter(|value| !value.is_empty()) else {
            return;
        };
        let record = serde_json::json!({
            "recorded_at": unix_seconds(),
            "session": self.session,
            "repo": self.repo,
            "runtime": self.runtime,
            "net_mode": self.net_mode,
            "ingress_mode": self.ingress_mode,
            "copied_bytes": self.copied_bytes,
            "copied_files": self.copied_files,
            "total_ms": self.total().as_millis(),
            "phases": self.phases.iter().map(|phase| serde_json::json!({
                "phase": phase.phase,
                "ms": phase.elapsed.as_millis(),
            })).collect::<Vec<_>>(),
        });
        let Ok(mut line) = serde_json::to_string(&record) else {
            return;
        };
        line.push('\n');
        let opened = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path);
        match opened {
            Ok(mut file) => {
                if let Err(error) = file.write_all(line.as_bytes()) {
                    tracing::warn!(error = %error, "could not append launch timing record");
                }
            }
            Err(error) => tracing::warn!(error = %error, "could not open launch timing file"),
        }
    }
}

/// Sub-phases of an already-billed parent phase; shown, never summed.
fn is_breakdown(phase: &str) -> bool {
    phase.starts_with("ingress/")
}

fn unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile() -> LaunchProfile {
        LaunchProfile::new("sess-1", Path::new("/tmp/repo"), "docker", "containment")
    }

    #[test]
    fn laps_accumulate_in_order_and_sum_to_the_total() {
        let mut profile = profile();
        profile.lap("preflight");
        profile.lap("gateway-create");
        let names: Vec<_> = profile.phases.iter().map(|p| p.phase.as_str()).collect();
        assert_eq!(
            names
                .iter()
                .filter(|n| **n != "driver-setup")
                .copied()
                .collect::<Vec<_>>(),
            vec!["preflight", "gateway-create"]
        );
        let summed: Duration = profile.phases.iter().map(|p| p.elapsed).sum();
        assert_eq!(profile.total(), summed);
    }

    #[test]
    fn ingress_sub_phases_are_namespaced_and_carry_tree_stats() {
        let mut profile = profile();
        profile.absorb_ingress(&capscode_core::IngressReport {
            copied_bytes: 4096,
            copied_files: 12,
            elapsed: Duration::from_secs(9),
            wip_commit: None,
            receipt: capscode_core::IngressReceipt::GitSnapshot { baseline: None },
            phases: vec![PhaseTiming::new("tar-upload", Duration::from_millis(1500))],
        });
        assert!(profile
            .phases
            .iter()
            .any(|p| p.phase == "ingress/tar-upload"));
        assert_eq!(profile.copied_files, 12);
        let table = profile.render_table();
        assert!(table.contains("ingress/tar-upload"));
        assert!(table.contains("GitSnapshot"));
    }
}
