//! Output formatting, terminal capability detection, and emission helpers.
//!
//! Pure formatters in [`fmt`] take an explicit [`Caps`] and return `String`.
//! [`emit`] is the only place that writes to stdout/stderr from this module.

mod caps;
mod emit;
mod fmt;
mod launch_view;
mod theme;

pub use caps::{Caps, ColorDepth};
pub use emit::{data, note, prompt};
pub use fmt::{glyph, header, kv, rule, styled, table, Status};
pub use launch_view::{ensure_no_live_region, no_live_region, LaunchObserver, LaunchView};
pub use theme::theme;
