use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(unix)]
use std::os::unix::io::RawFd;
#[cfg(unix)]
use std::ptr;
#[cfg(unix)]
use std::sync::atomic::AtomicPtr;

/// Process-wide flag: a live coding session owns SIGINT/SIGTERM for graceful
/// container teardown. When true, the prompt/exit signal restorer only restores
/// terminal state and returns — it must NOT re-raise the signal (that would
/// hard-kill the process and leak the gateway container).
static SESSION_OWNS_SIGNALS: AtomicBool = AtomicBool::new(false);
static HANGUP_RECEIVED: AtomicBool = AtomicBool::new(false);

/// Mark whether a live session currently owns process signals for graceful
/// teardown. Called once after Tokio's SIGINT/SIGTERM handlers are registered
/// in `commands::run`.
pub fn set_session_signal_owner(owns: bool) {
    SESSION_OWNS_SIGNALS.store(owns, Ordering::Relaxed);
}

pub fn mark_hangup_received() {
    HANGUP_RECEIVED.store(true, Ordering::Relaxed);
}

pub fn hangup_received() -> bool {
    HANGUP_RECEIVED.load(Ordering::Relaxed)
}

/// Snapshot of stdin termios taken at process start (cooked mode, echo on).
/// Intentionally leaked so the pointer is async-signal-safe to load from a
/// signal handler. Non-null only after a successful `install_prompt_restorer`
/// on a real stderr TTY.
#[cfg(unix)]
static COOKED_SNAPSHOT: AtomicPtr<libc::termios> = AtomicPtr::new(ptr::null_mut());

/// The host terminal's original (cooked) settings while an interactive guard
/// holds raw mode. Non-null for exactly one active guard at a time.
///
/// `TerminalGuard` itself installs no OS signal handler: the process already
/// installs Tokio SIGINT/SIGTERM handlers (`commands::signal_receiver`) that
/// drive graceful teardown of the capsule/gateway/volume. Restoration therefore
/// happens through `Drop` (normal return, `?` errors, panic unwind), and the
/// terminal is only ever raw while the interactive tool is actually attached.
///
/// A *separate* prompt/exit restorer (see `install_prompt_restorer`) does
/// install a cooperative SIGINT/SIGTERM handler that only restores terminal
/// state (show cursor, re-apply cooked termios) and re-raises the default
/// disposition **only when no live session owns signals** (`SESSION_OWNS_SIGNALS`).
/// During a live session the restorer restores and returns so Tokio's graceful
/// teardown is never clobbered.
#[cfg(unix)]
static SAVED_TERMIOS: AtomicPtr<libc::termios> = AtomicPtr::new(ptr::null_mut());

/// Install the process-wide prompt/exit terminal restorer. Call once at process
/// start, before any Tokio signal registration and before command dispatch.
///
/// On unix when stderr is a TTY: snapshots cooked stdin termios, installs an
/// async-signal-safe SIGINT/SIGTERM handler, and chains a panic hook. Otherwise
/// a no-op.
pub fn install_prompt_restorer() {
    #[cfg(unix)]
    {
        ignore_sigpipe();
        install_prompt_restorer_unix();
    }
}

#[cfg(unix)]
fn ignore_sigpipe() {
    // A closed terminal pipe should become an ordinary write error, not kill
    // the process before its session teardown can run.
    unsafe {
        let _ = libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
}

#[cfg(unix)]
fn install_prompt_restorer_unix() {
    if !std::io::stderr().is_terminal() {
        return;
    }

    // Snapshot cooked termios once; leak the Box so the pointer stays valid for
    // the life of the process and is safe to load from a signal handler.
    let mut t: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut t) } == 0 {
        COOKED_SNAPSHOT.store(Box::into_raw(Box::new(t)), Ordering::Relaxed);
    }

    // SA_RESTART matches signal-hook's flags so interrupted syscalls resume.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_prompt_signal as *const () as usize;
        sa.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut sa.sa_mask);
        let _ = libc::sigaction(libc::SIGINT, &sa, ptr::null_mut());
        let _ = libc::sigaction(libc::SIGTERM, &sa, ptr::null_mut());
    }

    // Panic mid-prompt still restores the cursor / termios.
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_prompt_terminal();
        prev(info);
    }));
}

