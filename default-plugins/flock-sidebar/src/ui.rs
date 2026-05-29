//! Sidebar rendering, ported from herdr's `ui/sidebar.rs` + `ui/status.rs` and
//! re-targeted from `ratatui` onto raw-ANSI output.
//!
//! The sidebar has two sections, matching herdr's split:
//!
//! - **sessions** — one row per zellij session with a single status dot that
//!   rolls up that session's agents (Blocked > Done-unseen > Working >
//!   Idle-seen > Unknown). The *current* session's rollup is computed from live
//!   per-pane state; other sessions can only be seen as metadata (their pane
//!   commands) until the cross-session bus lands in Phase 7, so they show a
//!   neutral "agents present" marker rather than a real state.
//! - **agents** — one row per agent pane *in the current session*: a state icon
//!   and a label. The icon alone carries the state (color + glyph), so there is
//!   no status word.
//!
//! Navigation is keyboard-first: a single selection cursor moves over the
//! sessions then the agents (Up/Down or k/j), and Enter activates the selected
//! row (switch session / focus pane). Mouse click and scroll mirror the same
//! actions but are not required.
//!
//! Colors come from the user's active zellij theme (see [`Theme`](crate::palette)),
//! rendered as raw ANSI so backgrounds, the scrollbar, and the spinner stay
//! precise while still matching whatever theme is configured.

use std::collections::BTreeMap;

use unicode_width::UnicodeWidthStr;
use zellij_tile::prelude::{PaneId, PaneManifest, PaletteColor, SessionInfo, TabInfo};

use crate::detect::{identify_agent_from_command, AgentState};
use crate::palette::{bg, fg, goto, Theme, BOLD, DIM, NORMAL_INTENSITY, RESET};
use crate::state::PaneAgentState;

// Braille spinner frames — smooth rotation. Ported verbatim from herdr's
// `ui.rs`. The plugin advances `spinner_tick` once per animation timer fire
// (~8/sec) so it indexes the frames directly rather than herdr's /8 at 60fps.
const SPINNERS: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Map the animation tick to a spinner frame.
pub fn spinner_frame(tick: u32) -> &'static str {
    SPINNERS[(tick as usize) % SPINNERS.len()]
}

/// Coarse per-session activity for the sessions-overview dot. The sessions
/// section deliberately collapses the full per-pane state into three buckets:
/// no agents → one or more agents stopped → at least one agent running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionActivity {
    /// No agents in the session.
    None,
    /// One or more agents present, but none actively working (idle / blocked /
    /// done — i.e. stopped, possibly waiting on the user).
    Stopped,
    /// At least one agent is actively working.
    Running,
}

/// The current session's activity, from its live per-pane state.
fn current_session_activity(agents: &BTreeMap<PaneId, PaneAgentState>) -> SessionActivity {
    let mut any = false;
    for st in agents.values() {
        if !st.is_agent() {
            continue;
        }
        if st.state == AgentState::Working {
            return SessionActivity::Running;
        }
        any = true;
    }
    if any {
        SessionActivity::Stopped
    } else {
        SessionActivity::None
    }
}

/// The dot glyph + color for a session's activity. Filled green = running,
/// filled yellow = stopped (worth a glance), dim dot = nothing here.
fn activity_dot(activity: SessionActivity, p: &Theme) -> (&'static str, PaletteColor) {
    match activity {
        SessionActivity::Running => ("●", p.green),
        SessionActivity::Stopped => ("●", p.yellow),
        SessionActivity::None => ("·", p.muted),
    }
}

/// The animated agent icon + its color, ported from herdr's `status::agent_icon`.
fn agent_icon(state: AgentState, seen: bool, tick: u32, p: &Theme) -> (&'static str, PaletteColor) {
    match (state, seen) {
        (AgentState::Blocked, _) => ("◉", p.red),
        (AgentState::Working, _) => (spinner_frame(tick), p.yellow),
        (AgentState::Idle, false) => ("●", p.teal),
        (AgentState::Idle, true) => ("✓", p.green),
        (AgentState::Unknown, _) => ("○", p.muted),
    }
}

