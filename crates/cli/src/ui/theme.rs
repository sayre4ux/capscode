use console::{style, Style};
use dialoguer::theme::ColorfulTheme;

use super::caps::Caps;

/// One shared `dialoguer` theme, adapted to stderr capabilities.
///
/// `ColorfulTheme::default()` hardcodes Unicode glyphs; when the locale is not
/// UTF-8 those fields are replaced with ASCII equivalents. When colour is off,
/// styling is forced off on every themed field.
pub fn theme() -> ColorfulTheme {
    let caps = Caps::stderr();
    let mut theme = ColorfulTheme::default();

    if !caps.unicode {
        // prompt_prefix is already ASCII ("?").
        theme.prompt_suffix = style(">".to_string()).for_stderr().black().bright();
        theme.success_prefix = style("[ok]".to_string()).for_stderr().green();
        theme.success_suffix = style(".".to_string()).for_stderr().black().bright();
        theme.error_prefix = style("[x]".to_string()).for_stderr().red();
        theme.active_item_prefix = style(">".to_string()).for_stderr().green();
        theme.checked_item_prefix = style("[x]".to_string()).for_stderr().green();
        theme.unchecked_item_prefix = style("[ ]".to_string()).for_stderr().magenta();
        theme.picked_item_prefix = style(">".to_string()).for_stderr().green();
    }

    if !caps.color {
        theme.defaults_style = Style::new().for_stderr().force_styling(false);
        theme.prompt_style = Style::new().for_stderr().force_styling(false);
        theme.error_style = Style::new().for_stderr().force_styling(false);
        theme.hint_style = Style::new().for_stderr().force_styling(false);
        theme.values_style = Style::new().for_stderr().force_styling(false);
        theme.active_item_style = Style::new().for_stderr().force_styling(false);
        theme.inactive_item_style = Style::new().for_stderr().force_styling(false);

        theme.prompt_prefix = theme.prompt_prefix.force_styling(false);
        theme.prompt_suffix = theme.prompt_suffix.force_styling(false);
        theme.success_prefix = theme.success_prefix.force_styling(false);
        theme.success_suffix = theme.success_suffix.force_styling(false);
        theme.error_prefix = theme.error_prefix.force_styling(false);
        theme.active_item_prefix = theme.active_item_prefix.force_styling(false);
        theme.inactive_item_prefix = theme.inactive_item_prefix.force_styling(false);
        theme.checked_item_prefix = theme.checked_item_prefix.force_styling(false);
        theme.unchecked_item_prefix = theme.unchecked_item_prefix.force_styling(false);
        theme.picked_item_prefix = theme.picked_item_prefix.force_styling(false);
        theme.unpicked_item_prefix = theme.unpicked_item_prefix.force_styling(false);
    }

    theme
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_builds_without_panic() {
        // Caps::stderr() is process-dependent; we only assert construction succeeds.
        let _theme = theme();
    }
}