/// Async-signal-safe SIGINT/SIGTERM handler for prompt/exit restore.
///
/// Uses only async-signal-safe ops: write, tcsetattr, signal, raise, atomic load.
#[cfg(unix)]
extern "C" fn on_prompt_signal(sig: libc::c_int) {
    // Show cursor + SGR reset on stderr (dialoguer renders there).
    const SEQ: &[u8] = b"\x1b[?25h\x1b[0m";
    unsafe {
        let _ = libc::write(
            libc::STDERR_FILENO,
            SEQ.as_ptr() as *const libc::c_void,
            SEQ.len(),
        );
        // tcflush is POSIX async-signal-safe; drop unread typeahead so a second
        // answer typed at a confirm cannot land in the parent shell after exit.
        let _ = libc::tcflush(libc::STDIN_FILENO, libc::TCIFLUSH);
    }

    let ptr = COOKED_SNAPSHOT.load(Ordering::Relaxed);
    if !ptr.is_null() {
        unsafe {
            let _ = libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, ptr);
        }
    }

    // Live session owns teardown: restore only, do not die.
    if SESSION_OWNS_SIGNALS.load(Ordering::Relaxed) {
        return;
    }

    // Fatal path (wizard prompts etc.): restore default disposition and re-raise
    // so exit status is correct (~130 for SIGINT).
    unsafe {
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
}

/// Normal-context restore for error-return paths (not signal-safe constraints).
/// Writes show-cursor + SGR reset to stderr, drains stdin typeahead, and
/// re-applies the cooked termios snapshot when present. No-op on non-tty /
/// non-unix.
pub fn restore_prompt_terminal() {
    if hangup_received() {
        return;
    }
    #[cfg(unix)]
    {
        restore_prompt_terminal_unix();
    }
    #[cfg(not(unix))]
    {
        // Still attempt cursor show + typeahead drain on non-unix paths.
        if std::io::stderr().is_terminal() {
            let _ = console::Term::stderr().show_cursor();
        }
        drain_typeahead();
    }
}

#[cfg(unix)]
fn restore_prompt_terminal_unix() {
    write_restore_sequences(std::io::stderr().is_terminal(), &mut std::io::stderr());

    // Discard leftover keystrokes so they never reach the parent shell.
    drain_typeahead();

    let ptr = COOKED_SNAPSHOT.load(Ordering::Relaxed);
    if ptr.is_null() {
        return;
    }
    unsafe {
        let _ = libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, ptr);
    }
}

fn write_restore_sequences(stderr_is_tty: bool, stderr: &mut impl Write) {
    if stderr_is_tty {
        let _ = stderr.write_all(b"\x1b[?25h\x1b[0m");
        let _ = stderr.flush();
    }
}

/// RAII guard for interactive `dialoguer` prompts.
///
/// `dialoguer` hides the cursor while a prompt is live. On Ctrl-C, `?`
/// propagation, panic unwind, or normal return this guard shows the cursor
/// again and drains any leftover typeahead so it cannot leak into the parent
/// shell. `Drop` is non-panicking.
pub struct CursorGuard {
    _private: (),
}

impl CursorGuard {
    /// Begin guarding the current prompt. Place as a local before invoking
    /// `dialoguer` so it covers every exit path of the enclosing scope.
    pub fn new() -> Self {
        Self { _private: () }
    }
}

impl Default for CursorGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for CursorGuard {
    fn drop(&mut self) {
        // console::Term::show_cursor is the primitive; ignore I/O errors so Drop
        // never panics (e.g. broken pipe on a closed stderr).
        if !hangup_received() {
            if std::io::stderr().is_terminal() {
                let _ = console::Term::stderr().show_cursor();
            }
            drain_typeahead();
        }
    }
}

/// Discard any pending bytes in the stdin typeahead queue.
///
/// Fail-soft: non-tty stdin and flush errors are silent no-ops. Used after
/// prompts and at `TerminalGuard` boundaries so leftover keystrokes cannot be
/// inherited by a child process or the parent shell (e.g. a second `yes`
/// becoming `/usr/bin/yes`).
pub fn drain_typeahead() {
    #[cfg(unix)]
    {
        if !std::io::stdin().is_terminal() {
            return;
        }
        // TCIFLUSH discards data received but not yet read. Errors are ignored:
        // this helper must never be fatal.
        let _ = unsafe { libc::tcflush(libc::STDIN_FILENO, libc::TCIFLUSH) };
    }
}

