use std::io::IsTerminal;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::ensure;
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};

use super::fmt::{glyph, styled, Status};
use super::Caps;

static LIVE_REGION: AtomicBool = AtomicBool::new(false);
static LIVE_PROGRESS: Mutex<Option<MultiProgress>> = Mutex::new(None);

const PENDING: u8 = 0;
const ACTIVE: u8 = 1;
const DONE: u8 = 2;
const FAILED: u8 = 3;

/// Progress sink for the launch path. Implemented by [`LaunchView`]; `()` is
/// the no-op implementation used when no live region is available.
pub trait LaunchObserver: Send + Sync {
    /// In-flight sub-status: updates the active row's trailing detail.
    fn message(&self, phase: &str, text: &str);
    /// Phase completion: settles the row with its elapsed time.
    fn lap(&self, phase: &str, elapsed: Duration);
}

impl LaunchObserver for () {
    fn message(&self, _phase: &str, _text: &str) {}

    fn lap(&self, _phase: &str, _elapsed: Duration) {}
}

struct Row {
    phase: String,
    bar: ProgressBar,
    state: AtomicU8,
}

/// Live, redrawn launch checklist on stderr.
pub struct LaunchView {
    multi: MultiProgress,
    rows: Vec<Row>,
    caps: Caps,
    finished: AtomicBool,
}

impl LaunchView {
    /// Build a live view for the normal, non-verbose launch path.
    pub fn new(phases: &[&str], caps: Caps) -> Option<Self> {
        Self::new_with_verbose(phases, caps, false)
    }

    /// Build a live view while applying the complete cursor-addressing gate.
    /// Verbose callers receive `None` and can keep their append-only log stream.
    pub fn new_with_verbose(phases: &[&str], caps: Caps, verbose: bool) -> Option<Self> {
        let stderr_is_tty = std::io::stderr().is_terminal();
        if !live_region_allowed(stderr_is_tty, console::is_dumb(), verbose) {
            return None;
        }
        if LIVE_REGION
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return None;
        }

        let multi = MultiProgress::with_draw_target(ProgressDrawTarget::stderr());
        if multi.is_hidden() {
            LIVE_REGION.store(false, Ordering::Release);
            return None;
        }
        let style = ProgressStyle::with_template("{wide_msg}").expect("static template is valid");
        let rows = phases
            .iter()
            .map(|phase| {
                let bar = multi.add(ProgressBar::new_spinner());
                bar.set_style(style.clone());
                bar.set_message(render_row(phase, None, PENDING, caps));
                Row {
                    phase: (*phase).to_owned(),
                    bar,
                    state: AtomicU8::new(PENDING),
                }
            })
            .collect();

        *lock_live_progress() = Some(multi.clone());
        Some(Self {
            multi,
            rows,
            caps,
            finished: AtomicBool::new(false),
        })
    }

    /// Settle incomplete rows, clear the redraw region, and unregister it.
    pub fn finish(self) {
        self.shutdown();
    }

    fn shutdown(&self) {
        if self.finished.swap(true, Ordering::AcqRel) {
            return;
        }

        let _ = catch_unwind(AssertUnwindSafe(|| {
            for row in &self.rows {
                if row
                    .state
                    .try_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                        (state < DONE).then_some(FAILED)
                    })
                    .is_ok()
                {
                    row.bar
                        .set_message(render_row(&row.phase, None, FAILED, self.caps));
                }
            }
            for row in &self.rows {
                row.bar.disable_steady_tick();
                row.bar.finish_and_clear();
            }
            let _ = self.multi.clear();
        }));

        lock_live_progress().take();
        LIVE_REGION.store(false, Ordering::Release);
    }
}

impl LaunchObserver for LaunchView {
    fn message(&self, phase: &str, text: &str) {
        let Some(row) = self.rows.iter().find(|row| row.phase == phase) else {
            return;
        };
        if row
            .state
            .try_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                (state < DONE).then_some(ACTIVE)
            })
            .is_ok()
        {
            row.bar
                .set_message(render_row(phase, Some(text), ACTIVE, self.caps));
        }
    }

    fn lap(&self, phase: &str, elapsed: Duration) {
        let Some(row) = self.rows.iter().find(|row| row.phase == phase) else {
            return;
        };
        if row
            .state
            .try_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                (state < DONE).then_some(DONE)
            })
            .is_ok()
        {
            let elapsed = format_elapsed(elapsed);
            row.bar
                .set_message(render_row(phase, Some(&elapsed), DONE, self.caps));
            row.bar.finish();
        }
    }
}