/// Count the panes in another session that look like agents, from their command
/// metadata alone. We can't see those sessions' screens (so no live state until
/// the Phase 7 bus), but the running command is enough to know an agent is there.
fn session_agent_count(session: &SessionInfo) -> usize {
    session
        .panes
        .panes
        .values()
        .flatten()
        .filter(|pane| !pane.is_plugin)
        .filter(|pane| {
            pane.terminal_command.as_deref().is_some_and(|cmd| {
                let argv: Vec<String> = cmd.split_whitespace().map(String::from).collect();
                identify_agent_from_command(&argv).is_some()
            })
        })
        .count()
}

/// A single agent row in the panel: a state icon and a label. No status word —
/// the icon's glyph and color carry the state.
pub struct AgentEntry {
    pub pane_id: PaneId,
    /// Display label: the agent name, or `tab·agent` when the session has more
    /// than one tab (matching herdr's multi-tab `pane_details` labelling).
    pub label: String,
    pub state: AgentState,
    /// Whether the user has looked at this pane since it last changed. Phase 4
    /// wires real tracking; until then everything is treated as seen.
    pub seen: bool,
    /// Whether this is the focused pane in the focused tab.
    pub is_active: bool,
}

/// What a navigable row points at — used for keyboard Enter and mouse clicks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// Switch to (or focus) the session with this name.
    Session(String),
    /// Focus this agent pane.
    Pane(PaneId),
}

/// A rendered row's click target: which absolute pane row it occupies and which
/// selection index it corresponds to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClickTarget {
    pub row: usize,
    pub index: usize,
}

/// Build the agent list from the session's panes, in tab then pane order, one
/// entry per pane that detection has tagged as an agent.
pub fn build_entries(
    panes: &PaneManifest,
    tabs: &[TabInfo],
    agents: &BTreeMap<PaneId, PaneAgentState>,
) -> Vec<AgentEntry> {
    let multi_tab = tabs.len() > 1;
    let tab_active: BTreeMap<usize, bool> =
        tabs.iter().map(|tab| (tab.position, tab.active)).collect();
    let tab_name: BTreeMap<usize, String> = tabs
        .iter()
        .map(|tab| (tab.position, tab.name.clone()))
        .collect();

    let mut entries = Vec::new();
    // `panes.panes` is a BTreeMap keyed by tab position, so iteration is already
    // in tab order.
    for (tab_idx, panes_in_tab) in &panes.panes {
        for pane in panes_in_tab {
            let pane_id = if pane.is_plugin {
                PaneId::Plugin(pane.id)
            } else {
                PaneId::Terminal(pane.id)
            };
            let Some(st) = agents.get(&pane_id) else {
                continue;
            };
            if !st.is_agent() {
                continue;
            }
            let agent_label = st.effective_agent_label().unwrap_or_else(|| "?".to_string());
            let label = if multi_tab {
                let tab = tab_name
                    .get(tab_idx)
                    .filter(|name| !name.is_empty())
                    .cloned()
                    .unwrap_or_else(|| format!("tab {}", tab_idx + 1));
                format!("{tab}·{agent_label}")
            } else {
                agent_label
            };
            let is_active = pane.is_focused && tab_active.get(tab_idx).copied().unwrap_or(false);
            entries.push(AgentEntry {
                pane_id,
                label,
                state: st.state,
                seen: st.seen,
                is_active,
            });
        }
    }
    entries
}

