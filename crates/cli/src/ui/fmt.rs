use console::{measure_text_width, style};

use super::caps::Caps;

/// Semantic status used for glyphs and colour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Ok,
    Warn,
    Fail,
    Dim,
}

/// Fixed label column width for `kv` alignment across consecutive lines.
const LABEL_WIDTH: usize = 12;

/// Status glyph for the active capability set.
pub fn glyph(s: Status, caps: Caps) -> &'static str {
    if caps.unicode {
        match s {
            Status::Ok => "✔",
            Status::Warn => "⚠",
            Status::Fail => "✖",
            Status::Dim => "·",
        }
    } else {
        match s {
            Status::Ok => "[ok]",
            Status::Warn => "[!]",
            Status::Fail => "[x]",
            Status::Dim => "[-]",
        }
    }
}

/// Apply the status colour to `text` when colour is enabled; otherwise return bare text.
pub fn styled(s: Status, text: &str, caps: Caps) -> String {
    if !caps.color {
        return text.to_string();
    }
    let styled = match s {
        Status::Ok => style(text).green(),
        Status::Warn => style(text).yellow(),
        Status::Fail => style(text).red(),
        Status::Dim => style(text).dim(),
    };
    // Force so pure formatters honour `caps.color` even when the process stream is not a tty.
    styled.force_styling(true).to_string()
}

/// Section header: bold when colour is on, bare otherwise.
pub fn header(text: &str, caps: Caps) -> String {
    if caps.color {
        style(text).bold().force_styling(true).to_string()
    } else {
        text.to_string()
    }
}

/// Key/value line with a fixed label gutter.
///
/// When `caps.width < 60` the label/value gap shrinks to one space; otherwise two.
/// Values are never truncated. An optional status prefixes the line with its glyph.
pub fn kv(label: &str, value: &str, status: Option<Status>, caps: Caps) -> String {
    let gutter = if caps.width < 60 { " " } else { "  " };
    let label_part = format!("{label:<LABEL_WIDTH$}");
    let body = format!("{label_part}{gutter}{value}");
    match status {
        Some(s) => format!("{} {body}", glyph(s, caps)),
        None => body,
    }
}

/// Space-separated aligned table. Column widths include the header; the final column is not padded.
pub fn table(headers: &[&str], rows: &[Vec<String>], _caps: Caps) -> String {
    if headers.is_empty() {
        return String::new();
    }
    let ncols = headers.len();
    let mut widths = vec![0usize; ncols];
    for (i, h) in headers.iter().enumerate() {
        widths[i] = measure_text_width(h);
    }
    for row in rows {
        for (i, cell) in row.iter().enumerate().take(ncols) {
            widths[i] = widths[i].max(measure_text_width(cell));
        }
    }

    let mut lines = Vec::with_capacity(1 + rows.len());
    lines.push(format_row(
        &headers.iter().map(|s| (*s).to_string()).collect::<Vec<_>>(),
        &widths,
    ));
    for row in rows {
        let cells: Vec<String> = (0..ncols)
            .map(|i| row.get(i).cloned().unwrap_or_default())
            .collect();
        lines.push(format_row(&cells, &widths));
    }
    lines.join("\n")
}

fn format_row(cells: &[String], widths: &[usize]) -> String {
    let last = cells.len().saturating_sub(1);
    let mut out = String::new();
    for (i, cell) in cells.iter().enumerate() {
        if i > 0 {
            out.push_str("  ");
        }
        if i == last {
            out.push_str(cell);
        } else {
            let pad = widths[i].saturating_sub(measure_text_width(cell));
            out.push_str(cell);
            out.extend(std::iter::repeat_n(' ', pad));
        }
    }
    out
}

