//! Reverse-layout rendering for the project picker, drawn as raw ANSI.
//!
//! The shape is the fzf/telescope `--layout=reverse-list`: the text input sits on
//! the **bottom** row and results render **above** it, ordered most-likely
//! (just above the input) to least-likely (toward the top). The selection cursor
//! defaults to the best (bottom-most) result. Matched (fuzzy-hit) chars are not
//! recolored, and the parent path is shown only to disambiguate a name collision.
//! Projects that already have a live session (matched against
//! `SessionInfo.workspace_root`) are badged so the user sees "switch" vs. "launch"
//! at a glance.
//!
//! Colors come from the user's active zellij theme (see [`crate::palette`]),
//! emitted as raw ANSI so the selection background, badges, and highlights stay
//! precise while still tracking the theme.

use unicode_width::UnicodeWidthStr;
use zellij_tile::prelude::PaletteColor;

use crate::palette::{bg, fg, goto, Theme, BOLD, DIM, NORMAL_INTENSITY, RESET};
use crate::ranking::Ranked;

/// The badge glyph marking a project that already has a live session.
const OPEN_BADGE: &str = "●";

/// A styled run of text on one row.
struct Span {
    text: String,
    fg: PaletteColor,
    bold: bool,
    dim: bool,
}

impl Span {
    fn new(text: impl Into<String>, fg: PaletteColor) -> Self {
        Span {
            text: text.into(),
            fg,
            bold: false,
            dim: false,
        }
    }
    fn bold(mut self) -> Self {
        self.bold = true;
        self
    }
    fn dim(mut self) -> Self {
        self.dim = true;
        self
    }
}

/// Inputs to a single render pass.
pub struct RenderInput<'a> {
    pub permissions_granted: bool,
    pub configured: bool,
    pub query: &'a str,
    pub results: &'a [Ranked<'a>],
    /// Absolute project paths that currently have a live session.
    pub open_paths: &'a std::collections::HashSet<String>,
    pub palette: &'a Theme,
    /// Selection cursor: absolute index into `results` (0 = best, bottom-most).
    pub selected: usize,
    /// Scroll offset: index of the bottom-most visible result.
    pub scroll: usize,
    pub total_candidates: usize,
    pub rows: usize,
    pub cols: usize,
}

/// Outputs of a render pass: the frame plus the clamped cursor/scroll and a
/// screen-row → result-index map for mouse hit-testing.
pub struct RenderOutput {
    pub ansi: String,
    pub selected: usize,
    pub scroll: usize,
    pub row_map: Vec<(usize, usize)>,
}

/// Render the picker frame.
pub fn render(input: RenderInput) -> RenderOutput {
    let RenderInput {
        permissions_granted,
        configured,
        query,
        results,
        open_paths,
        palette: p,
        rows,
        cols,
        total_candidates,
        ..
    } = input;

    let mut out = String::new();
    out.push_str("\u{1b}[2J");

    if rows == 0 || cols == 0 {
        return RenderOutput {
            ansi: out,
            selected: 0,
            scroll: 0,
            row_map: Vec::new(),
        };
    }

    let input_y = rows - 1;

    // Clamp the selection + scroll to the current result set, keeping the
    // selected row inside the visible window above the input.
    let total = results.len();
    let capacity = rows.saturating_sub(1).max(1);
    let selected = if total == 0 {
        0
    } else {
        input.selected.min(total - 1)
    };
    let mut scroll = input.scroll.min(selected);
    if selected >= scroll + capacity {
        scroll = selected + 1 - capacity;
    }
    if selected < scroll {
        scroll = selected;
    }

    // Hint states: no permissions, or nothing configured / discovered yet.
    if !permissions_granted {
        render_row(
            &mut out,
            0,
            input_y.saturating_sub(1),
            cols,
            None,
            &[Span::new(" waiting for permissions…", p.muted).dim()],
        );
    } else if total == 0 {
        let msg = if !configured {
            " no project folders configured"
        } else if query.trim().is_empty() {
            " no projects found"
        } else {
            " no matches"
        };
        render_row(
            &mut out,
            0,
            input_y.saturating_sub(1),
            cols,
            None,
            &[Span::new(msg, p.muted).dim()],
        );
    }

    // A project's parent path is only worth showing to disambiguate a name
    // collision, so count how many results share each basename; rows whose name
    // is unique render the name alone.
    let mut name_counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for r in results {
        *name_counts.entry(r.project.name.as_str()).or_insert(0) += 1;
    }

    // Results, best (results[scroll]) just above the input, worse ones higher up.
    let mut row_map = Vec::new();
    let visible_end = (scroll + capacity).min(total);
    for (k, idx) in (scroll..visible_end).enumerate() {
        let y = input_y - 1 - k;
        let r = &results[idx];
        let is_selected = idx == selected;
        let is_open = open_paths.contains(&r.project.path.to_string_lossy().to_string());
        let show_path = name_counts.get(r.project.name.as_str()).copied().unwrap_or(0) > 1;
        render_result_row(&mut out, y, cols, r, is_selected, is_open, show_path, p);
        row_map.push((y, idx));
    }

    // The input line on the bottom row: prompt + query + a block cursor, with a
    // right-aligned shown/total count.
    render_input_row(&mut out, input_y, cols, query, total, total_candidates, p);

    out.push_str(RESET);

    RenderOutput {
        ansi: out,
        selected,
        scroll,
        row_map,
    }
}

