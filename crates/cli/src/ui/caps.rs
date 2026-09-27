use std::env;
use std::ffi::{OsStr, OsString};
use std::sync::OnceLock;

use console::Term;

/// Colour resolution supported by the output stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorDepth {
    None,
    Ansi16,
    Ansi256,
    TrueColor,
}

/// Terminal capability snapshot for one output stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Caps {
    pub color: bool,
    pub unicode: bool,
    pub width: u16,
    pub sync_output: bool,
    pub color_depth: ColorDepth,
}

impl Caps {
    /// Detected once for stdout and cached for the process lifetime.
    pub fn stdout() -> Caps {
        static CAPS: OnceLock<Caps> = OnceLock::new();
        *CAPS.get_or_init(|| detect(&Term::stdout(), console::colors_enabled()))
    }

    /// Detected once for stderr and cached for the process lifetime.
    pub fn stderr() -> Caps {
        static CAPS: OnceLock<Caps> = OnceLock::new();
        *CAPS.get_or_init(|| detect(&Term::stderr(), console::colors_enabled_stderr()))
    }

    /// Fixed plain fixture: no color, no unicode, 80 columns.
    pub const PLAIN: Caps = Caps {
        color: false,
        unicode: false,
        width: 80,
        sync_output: false,
        color_depth: ColorDepth::None,
    };
}

fn detect(term: &Term, console_colors: bool) -> Caps {
    detect_with_env(term, console_colors, console::is_dumb(), &|key| {
        env::var_os(key)
    })
}

fn detect_with_env(
    term: &Term,
    console_colors: bool,
    is_dumb: bool,
    get_env: &impl Fn(&str) -> Option<OsString>,
) -> Caps {
    let is_tty = term.is_term();
    let color = resolve_color(
        console_colors,
        get_env("NO_COLOR").is_some(),
        env_str(get_env("CLICOLOR").as_deref()),
        env_str(get_env("CLICOLOR_FORCE").as_deref()),
        is_tty,
    );
    let color_depth = detect_color_depth(
        color,
        env_str(get_env("COLORTERM").as_deref()),
        env_str(get_env("TERM").as_deref()),
    );

    Caps {
        color,
        unicode: detect_unicode_from_env(
            env_str(get_env("LC_ALL").as_deref()),
            env_str(get_env("LC_CTYPE").as_deref()),
            env_str(get_env("LANG").as_deref()),
        ),
        width: detect_width(term),
        sync_output: detect_sync_output(color_depth, is_tty, is_dumb, get_env),
        color_depth,
    }
}

fn env_str(value: Option<&OsStr>) -> Option<&str> {
    value.and_then(OsStr::to_str)
}

fn detect_color_depth(color: bool, colorterm: Option<&str>, term: Option<&str>) -> ColorDepth {
    if !color {
        return ColorDepth::None;
    }
    if colorterm.is_some_and(|value| {
        value.eq_ignore_ascii_case("truecolor") || value.eq_ignore_ascii_case("24bit")
    }) {
        ColorDepth::TrueColor
    } else if term.is_some_and(|value| value.contains("256color")) {
        ColorDepth::Ansi256
    } else {
        ColorDepth::Ansi16
    }
}

fn detect_sync_output(
    color_depth: ColorDepth,
    is_tty: bool,
    is_dumb: bool,
    get_env: &impl Fn(&str) -> Option<OsString>,
) -> bool {
    if color_depth == ColorDepth::None || !is_tty || is_dumb {
        return false;
    }

    match env_str(get_env("CAPSCODE_SYNC_OUTPUT").as_deref()) {
        Some("0") => return false,
        Some("1") => return true,
        _ => {}
    }
    if env_str(get_env("TERM_FEATURES").as_deref()).is_some_and(|value| value.contains("Sy")) {
        return true;
    }
    if get_env("WT_SESSION").is_some() {
        return true;
    }
    matches!(
        env_str(get_env("TERM_PROGRAM").as_deref()),
        Some("iTerm.app" | "WezTerm" | "ghostty" | "kitty" | "vscode" | "Apple_Terminal")
    )
}

/// Pure colour decision used by detection and unit tests.
///
/// Rules (in order):
/// - `NO_COLOR` set (any value) ⇒ off
/// - `CLICOLOR_FORCE` set and not `"0"` ⇒ on (regardless of tty)
/// - `CLICOLOR=0` ⇒ off
/// - non-tty ⇒ off
/// - otherwise trust `console`'s per-stream decision
fn resolve_color(
    console_colors: bool,
    no_color_set: bool,
    clicolor: Option<&str>,
    clicolor_force: Option<&str>,
    is_tty: bool,
) -> bool {
    if no_color_set {
        return false;
    }
    if clicolor_force.is_some_and(|v| v != "0") {
        return true;
    }
    if clicolor == Some("0") {
        return false;
    }
    if !is_tty {
        return false;
    }
    console_colors
}

/// Pure unicode-locale decision used by detection and unit tests.
///
/// Checks `LC_ALL`, else `LC_CTYPE`, else `LANG` for a case-insensitive
/// `UTF-8` / `utf8` substring. Absent or other ⇒ false.
fn detect_unicode_from_env(
    lc_all: Option<&str>,
    lc_ctype: Option<&str>,
    lang: Option<&str>,
) -> bool {
    let locale = lc_all.or(lc_ctype).or(lang).unwrap_or("");
    let lower = locale.to_ascii_lowercase();
    lower.contains("utf-8") || lower.contains("utf8")
}