impl Drop for LaunchView {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// True when no launch redraw region currently owns stderr.
pub fn no_live_region() -> bool {
    !LIVE_REGION.load(Ordering::Acquire)
}

/// Prove that stderr is clean before handing the terminal to a child TUI.
pub fn ensure_no_live_region() -> anyhow::Result<()> {
    ensure!(no_live_region(), "a launch progress region is still live");
    Ok(())
}

pub(super) fn print_note(line: &str) -> bool {
    let progress = lock_live_progress().clone();
    catch_unwind(AssertUnwindSafe(|| {
        progress.is_some_and(|multi| print_to_progress(&multi, line))
    }))
    .unwrap_or(false)
}

fn print_to_progress(multi: &MultiProgress, line: &str) -> bool {
    !multi.is_hidden() && multi.println(line).is_ok()
}

fn lock_live_progress() -> std::sync::MutexGuard<'static, Option<MultiProgress>> {
    LIVE_PROGRESS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn live_region_allowed(stderr_is_tty: bool, dumb: bool, verbose: bool) -> bool {
    stderr_is_tty && !dumb && !verbose
}

fn render_row(phase: &str, detail: Option<&str>, state: u8, caps: Caps) -> String {
    let status = match state {
        PENDING => Status::Dim,
        ACTIVE => Status::Warn,
        DONE => Status::Ok,
        _ => Status::Fail,
    };
    let marker = styled(status, glyph(status, caps), caps);
    let phase = styled(status, phase, caps);
    match detail.filter(|detail| !detail.is_empty()) {
        Some(detail) => {
            let separator = if caps.unicode { "—" } else { "-" };
            format!("{marker} {phase} {separator} {detail}")
        }
        None => format!("{marker} {phase}"),
    }
}

fn format_elapsed(elapsed: Duration) -> String {
    if elapsed < Duration::from_secs(1) {
        format!("{}ms", elapsed.as_millis())
    } else if elapsed < Duration::from_secs(60) {
        format!("{:.2}s", elapsed.as_secs_f64())
    } else {
        let minutes = elapsed.as_secs() / 60;
        let seconds = elapsed.as_secs() % 60;
        format!("{minutes}m {seconds:02}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::{note, ColorDepth};

    #[test]
    fn gating_requires_every_condition() {
        assert!(live_region_allowed(true, false, false));
        assert!(!live_region_allowed(false, false, false));
        assert!(!live_region_allowed(true, true, false));
        assert!(!live_region_allowed(true, false, true));
    }

    #[test]
    fn rows_degrade_through_the_shared_formatters() {
        assert_eq!(
            render_row("Images", None, PENDING, Caps::PLAIN),
            "[-] Images"
        );
        assert_eq!(
            render_row("Images", Some("pulling layer 2"), ACTIVE, Caps::PLAIN),
            "[!] Images - pulling layer 2"
        );
        assert_eq!(
            render_row("Images", Some("1.25s"), DONE, Caps::PLAIN),
            "[ok] Images - 1.25s"
        );
        assert_eq!(
            render_row("Images", None, FAILED, Caps::PLAIN),
            "[x] Images"
        );
    }

    #[test]
    fn elapsed_format_is_compact() {
        assert_eq!(format_elapsed(Duration::from_millis(85)), "85ms");
        assert_eq!(format_elapsed(Duration::from_millis(1250)), "1.25s");
        assert_eq!(format_elapsed(Duration::from_secs(125)), "2m 05s");
    }

    #[test]
    fn hidden_progress_target_requests_stderr_fallback() {
        let multi = MultiProgress::with_draw_target(ProgressDrawTarget::hidden());
        assert!(!print_to_progress(&multi, "still visible via fallback"));
    }

    #[test]
    fn region_assertion_is_clean_without_a_view() {
        LIVE_REGION.store(false, Ordering::Release);
        assert!(no_live_region());
        assert!(ensure_no_live_region().is_ok());
    }

    #[test]
    fn live_view_smoke_when_test_has_a_tty() {
        let caps = Caps {
            color: true,
            unicode: true,
            width: 80,
            sync_output: false,
            color_depth: ColorDepth::TrueColor,
        };
        let Some(view) = LaunchView::new(&["Runtime", "Images"], caps) else {
            return;
        };

        view.message("Runtime", "detecting engine");
        std::thread::sleep(Duration::from_millis(40));
        view.lap("Runtime", Duration::from_millis(85));
        view.message("Images", "pulling layer 2");
        note("launch narration stays visible");
        std::thread::sleep(Duration::from_millis(40));
        view.lap("Images", Duration::from_millis(1250));
        view.finish();

        assert!(no_live_region());
        assert!(ensure_no_live_region().is_ok());
    }
}
