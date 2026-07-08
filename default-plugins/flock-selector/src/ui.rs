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

use crate::codespaces::{GhError, RankedCodespace, StateKind};
use crate::live_sessions::{RankedSession, SessionActivity};
use crate::palette::{bg, fg, goto, Theme, BOLD, DIM, NORMAL_INTENSITY, RESET};
use crate::ranking::Ranked;

/// The badge glyph marking a project/codespace that already has a live session.
const OPEN_BADGE: &str = "●";

/// Which list the picker is showing. Tab cycles through them; each keeps the
/// same reverse-layout fuzzy list, differing only in rows and data source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PickerMode {
    #[default]
    Sessions,
    Projects,
    Codespaces,
}

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
    /// Which list is showing; selects between `session_results`, `results`,
    /// and `codespace_results`.
    pub mode: PickerMode,
    /// Ranked live sessions (Sessions mode).
    pub session_results: &'a [RankedSession<'a>],
    pub results: &'a [Ranked<'a>],
    /// Absolute project paths that currently have a live session.
    pub open_paths: &'a std::collections::HashSet<String>,
    /// Ranked codespaces (Codespaces mode).
    pub codespace_results: &'a [RankedCodespace<'a>],
    /// Codespace names that currently have a live bound session.
    pub bound_codespaces: &'a std::collections::HashSet<String>,
    /// The latest `gh` failure, rendered as an actionable hint line.
    pub codespaces_error: Option<&'a GhError>,
    /// Whether a live `gh codespace list` is in flight.
    pub codespaces_refreshing: bool,
    /// The codespace name a stop is pending for, if any.
    pub pending_stop: Option<&'a str>,
    pub palette: &'a Theme,
    /// Selection cursor: absolute index into the active results (0 = best,
    /// bottom-most).
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
        mode,
        session_results,
        results,
        open_paths,
        codespace_results,
        bound_codespaces,
        codespaces_error,
        codespaces_refreshing,
        pending_stop,
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

    // The mode header (Projects · Codespaces) takes the top row when there's
    // room for it alongside the input row and at least one result row.
    let has_header = rows >= 3;
    if has_header {
        render_header_row(&mut out, cols, mode, p);
    }

    // Clamp the selection + scroll to the current result set, keeping the
    // selected row inside the visible window above the input.
    // `capacity` is the rows available for results: everything except the
    // input line and (when shown) the header. 0 when the pane is a single row
    // tall, in which case no result rows render (the loop below would
    // otherwise compute `input_y - 1 - k` with input_y == 0 and underflow).
    let total = match mode {
        PickerMode::Sessions => session_results.len(),
        PickerMode::Projects => results.len(),
        PickerMode::Codespaces => codespace_results.len(),
    };
    let capacity = rows.saturating_sub(if has_header { 2 } else { 1 });
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

    // Hint states: no permissions, a gh failure, or an empty list.
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
        let spans = match mode {
            PickerMode::Sessions => {
                let msg = if query.trim().is_empty() {
                    " no open sessions"
                } else {
                    " no matches"
                };
                vec![Span::new(msg, p.muted).dim()]
            },
            PickerMode::Projects => {
                let msg = if !configured {
                    " no project folders configured"
                } else if query.trim().is_empty() {
                    " no projects found"
                } else {
                    " no matches"
                };
                vec![Span::new(msg, p.muted).dim()]
            },
            PickerMode::Codespaces => match codespaces_error {
                Some(err) => codespaces_error_spans(err, p),
                None => {
                    let msg = if codespaces_refreshing {
                        " loading codespaces…"
                    } else if query.trim().is_empty() {
                        " no codespaces"
                    } else {
                        " no matches"
                    };
                    vec![Span::new(msg, p.muted).dim()]
                },
            },
        };
        render_row(&mut out, 0, input_y.saturating_sub(1), cols, None, &spans);
    }

    let mut row_map = Vec::new();
    let visible_end = (scroll + capacity).min(total);
    match mode {
        PickerMode::Sessions => {
            for (k, idx) in (scroll..visible_end).enumerate() {
                let y = input_y - 1 - k;
                let r = &session_results[idx];
                let is_selected = idx == selected;
                render_session_row(&mut out, y, cols, r, is_selected, p);
                row_map.push((y, idx));
            }
        },
        PickerMode::Projects => {
            // A project's parent path is only worth showing to disambiguate a
            // name collision, so count how many results share each basename;
            // rows whose name is unique render the name alone.
            let mut name_counts: std::collections::HashMap<&str, usize> =
                std::collections::HashMap::new();
            for r in results {
                *name_counts.entry(r.project.name.as_str()).or_insert(0) += 1;
            }

            // Results, best (results[scroll]) just above the input, worse ones
            // higher up.
            for (k, idx) in (scroll..visible_end).enumerate() {
                let y = input_y - 1 - k;
                let r = &results[idx];
                let is_selected = idx == selected;
                let is_open = open_paths.contains(&r.project.path.to_string_lossy().to_string());
                let show_path =
                    name_counts.get(r.project.name.as_str()).copied().unwrap_or(0) > 1;
                render_result_row(&mut out, y, cols, r, is_selected, is_open, show_path, p);
                row_map.push((y, idx));
            }
        },
        PickerMode::Codespaces => {
            for (k, idx) in (scroll..visible_end).enumerate() {
                let y = input_y - 1 - k;
                let r = &codespace_results[idx];
                let is_selected = idx == selected;
                let is_bound = bound_codespaces.contains(&r.codespace.name);
                let is_stopping = pending_stop == Some(r.codespace.name.as_str());
                render_codespace_row(&mut out, y, cols, r, is_selected, is_bound, is_stopping, p);
                row_map.push((y, idx));
            }
        },
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

/// The top header row: every mode name with the active one highlighted, plus a
/// right-aligned key hint.
fn render_header_row(out: &mut String, cols: usize, mode: PickerMode, p: &Theme) {
    let active = Style {
        fg: p.accent,
        bold: true,
        dim: false,
    };
    let inactive = Style {
        fg: p.text,
        bold: false,
        dim: true,
    };
    let style_for = |m: PickerMode| if m == mode { active } else { inactive };
    let mut spans = vec![
        Span::new(" ", p.text),
        styled("Sessions", style_for(PickerMode::Sessions)),
        Span::new(" · ", p.text).dim(),
        styled("Projects", style_for(PickerMode::Projects)),
        Span::new(" · ", p.text).dim(),
        styled("Codespaces", style_for(PickerMode::Codespaces)),
    ];

    let hint = match mode {
        PickerMode::Sessions | PickerMode::Projects => "Tab ",
        PickerMode::Codespaces => "Tab switch · Ctrl-x stop ",
    };
    let left_w: usize = spans.iter().map(|s| s.text.width()).sum();
    let hint_w = hint.width();
    if left_w + hint_w < cols {
        spans.push(Span::new(" ".repeat(cols - left_w - hint_w), p.text));
        spans.push(Span::new(hint, p.muted).dim());
    }
    render_row(out, 0, 0, cols, None, &spans);
}

/// The hint line for a gh failure: a red marker plus an actionable message.
fn codespaces_error_spans(err: &GhError, p: &Theme) -> Vec<Span> {
    let msg = match err {
        GhError::GhMissing => " gh not found — install the GitHub CLI".to_owned(),
        GhError::NotAuthed => " gh not authenticated — run: gh auth login".to_owned(),
        GhError::MissingScope => {
            " missing codespace scope — run: gh auth refresh -h github.com -s codespace".to_owned()
        },
        GhError::Other(detail) => format!(" gh error: {}", detail),
    };
    vec![Span::new(" ✗", p.red), Span::new(msg, p.muted).dim()]
}

/// The dim `(current)` marker suffixing the session the picker runs in — its
/// badge column carries the attention dot like every other row, so "current"
/// moves into the name column.
const CURRENT_SUFFIX: &str = " (current)";

/// A session's agent-attention dot, mirroring the flock-sidebar's session
/// overview dot so both UIs read the same: a blocked agent is the red ◉,
/// done-unseen teal, running green, idle yellow, no agents a dim ○.
fn activity_dot(activity: SessionActivity, p: &Theme) -> Span {
    match activity {
        SessionActivity::Blocked => Span::new("◉", p.red),
        SessionActivity::DoneUnseen => Span::new("●", p.teal),
        SessionActivity::Running => Span::new("●", p.green),
        SessionActivity::Stopped => Span::new("●", p.yellow),
        SessionActivity::None => Span::new("○", p.muted).dim(),
    }
}

/// Render one session row: `<dot> <name>  <dim workspace path>`. The badge
/// column holds the session's agent-attention dot (see [`activity_dot`] — it
/// is also what the attention-first ordering sorts by); the session the picker
/// runs in is marked with a dim `(current)` suffix on its name instead of a
/// badge.
fn render_session_row(
    out: &mut String,
    y: usize,
    cols: usize,
    r: &RankedSession,
    selected: bool,
    p: &Theme,
) {
    let row_bg = if selected { Some(p.selection_bg) } else { None };
    let mut spans = Vec::new();

    // Badge column (2 cells): the agent-attention dot.
    spans.push(Span::new(" ", p.text));
    spans.push(activity_dot(r.entry.activity, p));
    spans.push(Span::new(" ", p.text));

    // Session name, highlighted like a project name. When a path follows, keep
    // it to ~half width so the path has room; the current session's suffix
    // spends part of the name budget so the row still fits.
    let suffix_width = if r.entry.is_current {
        CURRENT_SUFFIX.width()
    } else {
        0
    };
    let name_budget = if r.entry.display_path.is_empty() {
        cols.saturating_sub(4 + suffix_width).max(4)
    } else {
        cols.saturating_sub(4 + suffix_width)
            .min(cols / 2 + 8)
            .max(4)
    };
    let name = truncate_text(&r.entry.name, name_budget);
    let name_style = Style {
        fg: p.text,
        bold: true,
        dim: false,
    };
    push_highlighted(
        &mut spans,
        &name,
        &clip_ranges(&r.name_ranges, &name),
        name_style,
        name_style,
    );
    if r.entry.is_current {
        spans.push(Span::new(CURRENT_SUFFIX, p.text).dim());
    }

    // The dimmed workspace path, truncated to what's left.
    if !r.entry.display_path.is_empty() {
        let used: usize = spans.iter().map(|s| s.text.width()).sum();
        let path_budget = cols.saturating_sub(used + 2);
        if path_budget > 1 {
            let path_style = Style {
                fg: p.text,
                bold: false,
                dim: true,
            };
            spans.push(styled("  ", path_style));
            let path = truncate_text(&r.entry.display_path, path_budget);
            push_highlighted(
                &mut spans,
                &path,
                &clip_ranges(&r.path_ranges, &path),
                path_style,
                path_style,
            );
        }
    }

    render_row(out, 0, y, cols, row_bg, &spans);
}

/// Render one codespace row: `<badge> <display name>  <repo>  <state>`. The
/// badge column mirrors the projects list (a green dot when a live session is
/// bound); the state renders as a colored word so "will boot on connect" vs.
/// "ready" is visible at a glance.
fn render_codespace_row(
    out: &mut String,
    y: usize,
    cols: usize,
    r: &RankedCodespace,
    selected: bool,
    is_bound: bool,
    is_stopping: bool,
    p: &Theme,
) {
    let row_bg = if selected { Some(p.selection_bg) } else { None };
    let mut spans = Vec::new();

    // Badge column (2 cells): a green dot for an already-bound live session.
    if is_bound {
        spans.push(Span::new(" ", p.text));
        spans.push(Span::new(OPEN_BADGE, p.green));
    } else {
        spans.push(Span::new("  ", p.text));
    }
    spans.push(Span::new(" ", p.text));

    // Display name, highlighted like a project name.
    let name_budget = cols.saturating_sub(4).min(cols / 2 + 8).max(4);
    let name = truncate_text(&r.codespace.display_name, name_budget);
    let name_style = Style {
        fg: p.text,
        bold: true,
        dim: false,
    };
    push_highlighted(
        &mut spans,
        &name,
        &clip_ranges(&r.name_ranges, &name),
        name_style,
        name_style,
    );

    // The state word (colored) goes at the end; reserve its width so the repo
    // truncates before the state does.
    let (state_text, state_color, state_dim) = if is_stopping {
        ("stopping…".to_owned(), p.yellow, false)
    } else {
        match r.codespace.state_kind() {
            StateKind::Running => (r.codespace.state.clone(), p.green, false),
            StateKind::Stopped => (r.codespace.state.clone(), p.text, true),
            StateKind::Busy => (r.codespace.state.clone(), p.yellow, false),
            StateKind::Unknown => (r.codespace.state.clone(), p.muted, true),
        }
    };
    let state_w = state_text.width() + 2;

    // The dimmed repository, truncated to what's left.
    if !r.codespace.repository.is_empty() {
        let used: usize = spans.iter().map(|s| s.text.width()).sum();
        let repo_budget = cols.saturating_sub(used + state_w + 2);
        if repo_budget > 1 {
            let repo_style = Style {
                fg: p.text,
                bold: false,
                dim: true,
            };
            spans.push(styled("  ", repo_style));
            let repo = truncate_text(&r.codespace.repository, repo_budget);
            push_highlighted(
                &mut spans,
                &repo,
                &clip_ranges(&r.repo_ranges, &repo),
                repo_style,
                repo_style,
            );
        }
    }

    if !state_text.is_empty() {
        let used: usize = spans.iter().map(|s| s.text.width()).sum();
        if used + state_w <= cols {
            spans.push(Span::new("  ", p.text));
            let mut state_span = Span::new(state_text, state_color);
            state_span.dim = state_dim;
            spans.push(state_span);
        }
    }

    render_row(out, 0, y, cols, row_bg, &spans);
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
        &clip_ranges(&r.name_ranges, &name),
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
                &clip_ranges(&r.path_ranges, &path),
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

/// Keep only the ranges that fall within `text`, clamping so a highlight never
/// spills past the visible text. The ranges were computed against the original
/// (untruncated) string, while `text` may be a truncated copy ending in a
/// multi-byte '…' — so every endpoint must also be snapped down to a char
/// boundary or slicing in `push_highlighted` panics mid-'…'.
fn clip_ranges(ranges: &[(usize, usize)], text: &str) -> Vec<(usize, usize)> {
    let snap = |i: usize| {
        let mut i = i.min(text.len());
        while i > 0 && !text.is_char_boundary(i) {
            i -= 1;
        }
        i
    };
    ranges
        .iter()
        .map(|&(s, e)| (snap(s), snap(e)))
        .filter(|(s, e)| s < e)
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

    /// A pane squeezed to a single row has no room for result rows; the
    /// render loop must not run (it would compute `input_y - 1 - k` with
    /// input_y == 0 and underflow).
    #[test]
    fn render_at_one_row_does_not_underflow() {
        let c = PaletteColor::EightBit(1);
        let theme = Theme {
            text: c,
            muted: c,
            separator: c,
            selection_bg: c,
            accent: c,
            red: c,
            yellow: c,
            green: c,
            teal: c,
            blue: c,
        };
        let project = crate::discovery::Project {
            path: std::path::PathBuf::from("/tmp/proj"),
            name: "proj".to_string(),
            display_path: "~/proj".to_string(),
        };
        let results = vec![Ranked {
            project: &project,
            rank: 1.0,
            name_ranges: Vec::new(),
            path_ranges: Vec::new(),
        }];
        let out = render(RenderInput {
            permissions_granted: true,
            configured: true,
            query: "p",
            mode: PickerMode::Projects,
            session_results: &[],
            results: &results,
            open_paths: &std::collections::HashSet::new(),
            codespace_results: &[],
            bound_codespaces: &std::collections::HashSet::new(),
            codespaces_error: None,
            codespaces_refreshing: false,
            pending_stop: None,
            palette: &theme,
            selected: 0,
            scroll: 0,
            total_candidates: 1,
            rows: 1,
            cols: 40,
        });
        assert!(out.row_map.is_empty(), "no result rows fit in a 1-row pane");
    }

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
        assert_eq!(
            clip_ranges(&[(0, 3), (5, 9)], "abcdef"),
            vec![(0, 3), (5, 6)]
        );
    }

    /// Ranges computed on the original string can land inside the multi-byte
    /// '…' of a truncated copy; they must be snapped to char boundaries (and
    /// dropped when they collapse), never sliced mid-char.
    #[test]
    fn clip_ranges_snaps_to_char_boundaries() {
        // truncate_text("abcdefgh", 5) == "abcd…": '…' occupies bytes 4..7.
        let truncated = truncate_text("abcdefgh", 5);
        assert_eq!(truncated, "abcd…");
        // 'f' matched at (5, 6) in the original — inside '…' here. Snapped
        // endpoints collapse, so the range is dropped entirely.
        assert_eq!(clip_ranges(&[(5, 6)], &truncated), vec![]);
        // A range straddling the ellipsis keeps the boundary-safe part.
        assert_eq!(clip_ranges(&[(3, 6)], &truncated), vec![(3, 4)]);
        // Rendering with the clipped ranges must not panic.
        let mut spans = Vec::new();
        let style = Style {
            fg: PaletteColor::EightBit(1),
            bold: false,
            dim: false,
        };
        push_highlighted(
            &mut spans,
            &truncated,
            &clip_ranges(&[(5, 6), (3, 6)], &truncated),
            style,
            style,
        );
        let joined: String = spans.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(joined, "abcd…");
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