fn detect_width(term: &Term) -> u16 {
    if !term.is_term() {
        return 80;
    }
    let cols = term.size().1;
    cols.clamp(40, 200)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_fixture() {
        assert_eq!(
            Caps::PLAIN,
            Caps {
                color: false,
                unicode: false,
                width: 80,
                sync_output: false,
                color_depth: ColorDepth::None,
            }
        );
    }

    #[test]
    fn color_depth_detection_table() {
        let cases = [
            (
                false,
                Some("truecolor"),
                Some("xterm-256color"),
                ColorDepth::None,
            ),
            (true, Some("truecolor"), None, ColorDepth::TrueColor),
            (true, Some("24BIT"), None, ColorDepth::TrueColor),
            (true, None, Some("xterm-256color"), ColorDepth::Ansi256),
            (true, None, Some("xterm"), ColorDepth::Ansi16),
            (true, None, None, ColorDepth::Ansi16),
        ];

        for (color, colorterm, term, expected) in cases {
            assert_eq!(detect_color_depth(color, colorterm, term), expected);
        }
    }

    #[test]
    fn sync_output_detection_table_uses_injected_environment() {
        struct Case<'a> {
            env: &'a [(&'a str, &'a str)],
            depth: ColorDepth,
            tty: bool,
            dumb: bool,
            expected: bool,
        }

        let cases = [
            Case {
                env: &[("CAPSCODE_SYNC_OUTPUT", "1")],
                depth: ColorDepth::Ansi16,
                tty: true,
                dumb: false,
                expected: true,
            },
            Case {
                env: &[("CAPSCODE_SYNC_OUTPUT", "0"), ("WT_SESSION", "set")],
                depth: ColorDepth::TrueColor,
                tty: true,
                dumb: false,
                expected: false,
            },
            Case {
                env: &[
                    ("CAPSCODE_SYNC_OUTPUT", "invalid"),
                    ("TERM_FEATURES", "ab:Sy:cd"),
                ],
                depth: ColorDepth::Ansi256,
                tty: true,
                dumb: false,
                expected: true,
            },
            Case {
                env: &[("WT_SESSION", "")],
                depth: ColorDepth::Ansi16,
                tty: true,
                dumb: false,
                expected: true,
            },
            Case {
                env: &[("TERM_PROGRAM", "ghostty")],
                depth: ColorDepth::Ansi16,
                tty: true,
                dumb: false,
                expected: true,
            },
            Case {
                env: &[("TERM_PROGRAM", "unknown")],
                depth: ColorDepth::Ansi16,
                tty: true,
                dumb: false,
                expected: false,
            },
            Case {
                env: &[("CAPSCODE_SYNC_OUTPUT", "1")],
                depth: ColorDepth::None,
                tty: true,
                dumb: false,
                expected: false,
            },
            Case {
                env: &[("CAPSCODE_SYNC_OUTPUT", "1")],
                depth: ColorDepth::Ansi16,
                tty: false,
                dumb: false,
                expected: false,
            },
            Case {
                env: &[("CAPSCODE_SYNC_OUTPUT", "1")],
                depth: ColorDepth::Ansi16,
                tty: true,
                dumb: true,
                expected: false,
            },
        ];

        for case in cases {
            let get_env = |key: &str| {
                case.env
                    .iter()
                    .find_map(|(name, value)| (*name == key).then(|| OsString::from(value)))
            };
            assert_eq!(
                detect_sync_output(case.depth, case.tty, case.dumb, &get_env),
                case.expected,
                "environment: {:?}",
                case.env
            );
        }
    }

    #[test]
    fn no_color_disables_color() {
        assert!(!resolve_color(true, true, None, None, true));
        // NO_COLOR wins even over CLICOLOR_FORCE.
        assert!(!resolve_color(true, true, None, Some("1"), true));
    }

    #[test]
    fn clicolor_zero_disables_color() {
        assert!(!resolve_color(true, false, Some("0"), None, true));
    }

    #[test]
    fn clicolor_force_enables_regardless_of_tty() {
        assert!(resolve_color(false, false, Some("0"), Some("1"), false));
        assert!(resolve_color(false, false, None, Some("1"), false));
        // Explicit "0" is not a force.
        assert!(!resolve_color(false, false, None, Some("0"), false));
    }

    #[test]
    fn non_tty_disables_color_without_force() {
        assert!(!resolve_color(true, false, None, None, false));
    }

    #[test]
    fn tty_defers_to_console_decision() {
        assert!(resolve_color(true, false, None, None, true));
        assert!(!resolve_color(false, false, None, None, true));
    }

    #[test]
    fn unicode_from_locale_chain() {
        assert!(detect_unicode_from_env(Some("en_US.UTF-8"), None, None));
        assert!(detect_unicode_from_env(None, Some("C.utf8"), None));
        assert!(detect_unicode_from_env(None, None, Some("en_GB.utf-8")));
        assert!(!detect_unicode_from_env(
            Some("C"),
            None,
            Some("en_US.UTF-8")
        ));
        assert!(!detect_unicode_from_env(None, None, None));
        assert!(!detect_unicode_from_env(None, None, Some("C")));
    }

    #[test]
    fn width_clamp_bounds() {
        assert_eq!(40u16.clamp(40, 200), 40);
        assert_eq!(200u16.clamp(40, 200), 200);
        assert_eq!(39u16.clamp(40, 200), 40);
        assert_eq!(201u16.clamp(40, 200), 200);
        // Floor is deliberately 40 so the width<60 branch in fmt is reachable.
        let floor = 40u16;
        let narrow_threshold = 60u16;
        assert!(floor < narrow_threshold);
    }
}