/// RAII guard that switches the host terminal to raw mode for the duration of an
/// interactive attach and restores the prior state on drop.
///
/// Raw mode is enabled only when both stdin and stdout are TTYs and no other
/// guard is already active; otherwise the guard is a no-op that preserves exact
/// prior (batch / piped) behavior. It installs no signal handlers, so it never
/// disturbs the process-global graceful-shutdown handlers (or the separate
/// prompt/exit restorer installed by `install_prompt_restorer`).
pub struct TerminalGuard {
    #[cfg(unix)]
    active: bool,
    #[cfg(not(unix))]
    _private: (),
}

impl TerminalGuard {
    /// Construct a guard. Acquires raw mode only for a real, non-nested TTY pair.
    pub fn new() -> Self {
        #[cfg(unix)]
        {
            Self::new_unix()
        }
        #[cfg(not(unix))]
        {
            Self { _private: () }
        }
    }

    #[cfg(unix)]
    fn new_unix() -> Self {
        if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
            return Self { active: false };
        }

        let fd: RawFd = libc::STDIN_FILENO;
        let mut orig: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut orig) } != 0 {
            // Not a controlling tty (or other failure); stay cooked.
            return Self { active: false };
        }

        // Publish the cooked snapshot BEFORE touching the terminal, and refuse to
        // nest: if a guard is already active the swap fails and we leave the
        // terminal (and the first guard's snapshot) untouched.
        let snapshot = Box::into_raw(Box::new(orig));
        if SAVED_TERMIOS
            .compare_exchange(
                ptr::null_mut(),
                snapshot,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_err()
        {
            unsafe { drop(Box::from_raw(snapshot)) };
            return Self { active: false };
        }

        // Drop any keystrokes typed during a preceding wait so they do not
        // feed the interactive child TUI.
        drain_typeahead();

        let mut raw = orig;
        unsafe { libc::cfmakeraw(&mut raw) };
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
            // Never actually changed the terminal; just drop the published snapshot.
            restore_terminal();
            return Self { active: false };
        }

        Self { active: true }
    }

    /// Test-only constructor that mocks the is-terminal decision. It never
    /// performs a real `tcsetattr`, so `cargo test` from an interactive shell can
    /// never be left in raw mode.
    #[cfg(test)]
    pub fn new_with_tty_decision(stdin_is_tty: bool, stdout_is_tty: bool) -> Self {
        #[cfg(unix)]
        {
            Self {
                active: stdin_is_tty && stdout_is_tty,
            }
        }
        #[cfg(not(unix))]
        {
            let _ = (stdin_is_tty, stdout_is_tty);
            Self { _private: () }
        }
    }
}

impl Default for TerminalGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            if self.active && !hangup_received() {
                // Drain before handing the tty back (child exit / attach end) so
                // leftover input cannot reach the parent shell. Sits beside
                // sanitize_terminal (escape-sequence reset), not folded into it:
                // input-queue flush and in-band mode reset are separate concerns.
                drain_typeahead();
                if std::io::stdout().is_terminal() {
                    sanitize_terminal();
                }
                restore_terminal();
                self.active = false;
            }
        }
    }
}

/// Restores the saved terminal state if a guard is active. Idempotent: the atomic
/// swap means the first caller restores and any later call is a no-op.
#[cfg(unix)]
fn restore_terminal() {
    let saved = SAVED_TERMIOS.swap(ptr::null_mut(), Ordering::SeqCst);
    if saved.is_null() {
        return;
    }
    let orig = unsafe { *saved };
    unsafe { drop(Box::from_raw(saved)) };
    // Retry across EINTR; other failures are unrecoverable at this point.
    loop {
        if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &orig) } == 0 {
            break;
        }
        if std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            break;
        }
    }
}