/// Render one result row: `<badge> <name>` plus a `<dim path>` only when
/// `show_path` (a name collision needs disambiguating). Matched ranges are *not*
/// recolored — the fuzzy hit isn't tinted — and the selected row is filled with
/// the selection background.
fn render_result_row(
    out: &mut String,
    y: usize,
    cols: usize,
    r: &Ranked,
    selected: bool,
    is_open: bool,
    show_path: bool,
    p: &Theme,
) {
    let row_bg = if selected { Some(p.selection_bg) } else { None };
    let mut spans = Vec::new();

    // Badge column (2 cells): a green dot for an already-open session, else blank.
    if is_open {
        spans.push(Span::new(" ", p.text));
        spans.push(Span::new(OPEN_BADGE, p.green));
    } else {
        spans.push(Span::new("  ", p.text));
    }
    spans.push(Span::new(" ", p.text));

    // Name. When a path follows (collision), keep it to ~half width so the path
    // has room; otherwise let the name use the full row. Matched chars are not
    // recolored — base and highlight styles are identical.
    let name_budget = if show_path {
        cols.saturating_sub(4).min(cols / 2 + 8).max(4)
    } else {
        cols.saturating_sub(4).max(4)
    };
    let name = truncate_text(&r.project.name, name_budget);
    let name_style = Style {
        fg: p.text,
        bold: true,
        dim: false,
    };
    push_highlighted(
        &mut spans,
        &name,
        &clip_ranges(&r.name_ranges, name.len()),
        name_style,
        name_style,
    );

    // The dimmed parent path, shown only to disambiguate a name collision.
    // Colored with the theme's text color (dimmed) rather than the muted/gray
    // slot, which can resolve to black on dark themes.
    if show_path {
        let used: usize = spans.iter().map(|s| s.text.width()).sum();
        let path_budget = cols.saturating_sub(used + 2);
        if path_budget > 1 {
            let path_style = Style {
                fg: p.text,
                bold: false,
                dim: true,
            };
            spans.push(styled("  ", path_style));
            let path = truncate_text(&r.project.display_path, path_budget);
            push_highlighted(
                &mut spans,
                &path,
                &clip_ranges(&r.path_ranges, path.len()),
                path_style,
                path_style,
            );
        }
    }

    render_row(out, 0, y, cols, row_bg, &spans);
}

/// Render the bottom input line.
fn render_input_row(
    out: &mut String,
    y: usize,
    cols: usize,
    query: &str,
    shown: usize,
    total: usize,
    p: &Theme,
) {
    let count = format!("{}/{} ", shown, total);
    let count_w = count.width();
    let prompt = "  ";
    // Width available for the typed query before the count.
    let query_budget = cols.saturating_sub(prompt.width() + count_w + 1).max(1);
    let shown_query = tail_text(query, query_budget);

    let mut spans = vec![
        Span::new("❯ ", p.accent).bold(),
        Span::new(shown_query, p.text),
        // A block cursor so the caret is visible without a real terminal cursor.
        Span::new("▏", p.accent),
    ];

    // Right-align the count by padding between it and the query.
    let left_w: usize = spans.iter().map(|s| s.text.width()).sum();
    if left_w + count_w < cols {
        spans.push(Span::new(" ".repeat(cols - left_w - count_w), p.text));
        spans.push(Span::new(count, p.muted).dim());
    }

    render_row(out, 0, y, cols, None, &spans);
}

/// A resolved text style for a highlight span.
#[derive(Clone, Copy)]
struct Style {
    fg: PaletteColor,
    bold: bool,
    dim: bool,
}