/// The ordered list of navigable targets: every session (in list order) then
/// every agent in the current session (in [`build_entries`] order). The same
/// ordering is used by [`render`], so a selection index maps consistently
/// whether it came from a keypress or a click.
pub fn navigable_targets(
    panes: &PaneManifest,
    tabs: &[TabInfo],
    agents: &BTreeMap<PaneId, PaneAgentState>,
    sessions: &[SessionInfo],
) -> Vec<Target> {
    let mut targets: Vec<Target> = sessions
        .iter()
        .map(|s| Target::Session(s.name.clone()))
        .collect();
    targets.extend(
        build_entries(panes, tabs, agents)
            .into_iter()
            .map(|e| Target::Pane(e.pane_id)),
    );
    targets
}

/// Clamp a selection index to the navigable target count.
pub fn clamp_selection(selected: usize, total: usize) -> usize {
    selected.min(total.saturating_sub(1))
}

/// One styled run of text within a rendered row.
struct Span {
    text: String,
    fg: PaletteColor,
    bold: bool,
    dim: bool,
}

impl Span {
    fn new(text: impl Into<String>, fg: PaletteColor) -> Self {
        Self {
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

/// Emit one row of styled spans at `(x, y)`, padded to `width` with `row_bg`
/// (when set) and terminated with a full reset. A leading background is held
/// across spans (an intensity reset doesn't clear it) so a selected row's
/// highlight fills the whole width.
fn render_row(
    out: &mut String,
    x: usize,
    y: usize,
    width: usize,
    row_bg: Option<PaletteColor>,
    spans: &[Span],
) {
    out.push_str(&goto(x, y));
    if let Some(row_bg) = row_bg {
        out.push_str(&bg(row_bg));
    }
    let mut used = 0usize;
    for span in spans {
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

/// Truncate `text` to `max_width` display columns, with an ellipsis. Ported
/// from herdr's `sidebar::truncate_text`.
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

/// The full sidebar render input.
pub struct RenderInput<'a> {
    pub permissions_granted: bool,
    pub panes: &'a PaneManifest,
    pub tabs: &'a [TabInfo],
    pub agents: &'a BTreeMap<PaneId, PaneAgentState>,
    pub sessions: &'a [SessionInfo],
    pub palette: &'a Theme,
    /// Unified selection cursor over sessions-then-agents.
    pub selected: usize,
    /// Scroll offset into the agent list.
    pub scroll: usize,
    pub spinner_tick: u32,
    pub rows: usize,
    pub cols: usize,
}

/// The full sidebar render output.
pub struct RenderOutput {
    /// The raw-ANSI frame to print.
    pub ansi: String,
    /// Selection index after clamping to the target count.
    pub selected: usize,
    /// Agent-list scroll offset after clamping / keeping the selection visible.
    pub scroll: usize,
    /// Click targets for the rows drawn this frame.
    pub click_map: Vec<ClickTarget>,
}

/// Render the whole sidebar to a raw-ANSI string plus the click map.
pub fn render(input: RenderInput) -> RenderOutput {
    let p = input.palette;
    let cols = input.cols;
    let rows = input.rows;
    let mut out = String::new();
    let mut click_map = Vec::new();

    // Clear the pane so stale rows from a taller previous frame don't linger.
    out.push_str("\u{1b}[2J");

    if !input.permissions_granted {
        render_row(
            &mut out,
            0,
            0,
            cols,
            None,
            &[Span::new("waiting for permissions…", p.yellow)],
        );
        return RenderOutput {
            ansi: out,
            selected: 0,
            scroll: 0,
            click_map,
        };
    }

    let entries = build_entries(input.panes, input.tabs, input.agents);
    let n_sessions = input.sessions.len();
    let total_targets = n_sessions + entries.len();
    let selected = clamp_selection(input.selected, total_targets);

    let mut y = 0usize;

    // ---- sessions section ----
    render_row(&mut out, 0, y, cols, None, &[Span::new(" sessions", p.muted).bold()]);
    y += 1;

    if input.sessions.is_empty() {
        render_row(&mut out, 0, y, cols, None, &[Span::new(" (none)", p.muted).dim()]);
        y += 1;
    } else {
        for (i, session) in input.sessions.iter().enumerate() {
            if y >= rows {
                break;
            }
            let activity = if session.is_current_session {
                current_session_activity(input.agents)
            } else if session_agent_count(session) > 0 {
                // Other sessions: we can see agents exist but not whether they're
                // running until the cross-session bus (Phase 7), so presence maps
                // to the "stopped" bucket.
                SessionActivity::Stopped
            } else {
                SessionActivity::None
            };
            let (dot, dot_color) = activity_dot(activity, p);
            let is_selected = i == selected;
            let row_bg = is_selected.then_some(p.selection_bg);
            let name_color = if is_selected || session.is_current_session {
                p.text
            } else {
                p.muted
            };
            let mut name = Span::new(session.name.clone(), name_color);
            if is_selected || session.is_current_session {
                name = name.bold();
            }
            render_row(
                &mut out,
                0,
                y,
                cols,
                row_bg,
                &[Span::new(" ", p.text), Span::new(dot, dot_color), Span::new(" ", p.text), name],
            );
            click_map.push(ClickTarget { row: y, index: i });
            y += 1;
        }
    }

    // ---- divider ----
    if y < rows {
        render_row(&mut out, 0, y, cols, None, &[Span::new("─".repeat(cols), p.separator)]);
        y += 1;
    }

    // ---- agents header ----
    if y < rows {
        render_row(
            &mut out,
            0,
            y,
            cols,
            None,
            &[
                Span::new(" agents", p.muted).bold(),
                Span::new(format!("  {}", entries.len()), p.muted).dim(),
            ],
        );
        y += 1;
    }

    let body_start = y;
    let body_height = rows.saturating_sub(body_start);

    if entries.is_empty() {
        if body_start < rows {
            render_row(
                &mut out,
                0,
                body_start,
                cols,
                None,
                &[Span::new(" no agents in this session", p.muted).dim()],
            );
        }
        return RenderOutput {
            ansi: out,
            selected,
            scroll: 0,
            click_map,
        };
    }

    // One row per agent. Keep the selected agent (if the cursor is in this
    // section) within the visible window.
    let total = entries.len();
    let visible = body_height;
    let mut scroll = input.scroll.min(total.saturating_sub(visible));
    if selected >= n_sessions {
        let agent_idx = selected - n_sessions;
        if agent_idx < scroll {
            scroll = agent_idx;
        } else if visible > 0 && agent_idx >= scroll + visible {
            scroll = agent_idx + 1 - visible;
        }
    }

    let show_scrollbar = total > visible && body_height > 0;
    let content_width = cols.saturating_sub(usize::from(show_scrollbar));

    let mut row_y = body_start;
    let body_bottom = body_start + body_height;
    for (j, entry) in entries.iter().enumerate().skip(scroll) {
        if row_y >= body_bottom {
            break;
        }
        let index = n_sessions + j;
        let is_selected = index == selected;
        let row_bg = is_selected.then_some(p.selection_bg);
        let (icon, icon_color) = agent_icon(entry.state, entry.seen, input.spinner_tick, p);
        let name_color = if is_selected || entry.is_active {
            p.text
        } else {
            p.muted
        };
        let label = truncate_text(&entry.label, content_width.saturating_sub(3));
        let mut name = Span::new(label, name_color);
        if is_selected || entry.is_active {
            name = name.bold();
        }
        render_row(
            &mut out,
            0,
            row_y,
            content_width,
            row_bg,
            &[Span::new(" ", p.text), Span::new(icon, icon_color), Span::new(" ", p.text), name],
        );
        click_map.push(ClickTarget { row: row_y, index });
        row_y += 1;
    }

    if show_scrollbar {
        render_scrollbar(&mut out, cols.saturating_sub(1), body_start, body_height, total, visible, scroll, p);
    }

    RenderOutput {
        ansi: out,
        selected,
        scroll,
        click_map,
    }
}

/// Draw a top-down scrollbar in column `x` over `body_height` rows. Thumb size
/// and position follow herdr's `scrollbar::scrollbar_thumb` math, simplified for
/// the plugin's scroll-from-top model.
#[allow(clippy::too_many_arguments)]
fn render_scrollbar(
    out: &mut String,
    x: usize,
    body_start: usize,
    body_height: usize,
    total: usize,
    visible: usize,
    scroll: usize,
    p: &Theme,
) {
    if body_height == 0 || total <= visible {
        return;
    }
    let thumb_len = ((visible * body_height) as f32 / total as f32)
        .round()
        .max(1.0)
        .min(body_height as f32) as usize;
    let max_thumb_top = body_height.saturating_sub(thumb_len);
    let max_scroll = total.saturating_sub(visible);
    let thumb_top = if max_thumb_top == 0 || max_scroll == 0 {
        0
    } else {
        ((scroll * max_thumb_top) as f32 / max_scroll as f32)
            .round()
            .min(max_thumb_top as f32) as usize
    };

    for i in 0..body_height {
        let (symbol, color) = if i >= thumb_top && i < thumb_top + thumb_len {
            ("▐", p.accent)
        } else {
            ("▕", p.separator)
        };
        render_row(out, x, body_start + i, 1, None, &[Span::new(symbol, color)]);
    }
}

/// Map a clicked row to the selection index whose row it occupies, if any.
pub fn index_at_row(click_map: &[ClickTarget], row: usize) -> Option<usize> {
    click_map
        .iter()
        .find(|hit| hit.row == row)
        .map(|hit| hit.index)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_selection_bounds_to_target_count() {
        assert_eq!(clamp_selection(0, 5), 0);
        assert_eq!(clamp_selection(9, 5), 4);
        assert_eq!(clamp_selection(2, 5), 2);
        assert_eq!(clamp_selection(0, 0), 0);
    }

    #[test]
    fn index_at_row_finds_matching_click_target() {
        let map = vec![
            ClickTarget { row: 1, index: 0 },
            ClickTarget { row: 5, index: 3 },
        ];
        assert_eq!(index_at_row(&map, 1), Some(0));
        assert_eq!(index_at_row(&map, 5), Some(3));
        assert_eq!(index_at_row(&map, 2), None);
    }

    #[test]
    fn truncate_text_adds_ellipsis_when_too_wide() {
        assert_eq!(truncate_text("claude", 10), "claude");
        assert_eq!(truncate_text("claude-code", 6), "claud…");
        assert_eq!(truncate_text("x", 1), "x");
        assert_eq!(truncate_text("xy", 1), "…");
    }

    #[test]
    fn agent_icon_uses_spinner_for_working() {
        let p = Theme::default();
        let (icon, color) = agent_icon(AgentState::Working, true, 0, &p);
        assert_eq!(icon, SPINNERS[0]);
        assert_eq!(color, p.yellow);
        let (done_icon, done_color) = agent_icon(AgentState::Idle, false, 0, &p);
        assert_eq!(done_icon, "●");
        assert_eq!(done_color, p.teal);
    }

    #[test]
    fn current_session_activity_buckets_into_three_states() {
        let mut agents: BTreeMap<PaneId, PaneAgentState> = BTreeMap::new();

        // No agents → None.
        assert_eq!(current_session_activity(&agents), SessionActivity::None);

        // An idle/blocked agent (not working) → Stopped.
        let mut blocked = PaneAgentState::new();
        blocked.detected_agent = Some(crate::detect::Agent::Codex);
        blocked.state = AgentState::Blocked;
        agents.insert(PaneId::Terminal(1), blocked);
        assert_eq!(current_session_activity(&agents), SessionActivity::Stopped);

        // Any working agent → Running, regardless of the others.
        let mut working = PaneAgentState::new();
        working.detected_agent = Some(crate::detect::Agent::Claude);
        working.state = AgentState::Working;
        agents.insert(PaneId::Terminal(2), working);
        assert_eq!(current_session_activity(&agents), SessionActivity::Running);
    }
}
