//! Theme colors + raw-ANSI helpers.
//!
//! The sidebar renders its rows with raw ANSI (`print!` in `render()`) so it can
//! control backgrounds, the scrollbar, and the spinner precisely — but the
//! *colors* come from the user's active zellij theme, not hardcoded values, so
//! the sidebar matches whatever theme is configured.
//!
//! zellij delivers the theme to plugins as a semantic [`Styling`] (text / ribbon
//! / emphasis / error / success slots). zellij itself provides the bridge from
//! that semantic model to named hues via `From<Styling> for Palette` — the same
//! mapping its web client uses — so we lean on it to recover herdr's
//! red/yellow/green/cyan state colors from any theme, and read a couple of
//! structural colors (main text, selection background) straight off the
//! `Styling`.

use zellij_tile::prelude::{Palette as ZellijPalette, PaletteColor, Style};

/// SGR sequence setting `color` as the foreground, handling both truecolor and
/// 256-color theme entries (mirrors zellij's own `compact-bar` plugin).
pub fn fg(color: PaletteColor) -> String {
    match color {
        PaletteColor::Rgb((r, g, b)) => format!("\u{1b}[38;2;{r};{g};{b}m"),
        PaletteColor::EightBit(c) => format!("\u{1b}[38;5;{c}m"),
    }
}

/// SGR sequence setting `color` as the background.
pub fn bg(color: PaletteColor) -> String {
    match color {
        PaletteColor::Rgb((r, g, b)) => format!("\u{1b}[48;2;{r};{g};{b}m"),
        PaletteColor::EightBit(c) => format!("\u{1b}[48;5;{c}m"),
    }
}

/// Reset all SGR attributes.
pub const RESET: &str = "\u{1b}[0m";
/// Bold on.
pub const BOLD: &str = "\u{1b}[1m";
/// Dim (faint) on.
pub const DIM: &str = "\u{1b}[2m";
/// Normal intensity (cancels bold/dim) without touching colors.
pub const NORMAL_INTENSITY: &str = "\u{1b}[22m";

/// Move the cursor to a 0-based `(x, y)` cell. ANSI is 1-based, hence the `+1`
/// (mirrors the cursor positioning used by zellij's own `about` plugin).
pub fn goto(x: usize, y: usize) -> String {
    format!("\u{1b}[{};{}H", y + 1, x + 1)
}

/// The sidebar's color set, resolved from the active zellij theme. Field names
/// follow herdr's `Palette` so the ported render logic maps over unchanged.
#[derive(Clone)]
pub struct Theme {
    /// Main text color.
    pub text: PaletteColor,
    /// Muted/secondary text (headers, the Unknown icon, dim labels).
    pub muted: PaletteColor,
    /// Separator-line color.
    pub separator: PaletteColor,
    /// Background fill for the focused/active row.
    pub selection_bg: PaletteColor,
    /// Primary accent (title, scrollbar thumb).
    pub accent: PaletteColor,
    /// Needs-attention / blocked state.
    pub red: PaletteColor,
    /// Working / running state.
    pub yellow: PaletteColor,
    /// Done / idle-seen state.
    pub green: PaletteColor,
    /// Done-unseen state.
    pub teal: PaletteColor,
    /// Done notification accent (Phase 4 unseen-toast surfacing).
    #[allow(dead_code)]
    pub blue: PaletteColor,
}

impl Theme {
    /// Resolve the sidebar colors from the active theme `Style`.
    pub fn from_style(style: &Style) -> Self {
        let styling = style.colors;
        // zellij's own semantic→named-hue mapping (used by its web client too).
        let named = ZellijPalette::from(styling);
        Self {
            // Read structural colors straight off the semantic theme so they're
            // exactly what the theme intends for body text and selections.
            text: styling.text_unselected.base,
            selection_bg: styling.text_selected.background,
            // Named hues, theme-derived.
            muted: named.gray,
            separator: named.gray,
            accent: named.blue,
            red: named.red,
            yellow: named.yellow,
            green: named.green,
            teal: named.cyan,
            blue: named.blue,
        }
    }
}

impl Default for Theme {
    /// The default zellij theme, used until the first `ModeUpdate` arrives.
    fn default() -> Self {
        Self::from_style(&Style::default())
    }
}