/// Split `text` into spans by `ranges` (byte ranges, sorted/merged, clipped to
/// `text`), styling matched runs with `hl` and the rest with `base`.
fn push_highlighted(spans: &mut Vec<Span>, text: &str, ranges: &[(usize, usize)], base: Style, hl: Style) {
    if ranges.is_empty() {
        spans.push(styled(text, base));
        return;
    }
    let mut cursor = 0usize;
    for &(start, end) in ranges {
        let start = start.min(text.len());
        let end = end.min(text.len());
        if start < end {
            if cursor < start {
                spans.push(styled(&text[cursor..start], base));
            }
            spans.push(styled(&text[start..end], hl));
            cursor = end;
        }
    }
    if cursor < text.len() {
        spans.push(styled(&text[cursor..], base));
    }
}

fn styled(text: &str, s: Style) -> Span {
    let mut span = Span::new(text.to_string(), s.fg);
    span.bold = s.bold;
    span.dim = s.dim;
    span
}

/// Keep only the ranges that fall within `len` (a truncated string), clamping
/// the tail range so a highlight never spills past the visible text.
fn clip_ranges(ranges: &[(usize, usize)], len: usize) -> Vec<(usize, usize)> {
    ranges
        .iter()
        .filter(|(s, _)| *s < len)
        .map(|&(s, e)| (s, e.min(len)))
        .collect()
}

/// Emit one row of styled spans at `(x, y)`, padded to `width` with `row_bg`.
/// (The selected row's background is re-asserted per span so it fills the width.)
fn render_row(out: &mut String, x: usize, y: usize, width: usize, row_bg: Option<PaletteColor>, spans: &[Span]) {
    out.push_str(&goto(x, y));
    if let Some(row_bg) = row_bg {
        out.push_str(&bg(row_bg));
    }
    let mut used = 0usize;
    for span in spans {
        if used >= width {
            break;
        }
        out.push_str(NORMAL_INTENSITY);
        if span.bold {
            out.push_str(BOLD);
        }
        if span.dim {
            out.push_str(DIM);
        }
        if let Some(row_bg) = row_bg {
            out.push_str(&bg(row_bg));
        }
        out.push_str(&fg(span.fg));
        out.push_str(&span.text);
        used += span.text.width();
    }
    if used < width {
        out.push_str(NORMAL_INTENSITY);
        if let Some(row_bg) = row_bg {
            out.push_str(&bg(row_bg));
        }
        out.push_str(&" ".repeat(width - used));
    }
    out.push_str(RESET);
}

/// Truncate `text` to `max_width` display columns with a trailing ellipsis.
fn truncate_text(text: &str, max_width: usize) -> String {
    let len = text.width();
    if len <= max_width {
        return text.to_string();
    }
    if max_width == 0 {
        return String::new();
    }
    if max_width == 1 {
        return "…".to_string();
    }
    let mut out = String::new();
    let mut w = 0usize;
    for ch in text.chars() {
        let cw = ch.to_string().width();
        if w + cw > max_width.saturating_sub(1) {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('…');
    out
}

/// Keep the trailing `max_width` columns of `text` (so the caret end of a long
/// query stays visible), with a leading ellipsis when clipped.
fn tail_text(text: &str, max_width: usize) -> String {
    let len = text.width();
    if len <= max_width {
        return text.to_string();
    }
    if max_width <= 1 {
        return "…".to_string();
    }
    let mut tail: Vec<char> = Vec::new();
    let mut w = 0usize;
    for ch in text.chars().rev() {
        let cw = ch.to_string().width();
        if w + cw > max_width - 1 {
            break;
        }
        tail.push(ch);
        w += cw;
    }
    tail.reverse();
    let mut out = String::from("…");
    out.extend(tail);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_adds_ellipsis() {
        assert_eq!(truncate_text("hello", 10), "hello");
        assert_eq!(truncate_text("hello-world", 6), "hello…");
    }

    #[test]
    fn tail_keeps_end() {
        assert_eq!(tail_text("abcdefgh", 4), "…fgh");
        assert_eq!(tail_text("abc", 10), "abc");
    }

    #[test]
    fn clip_ranges_drops_and_clamps() {
        assert_eq!(clip_ranges(&[(0, 3), (5, 9)], 6), vec![(0, 3), (5, 6)]);
    }

    #[test]
    fn highlighted_splits_into_runs() {
        let base = Style {
            fg: PaletteColor::EightBit(1),
            bold: false,
            dim: false,
        };
        let hl = Style {
            fg: PaletteColor::EightBit(2),
            bold: true,
            dim: false,
        };
        let mut spans = Vec::new();
        push_highlighted(&mut spans, "zellij", &[(0, 1), (2, 3)], base, hl);
        // z | el | l | ij  → 4 segments (z hl, e base, l hl, lij base)
        let texts: Vec<&str> = spans.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(texts, vec!["z", "e", "l", "lij"]);
        assert!(spans[0].bold && !spans[1].bold);
    }
}