/// Full-width rule, optionally labeled.
///
/// With a label: `── Runtime ────…` (unicode) or `-- Runtime ----…` (ASCII).
/// The label is never truncated; if it alone exceeds `caps.width`, a short trailing
/// rule segment is still emitted.
pub fn rule(label: Option<&str>, caps: Caps) -> String {
    let width = caps.width as usize;
    let (fill, start) = if caps.unicode {
        ('─', "──")
    } else {
        ('-', "--")
    };

    match label {
        None => std::iter::repeat_n(fill, width).collect(),
        Some(label) => {
            let prefix = format!("{start} {label} ");
            let prefix_w = measure_text_width(&prefix);
            if prefix_w >= width {
                // Label never truncated; keep a short trailing segment.
                format!("{start} {label} {start}")
            } else {
                let rest = width - prefix_w;
                let mut out = prefix;
                out.extend(std::iter::repeat_n(fill, rest));
                out
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COLORFUL: Caps = Caps {
        color: true,
        unicode: true,
        width: 100,
        sync_output: false,
        color_depth: super::super::caps::ColorDepth::TrueColor,
    };

    #[test]
    fn glyph_plain_and_unicode() {
        assert_eq!(glyph(Status::Ok, Caps::PLAIN), "[ok]");
        assert_eq!(glyph(Status::Warn, Caps::PLAIN), "[!]");
        assert_eq!(glyph(Status::Fail, Caps::PLAIN), "[x]");
        assert_eq!(glyph(Status::Dim, Caps::PLAIN), "[-]");

        assert_eq!(glyph(Status::Ok, COLORFUL), "✔");
        assert_eq!(glyph(Status::Warn, COLORFUL), "⚠");
        assert_eq!(glyph(Status::Fail, COLORFUL), "✖");
        assert_eq!(glyph(Status::Dim, COLORFUL), "·");
    }

    #[test]
    fn styled_plain_is_bare() {
        assert_eq!(styled(Status::Ok, "ready", Caps::PLAIN), "ready");
        assert_eq!(styled(Status::Fail, "boom", Caps::PLAIN), "boom");
    }

    #[test]
    fn styled_colorful_wraps_ansi() {
        assert_eq!(
            styled(Status::Ok, "ready", COLORFUL),
            "\u{1b}[32mready\u{1b}[0m"
        );
        assert_eq!(
            styled(Status::Warn, "careful", COLORFUL),
            "\u{1b}[33mcareful\u{1b}[0m"
        );
        assert_eq!(
            styled(Status::Fail, "boom", COLORFUL),
            "\u{1b}[31mboom\u{1b}[0m"
        );
        assert_eq!(
            styled(Status::Dim, "hint", COLORFUL),
            "\u{1b}[2mhint\u{1b}[0m"
        );
    }

    #[test]
    fn header_plain_and_bold() {
        assert_eq!(header("Runtime", Caps::PLAIN), "Runtime");
        assert_eq!(header("Runtime", COLORFUL), "\u{1b}[1mRuntime\u{1b}[0m");
    }

    #[test]
    fn kv_plain_two_space_gutter() {
        assert_eq!(
            kv("runtime", "docker", None, Caps::PLAIN),
            "runtime       docker"
        );
        assert_eq!(
            kv("runtime", "docker", Some(Status::Ok), Caps::PLAIN),
            "[ok] runtime       docker"
        );
    }

    #[test]
    fn kv_colorful_unicode_status() {
        assert_eq!(
            kv("runtime", "docker", Some(Status::Ok), COLORFUL),
            "✔ runtime       docker"
        );
        assert_eq!(
            kv("status", "degraded", Some(Status::Warn), COLORFUL),
            "⚠ status        degraded"
        );
    }

    #[test]
    fn kv_narrow_gutter_keeps_full_value() {
        let narrow = Caps {
            color: false,
            unicode: false,
            width: 45,
            ..Caps::PLAIN
        };
        let long_value = "this-value-is-deliberately-long-and-complete";
        let line = kv("runtime", long_value, None, narrow);
        // One-space gutter (narrow), not two.
        assert!(
            line.contains("runtime      this-value"),
            "expected single-space gutter, got {line:?}"
        );
        assert!(
            !line.contains("runtime       this-value"),
            "must not use the wide two-space gutter at width 45: {line:?}"
        );
        assert!(
            line.ends_with(long_value),
            "value must not be truncated: {line:?}"
        );
    }

    #[test]
    fn table_plain_snapshot() {
        let headers = ["ID", "STATUS"];
        let rows = vec![
            vec!["abc".into(), "ok".into()],
            vec!["defg".into(), "fail".into()],
        ];
        let rendered = table(&headers, &rows, Caps::PLAIN);
        assert_eq!(
            rendered,
            "ID    STATUS\n\
             abc   ok\n\
             defg  fail"
        );
        assert!(!rendered.contains('\t'));
    }

    #[test]
    fn table_colorful_snapshot() {
        let headers = ["NAME", "VALUE"];
        let rows = vec![vec!["alpha".into(), "1".into()]];
        let rendered = table(&headers, &rows, COLORFUL);
        assert_eq!(rendered, "NAME   VALUE\nalpha  1");
    }

    #[test]
    fn table_long_cell_aligns_final_column() {
        let headers = ["A", "B", "C"];
        let rows = vec![
            vec!["short".into(), "x".into(), "one".into()],
            vec![
                "a-deliberately-long-identifier".into(),
                "yy".into(),
                "two".into(),
            ],
            vec!["mid".into(), "z".into(), "three".into()],
        ];
        let rendered = table(&headers, &rows, Caps::PLAIN);
        assert!(
            !rendered.contains('\t'),
            "table must not use tabs: {rendered:?}"
        );

        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines.len(), 4); // header + 3 rows

        // Final column starts at the same byte offset on every line.
        let final_starts: Vec<usize> = lines
            .iter()
            .map(|line| {
                // Last column content for each line.
                let last = line.split_whitespace().last().unwrap();
                line.rfind(last).unwrap()
            })
            .collect();
        assert!(
            final_starts.windows(2).all(|w| w[0] == w[1]),
            "final column misaligned: offsets={final_starts:?}\n{rendered}"
        );
    }

    #[test]
    fn rule_plain_unlabeled() {
        let r = rule(None, Caps::PLAIN);
        assert_eq!(r, "-".repeat(80));
        assert_eq!(r.len(), 80);
    }

    #[test]
    fn rule_plain_labeled() {
        let r = rule(Some("Runtime"), Caps::PLAIN);
        // "-- Runtime " is 11 cols; fill the remaining 69 with '-'.
        assert_eq!(r, format!("-- Runtime {}", "-".repeat(69)));
        assert_eq!(r.len(), 80);
    }

    #[test]
    fn rule_unicode_labeled() {
        let r = rule(Some("Runtime"), COLORFUL);
        // "── Runtime " is 11 cols; fill the remaining 89 with '─'.
        let expected = format!("── Runtime {}", "─".repeat(89));
        assert_eq!(r, expected);
        assert_eq!(measure_text_width(&r), 100);
    }

    #[test]
    fn rule_unicode_unlabeled() {
        let r = rule(None, COLORFUL);
        assert_eq!(r.chars().count(), 100);
        assert!(r.chars().all(|c| c == '─'));
    }

    #[test]
    fn rule_label_longer_than_width_is_not_truncated() {
        let narrow = Caps {
            color: false,
            unicode: true,
            width: 10,
            ..Caps::PLAIN
        };
        let label = "AVeryLongSectionName";
        let r = rule(Some(label), narrow);
        assert!(r.contains(label), "label must appear in full, got {r:?}");
        assert!(
            r.ends_with("──"),
            "must keep a short trailing rule segment: {r:?}"
        );
        assert!(
            measure_text_width(&r) > 10,
            "output may exceed width when label is longer"
        );
    }

    #[test]
    fn rule_ascii_label_longer_than_width() {
        let narrow = Caps {
            color: false,
            unicode: false,
            width: 8,
            ..Caps::PLAIN
        };
        let label = "Providers";
        let r = rule(Some(label), narrow);
        assert_eq!(r, "-- Providers --");
    }

    #[test]
    fn redaction_before_styling_keeps_ansi_balanced() {
        // Correct order: redact the raw secret, then style. Styling first would let
        // the sk- matcher consume the closing SGR reset and leave an open escape.
        let secret = "sk-live-secret-value";
        let redacted = crate::redact::redact(secret);
        assert_eq!(redacted, "[REDACTED]");

        let colored = styled(Status::Ok, &redacted, COLORFUL);
        assert_eq!(colored, "\u{1b}[32m[REDACTED]\u{1b}[0m");
        assert!(!colored.contains("sk-"));
        assert!(
            colored.contains("[REDACTED]"),
            "masked value must remain visible"
        );
        // Balanced: every SGR open is closed by a reset; no dangling ESC.
        assert!(
            colored.ends_with("\u{1b}[0m"),
            "must end with a reset: {colored:?}"
        );
        let escapes: Vec<_> = colored.match_indices('\u{1b}').collect();
        assert_eq!(escapes.len(), 2, "open + reset only: {colored:?}");
    }

    #[test]
    fn redaction_of_prestyled_secret_is_why_order_matters() {
        // Document the failure mode that forces redact-then-style: applying
        // redact() to an already-styled sk- value swallows the reset code.
        let prestyled = styled(Status::Ok, "sk-live-secret-value", COLORFUL);
        assert!(prestyled.starts_with("\u{1b}[32m"));
        assert!(prestyled.ends_with("\u{1b}[0m"));
        let broken = crate::redact::redact(&prestyled);
        assert!(
            broken.contains("[REDACTED]"),
            "matcher still finds sk-: {broken:?}"
        );
        assert!(
            !broken.ends_with("\u{1b}[0m"),
            "reset was consumed with the secret: {broken:?}"
        );
    }
}
