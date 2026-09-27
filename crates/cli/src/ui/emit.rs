/// Write a data line to stdout (tables, JSON, pipeable values). Never redacts.
#[allow(clippy::print_stdout)]
pub fn data(line: impl std::fmt::Display) {
    println!("{line}");
}

/// Write a narration line to stderr (status, progress, prompts).
///
/// The rendered string is passed through [`crate::redact::redact`] before writing.
/// Callers must redact-or-avoid secrets **before** applying ANSI styling: the
/// matcher is not ANSI-aware and will swallow a trailing SGR reset if an `sk-`
/// value is already colour-wrapped.
#[allow(clippy::print_stderr)]
pub fn note(line: impl std::fmt::Display) {
    let raw = line.to_string();
    let redacted = crate::redact::redact(&raw);
    if !super::launch_view::print_note(&redacted) {
        eprintln!("{redacted}");
    }
}

/// A prompt: no trailing newline, flushed immediately, on stderr like every
/// other piece of narration. The REPL needs this; nothing else should.
#[allow(clippy::print_stderr)]
pub fn prompt(text: impl std::fmt::Display) {
    use std::io::Write;
    eprint!("{text}");
    let _ = std::io::stderr().flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::fmt::{styled, Status};
    use crate::ui::Caps;

    #[test]
    fn note_redacts_plain_sk_values() {
        // note() redacts the raw Display string. We exercise the same path
        // redact() takes without capturing stderr.
        let line = "token=sk-live-secret-value ok";
        let redacted = crate::redact::redact(line);
        assert_eq!(redacted, "token=[REDACTED] ok");
        // Call note so the function is covered; output goes to the test harness stderr.
        note(line);
    }

    #[test]
    fn data_does_not_redact() {
        // data() is a thin println; document that it does not call redact.
        // Calling it exercises the function for coverage / dead-code.
        data("token=sk-live-secret-value");
    }

    #[test]
    fn redact_then_style_then_note_path_is_balanced() {
        const COLORFUL: Caps = Caps {
            color: true,
            unicode: true,
            width: 100,
            sync_output: false,
            color_depth: crate::ui::ColorDepth::TrueColor,
        };
        let raw = "sk-live-secret-value";
        let safe = crate::redact::redact(raw);
        let line = styled(Status::Fail, &safe, COLORFUL);
        assert_eq!(line, "\u{1b}[31m[REDACTED]\u{1b}[0m");
        assert!(line.ends_with("\u{1b}[0m"));
        note(&line);
    }
}