/// Emits terminal-reset sequences to undo in-band modes an interactive TUI (e.g.
/// codex's Kitty keyboard protocol) may have enabled and not cleaned up on an
/// abrupt exit. Every sequence is a no-op if the mode was not set. Written raw to
/// the stdout fd; the interactive attach loop has already flushed tokio's stdout,
/// and this runs just before termios is restored.
#[cfg(unix)]
fn sanitize_terminal() {
    // Kitty keyboard pop; bracketed-paste off; focus-reporting off; mouse tracking
    // off (1000/1002/1003/1006); show cursor; reset SGR. Alternate-screen is left
    // untouched on purpose (it is not the observed problem and toggling it could
    // clear output).
    const RESET: &[u8] =
        b"\x1b[<u\x1b[?2004l\x1b[?1004l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?25h\x1b[0m";
    unsafe {
        let _ = libc::write(
            libc::STDOUT_FILENO,
            RESET.as_ptr() as *const libc::c_void,
            RESET.len(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{
        set_session_signal_owner, write_restore_sequences, TerminalGuard, SESSION_OWNS_SIGNALS,
    };
    use std::sync::atomic::Ordering;

    #[cfg(unix)]
    fn restore() {
        super::restore_terminal();
    }
    #[cfg(not(unix))]
    fn restore() {}

    #[test]
    fn guard_is_noop_when_not_both_ttys() {
        // None of these may activate raw mode or panic.
        drop(TerminalGuard::new_with_tty_decision(false, false));
        drop(TerminalGuard::new_with_tty_decision(true, false));
        drop(TerminalGuard::new_with_tty_decision(false, true));
        restore();
    }

    #[test]
    fn restore_is_idempotent_and_safe() {
        restore();
        restore();
        drop(TerminalGuard::new_with_tty_decision(false, false));
        restore();
    }

    #[test]
    fn mock_construction_never_panics() {
        // Mock path only — never touches the real controlling terminal.
        let _g0 = TerminalGuard::new_with_tty_decision(true, true);
        let _g1 = TerminalGuard::new_with_tty_decision(false, false);
        drop(_g0);
        drop(_g1);
        restore();
    }

    #[test]
    fn set_session_signal_owner_toggles_atomic() {
        // Save and restore so other tests (or parallel runs) are not affected.
        let prev = SESSION_OWNS_SIGNALS.load(Ordering::Relaxed);
        set_session_signal_owner(true);
        assert!(SESSION_OWNS_SIGNALS.load(Ordering::Relaxed));
        set_session_signal_owner(false);
        assert!(!SESSION_OWNS_SIGNALS.load(Ordering::Relaxed));
        set_session_signal_owner(prev);
    }

    #[test]
    fn install_and_restore_prompt_are_safe_noops_without_tty() {
        // Under `cargo test` stderr is typically not a terminal, so
        // install_prompt_restorer is a no-op that never panics and never
        // installs a real signal handler / tcsetattr. restore_prompt_terminal
        // is always safe to call (writes escape sequences only to a TTY; tcsetattr only if
        // COOKED_SNAPSHOT is non-null, which it isn't without a TTY install).
        super::install_prompt_restorer();
        super::restore_prompt_terminal();
        super::restore_prompt_terminal();
    }

    #[test]
    fn restore_escape_sequences_are_written_only_to_a_tty() {
        let mut stderr = Vec::new();
        write_restore_sequences(false, &mut stderr);
        assert!(stderr.is_empty());
        write_restore_sequences(true, &mut stderr);
        assert_eq!(stderr, b"\x1b[?25h\x1b[0m");
    }

    #[test]
    fn drain_typeahead_is_safe_without_tty() {
        // Non-tty (or flush failure) must be a silent no-op, never panic.
        super::drain_typeahead();
        super::drain_typeahead();
    }

    #[test]
    fn cursor_guard_drop_shows_cursor_without_panic() {
        // Drop covers normal return; nested scopes cover sequential prompts.
        {
            let _g = super::CursorGuard::new();
        }
        let g = super::CursorGuard::new();
        drop(g);
        // restore_prompt_terminal also drains; must stay composable.
        super::restore_prompt_terminal();
        super::drain_typeahead();
    }

    #[test]
    fn cursor_guard_default_matches_new() {
        let _a = super::CursorGuard::new();
        let _b = super::CursorGuard::default();
    }
}
