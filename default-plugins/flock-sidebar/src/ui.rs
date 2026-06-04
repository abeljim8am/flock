//! Sidebar rendering, ported from herdr's `ui/sidebar.rs` + `ui/status.rs` and
//! re-targeted from `ratatui` onto raw-ANSI output.
//!
//! The sidebar has two sections, matching herdr's split:
//!
//! - **sessions** — one row per zellij session with a single status dot that
//!   rolls its agents up to the most attention-worthy one, by herdr's priority
//!   (Blocked > Done-unseen > Working > Idle > none): a session waiting on the
//!   user shows the same red ◉ as a blocked agent, a background completion shows
//!   teal, a working agent green. The *current* session's rollup is computed
//!   from live per-pane state; other sessions' rollups come from the state they
//!   publish to the cross-session bus (Phase 7), carried on
//!   `SessionInfo.agent_states`, so a blocked or working agent in another
//!   workspace surfaces here in full fidelity. Sessions with no published state
//!   fall back to a coarse "agents present" marker derived from their pane
//!   commands.
//! - **agents** — one row per agent pane *in the current session*: a state icon
//!   and a label. The icon alone carries the state (color + glyph), so there is
//!   no status word.
//!
//! The two sections are stacked vertically: the workspaces overview fills the
//! top half and the agents section is pinned at the vertical midpoint, so the
//! split stays put as sessions come and go. Each half scrolls independently.
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
use zellij_tile::prelude::{
    AgentRunState, FlockSidebarMode, PaletteColor, PaneAgentStatus, PaneId, PaneManifest,
    SessionInfo, TabInfo,
};

use crate::detect::{identify_agent_from_command, AgentState};
use crate::palette::{bg, fg, goto, Theme, BOLD, DIM, NORMAL_INTENSITY, RESET};
use crate::state::PaneAgentState;

// Braille spinner frames — smooth rotation. Ported verbatim from herdr's
// `ui.rs`. The plugin advances `spinner_tick` once per animation timer fire
// (~8/sec) so it indexes the frames directly rather than herdr's /8 at 60fps.
const SPINNERS: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Pane width (columns) below which the sidebar renders as a clean icon-only
/// rail instead of the full text layout. A slim docked strip lands here.
const THIN_WIDTH: usize = 16;

/// Blank rows kept above and below the sidebar content (both the thin/mini rail
/// and the full labeled view), so it gets a little breathing room from the
/// pane's top and bottom edges and the two views line up.
const RAIL_VPAD: usize = 1;

/// Blank columns kept to the right of the mini rail's divider, so the divider
/// doesn't sit flush against the content pane beside it. With the slim rail at 5
/// cols this leaves a centered dot, a gap, the divider, then this padding.
const RAIL_HPAD: usize = 1;

/// The user's requested sidebar presentation, shared by every rendered section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidebarMode {
    Open,
    Closed,
}

impl Default for SidebarMode {
    fn default() -> Self {
        Self::Open
    }
}

impl SidebarMode {
    pub fn toggled(self) -> Self {
        match self {
            Self::Open => Self::Closed,
            Self::Closed => Self::Open,
        }
    }

    pub fn is_open(self) -> bool {
        matches!(self, Self::Open)
    }
}

impl From<FlockSidebarMode> for SidebarMode {
    fn from(mode: FlockSidebarMode) -> Self {
        match mode {
            FlockSidebarMode::Open => Self::Open,
            FlockSidebarMode::Closed => Self::Closed,
        }
    }
}

impl From<SidebarMode> for FlockSidebarMode {
    fn from(mode: SidebarMode) -> Self {
        match mode {
            SidebarMode::Open => Self::Open,
            SidebarMode::Closed => Self::Closed,
        }
    }
}

/// Map the animation tick to a spinner frame.
pub fn spinner_frame(tick: u32) -> &'static str {
    SPINNERS[(tick as usize) % SPINNERS.len()]
}

/// Per-session activity for the sessions-overview dot. This rolls the session's
/// agents up to the single most attention-worthy one, following herdr's
/// `pane_attention_priority`: Blocked > Done-unseen > Working > Idle(stopped) >
/// none. Ordered by ascending priority so the highest discriminant wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum SessionActivity {
    /// No agents in the session.
    None,
    /// One or more agents present, all idle and already seen — nothing to do.
    Stopped,
    /// At least one agent is actively working.
    Running,
    /// At least one agent finished in the background and hasn't been looked at
    /// yet (and none is blocked) — worth a glance.
    DoneUnseen,
    /// At least one agent is blocked waiting on the user — the most
    /// attention-worthy state, so it wins over everything else.
    Blocked,
}

/// Roll a set of `(state, seen)` agent signals into the single session dot
/// bucket by herdr's attention priority. Empty input ⇒ [`SessionActivity::None`].
fn rollup_activity(agents: impl Iterator<Item = (AgentState, bool)>) -> SessionActivity {
    let mut activity = SessionActivity::None;
    for (state, seen) in agents {
        let this = match state {
            AgentState::Blocked => SessionActivity::Blocked,
            AgentState::Working => SessionActivity::Running,
            AgentState::Idle if !seen => SessionActivity::DoneUnseen,
            // Idle-seen or Unknown: an agent is present but needs no attention.
            _ => SessionActivity::Stopped,
        };
        activity = activity.max(this);
    }
    activity
}

/// The current session's activity, from its live per-pane state.
fn current_session_activity(agents: &BTreeMap<PaneId, PaneAgentState>) -> SessionActivity {
    rollup_activity(
        agents
            .values()
            .filter(|st| st.is_agent())
            .map(|st| (st.state, st.seen)),
    )
}

/// A session's overview-dot activity: the live per-pane state for our own
/// session (fresher than what we publish), else the cross-session published
/// state, falling back to a coarse "agents present" marker from pane commands.
fn session_activity(
    session: &SessionInfo,
    agents: &BTreeMap<PaneId, PaneAgentState>,
) -> SessionActivity {
    if session.is_current_session {
        current_session_activity(agents)
    } else {
        let activity = session_activity_from_states(&session.agent_states);
        if activity == SessionActivity::None && session_agent_count(session) > 0 {
            SessionActivity::Stopped
        } else {
            activity
        }
    }
}

/// The dot glyph + color for a session's activity. Blocked is the red ◉ that
/// also marks a blocked agent in the detail list, so a session waiting on the
/// user stands out at a glance; done-unseen is teal, running green, idle yellow,
/// nothing a dim dot.
fn activity_dot(activity: SessionActivity, p: &Theme) -> (&'static str, PaletteColor) {
    match activity {
        SessionActivity::Blocked => ("◉", p.red),
        SessionActivity::DoneUnseen => ("●", p.teal),
        SessionActivity::Running => ("●", p.green),
        SessionActivity::Stopped => ("●", p.yellow),
        SessionActivity::None => ("○", p.muted),
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

/// Roll another session's published per-pane agent state (the Phase 7
/// cross-session bus, carried on `SessionInfo.agent_states`) into the session
/// dot, using the same attention priority as our own session — so a *blocked*
/// agent in another workspace shows its red ◉ here, not a generic "stopped" dot.
fn session_activity_from_states(states: &BTreeMap<PaneId, PaneAgentStatus>) -> SessionActivity {
    rollup_activity(
        states
            .values()
            .map(|status| (run_state_to_agent_state(status.state), status.seen)),
    )
}

/// Map the serializable cross-session [`AgentRunState`] back to the detector's
/// [`AgentState`] so both rollup paths share one priority function.
fn run_state_to_agent_state(state: AgentRunState) -> AgentState {
    match state {
        AgentRunState::Idle => AgentState::Idle,
        AgentRunState::Working => AgentState::Working,
        AgentRunState::Blocked => AgentState::Blocked,
        AgentRunState::Unknown => AgentState::Unknown,
    }
}

/// Count the panes in another session that look like agents, from their command
/// metadata alone. Used as a fallback for sessions whose flock-sidebar isn't
/// running (so they publish no `agent_states`): the running command is still
/// enough to know an agent is present, even without live state.
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
    /// Whether the user has looked at this pane since it last finished in the
    /// background. A Done pane that hasn't been seen renders with the teal
    /// "done-unseen" icon until focused.
    pub seen: bool,
    /// Whether this is the focused pane in the focused tab.
    pub is_active: bool,
    /// Whether this agent lives in the *current* session. A current-session agent
    /// can be focused directly; an agent in another session is reached by
    /// switching to that session first (its pane isn't focusable from here).
    pub is_current: bool,
    /// The name of the session this agent belongs to (the switch target for a
    /// non-current agent).
    pub session_name: String,
}

/// One entry in the unified sidebar list: a workspace (session) header, or an
/// agent that belongs to the session listed above it. The list interleaves each
/// session with its own agents so every agent is visible regardless of which
/// session is currently focused.
pub(crate) enum Row {
    Session {
        name: String,
        activity: SessionActivity,
        is_current: bool,
    },
    Agent(AgentEntry),
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

/// Build the agent list for the *current* session from its live panes, in tab
/// then pane order, one entry per pane that detection has tagged as an agent.
pub fn build_entries(
    panes: &PaneManifest,
    tabs: &[TabInfo],
    agents: &BTreeMap<PaneId, PaneAgentState>,
    session_name: &str,
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
            let agent_label = st
                .effective_agent_label()
                .unwrap_or_else(|| "?".to_string());
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
                is_current: true,
                session_name: session_name.to_string(),
            });
        }
    }
    entries
}

/// The unified, ordered sidebar list: every session (in [`ordered_sessions`]
/// order) as a dot-only overview row, followed by the *current* session's own
/// agent rows. The two runs map onto the two stacked render sections — sessions
/// up top, agents at the midpoint — while staying a single flat list so a
/// selection index maps consistently across the full view, the rail, keypresses
/// and clicks. [`render`], [`render_thin`] and [`navigable_targets`] all derive
/// from this.
pub(crate) fn build_rows(
    panes: &PaneManifest,
    tabs: &[TabInfo],
    agents: &BTreeMap<PaneId, PaneAgentState>,
    sessions: &[SessionInfo],
) -> Vec<Row> {
    let mut rows = Vec::new();
    // Top section: the workspaces overview — one dot-only row per session, no
    // per-agent rows. Cross-session agent detail is carried entirely by each
    // session's rollup dot.
    for session in ordered_sessions(sessions) {
        rows.push(Row::Session {
            name: session.name.clone(),
            activity: session_activity(session, agents),
            is_current: session.is_current_session,
        });
    }
    // Bottom section: the current session's own agents, one row each. Only the
    // current session's panes are observable from here, so this is the live
    // detail view for the workspace you're in.
    if let Some(current) = sessions.iter().find(|s| s.is_current_session) {
        let entries = build_entries(panes, tabs, agents, &current.name);
        rows.extend(entries.into_iter().map(Row::Agent));
    }
    rows
}

/// The activation target for a row: switch to a session, focus a current-session
/// agent pane, or switch to the session owning a non-current agent.
fn row_target(row: &Row) -> Target {
    match row {
        Row::Session { name, .. } => Target::Session(name.clone()),
        Row::Agent(entry) => {
            if entry.is_current {
                Target::Pane(entry.pane_id)
            } else {
                Target::Session(entry.session_name.clone())
            }
        },
    }
}

/// Sessions in a stable display order: one row per session (each session is its
/// own workspace). Ordered by `workspace_root` path so the layout is stable
/// frame to frame — sessions sharing a path keep their original order, and those
/// whose server reported no workspace root (empty path) sort last.
pub fn ordered_sessions(sessions: &[SessionInfo]) -> Vec<&SessionInfo> {
    let mut ordered: Vec<&SessionInfo> = sessions.iter().collect();
    // sort_by is stable, so equal keys preserve the original list order.
    ordered.sort_by(|a, b| {
        let ka = a.workspace_root.display().to_string();
        let kb = b.workspace_root.display().to_string();
        ka.is_empty().cmp(&kb.is_empty()).then(ka.cmp(&kb))
    });
    ordered
}

/// The ordered list of navigable targets, one per [`build_rows`] entry (each
/// session followed by its agents). The same ordering drives [`render`] and
/// [`render_thin`], so a selection index maps consistently whether it came from
/// a keypress or a click.
pub fn navigable_targets(
    panes: &PaneManifest,
    tabs: &[TabInfo],
    agents: &BTreeMap<PaneId, PaneAgentState>,
    sessions: &[SessionInfo],
) -> Vec<Target> {
    build_rows(panes, tabs, agents, sessions)
        .iter()
        .map(row_target)
        .collect()
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
    /// Shared open/closed state for both sidebar sections.
    pub sidebar_mode: SidebarMode,
    /// Whether the sidebar pane is focused. The selection cursor is only drawn
    /// when focused, so an unfocused ambient rail shows status without a cursor.
    pub focused: bool,
    /// Unified selection cursor over sessions-then-agents.
    pub selected: usize,
    /// Scroll offset into the workspaces (sessions) section.
    pub scroll_sessions: usize,
    /// Scroll offset into the agents section.
    pub scroll_agents: usize,
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
    /// Workspaces-section scroll offset after clamping to keep the selection visible.
    pub scroll_sessions: usize,
    /// Agents-section scroll offset after clamping to keep the selection visible.
    pub scroll_agents: usize,
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

    // Clear the pane explicitly by painting every row blank. A bare `\u{1b}[2J`
    // proved unreliable when the pane shrinks (e.g. collapsing from the full
    // labeled view to the thin rail): rows the new frame no longer draws kept
    // their stale content. Blanking the full height up front guarantees a clean
    // canvas regardless of how few rows the frame then draws over it.
    out.push_str("\u{1b}[2J");
    for y in 0..rows {
        render_row(&mut out, 0, y, cols, None, &[]);
    }

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
            scroll_sessions: 0,
            scroll_agents: 0,
            click_map,
        };
    }

    let rows_data = build_rows(input.panes, input.tabs, input.agents, input.sessions);
    let selected = clamp_selection(input.selected, rows_data.len());

    // Closed mode uses the icon rail even if the pane has enough room for labels.
    // A physically narrow pane also falls back to the rail so open mode never
    // tries to draw labels into too few columns.
    if !input.sidebar_mode.is_open() || cols < THIN_WIDTH {
        return render_thin(out, &input, &rows_data, selected);
    }

    let (content_cols, divider_x) = divider_geometry(cols);

    // Match the thin rail's breathing room: keep RAIL_VPAD blank rows above and
    // below the content, so the full view and the rail line up at the same top
    // offset and neither sits flush against the pane edges.
    let top = RAIL_VPAD.min(rows);
    let bottom_limit = rows.saturating_sub(RAIL_VPAD);
    // The agents header sits on the vertical midpoint so the split between the
    // two sections stays put as sessions come and go. On a pane too short to
    // hold both halves it collapses toward the bottom and the bodies empty out.
    let mid = if bottom_limit > top + 1 {
        (rows / 2).clamp(top + 1, bottom_limit)
    } else {
        bottom_limit
    };

    // Every session row sorts ahead of every agent row in `rows_data`, so the
    // leading run of sessions is the top section and the rest are the agents.
    let session_count = rows_data
        .iter()
        .take_while(|row| matches!(row, Row::Session { .. }))
        .count();
    let agent_count = rows_data.len() - session_count;

    // ---- top section: workspaces overview ----
    let sessions_body_start = top + 1;
    let sessions_body_height = mid.saturating_sub(sessions_body_start);
    let sessions_sel = (selected < session_count).then_some(selected);
    let scroll_sessions = keep_visible(
        input.scroll_sessions,
        sessions_sel,
        session_count,
        sessions_body_height,
    );
    if top < bottom_limit {
        render_row(
            &mut out,
            0,
            top,
            content_cols,
            None,
            &[Span::new(" workspaces", p.muted).bold()],
        );
    }
    render_section(
        &mut out,
        &mut click_map,
        SectionInput {
            rows: &rows_data[..session_count],
            index_offset: 0,
            body_start: sessions_body_start,
            body_height: sessions_body_height,
            scroll: scroll_sessions,
            selected,
            focused: input.focused,
            spinner_tick: input.spinner_tick,
            cols: content_cols,
            p,
        },
    );

    // ---- bottom section: current session's agents ----
    let agents_body_start = mid + 1;
    let agents_body_height = bottom_limit.saturating_sub(agents_body_start);
    let agents_sel = (selected >= session_count).then(|| selected - session_count);
    let scroll_agents = keep_visible(
        input.scroll_agents,
        agents_sel,
        agent_count,
        agents_body_height,
    );
    if mid < bottom_limit {
        render_row(
            &mut out,
            0,
            mid,
            content_cols,
            None,
            &[Span::new(" agents", p.muted).bold()],
        );
    }
    render_section(
        &mut out,
        &mut click_map,
        SectionInput {
            rows: &rows_data[session_count..],
            index_offset: session_count,
            body_start: agents_body_start,
            body_height: agents_body_height,
            scroll: scroll_agents,
            selected,
            focused: input.focused,
            spinner_tick: input.spinner_tick,
            cols: content_cols,
            p,
        },
    );
    render_divider(&mut out, divider_x, rows, p);

    RenderOutput {
        ansi: out,
        selected,
        scroll_sessions,
        scroll_agents,
        click_map,
    }
}

/// Clamp a section's scroll offset, then — if its selection cursor lives in this
/// section — nudge the offset so the selected row stays within the visible
/// window. `selected` is the row's index *within the section* (None when the
/// cursor is in the other section, so only the clamp applies).
fn keep_visible(scroll: usize, selected: Option<usize>, total: usize, visible: usize) -> usize {
    let mut scroll = scroll.min(total.saturating_sub(visible));
    if let Some(sel) = selected {
        if sel < scroll {
            scroll = sel;
        } else if visible > 0 && sel >= scroll + visible {
            scroll = sel + 1 - visible;
        }
    }
    scroll
}

/// Inputs to [`render_section`] — one stacked section's slice of rows and the
/// geometry it draws into.
struct SectionInput<'a> {
    /// This section's rows (a contiguous slice of `rows_data`).
    rows: &'a [Row],
    /// Flat selection index of `rows[0]`: 0 for the sessions section, the
    /// session count for the agents section. Added to each row's local index so
    /// the cursor and click map line up with [`navigable_targets`].
    index_offset: usize,
    body_start: usize,
    body_height: usize,
    scroll: usize,
    /// The global selection cursor (already clamped).
    selected: usize,
    focused: bool,
    spinner_tick: u32,
    cols: usize,
    p: &'a Theme,
}

/// Render one stacked section's body: its rows from `scroll` down, an empty
/// `(none)` line when it has none, and a scrollbar when they overflow.
fn render_section(out: &mut String, click_map: &mut Vec<ClickTarget>, s: SectionInput) {
    let p = s.p;
    let total = s.rows.len();
    let body_bottom = s.body_start + s.body_height;
    let scrollbar = total > s.body_height && s.body_height > 0;
    let content_width = s.cols.saturating_sub(usize::from(scrollbar));

    if total == 0 {
        if s.body_start < body_bottom {
            render_row(
                out,
                0,
                s.body_start,
                content_width,
                None,
                &[Span::new(" (none)", p.muted).dim()],
            );
        }
        return;
    }

    let mut row_y = s.body_start;
    for (local, row) in s.rows.iter().enumerate().skip(s.scroll) {
        if row_y >= body_bottom {
            break;
        }
        let index = s.index_offset + local;
        // The cursor only shows while the sidebar is focused.
        let cursor = index == s.selected && s.focused;
        draw_row(out, row, row_y, content_width, cursor, s.spinner_tick, p);
        click_map.push(ClickTarget { row: row_y, index });
        row_y += 1;
    }

    if scrollbar {
        render_scrollbar(
            out,
            s.cols.saturating_sub(1),
            s.body_start,
            s.body_height,
            total,
            s.body_height,
            s.scroll,
            p,
        );
    }
}

/// Draw one sidebar row — a session overview dot or an agent state icon — at
/// `row_y`. A current session and the focused agent pane stay emphasized even
/// without the cursor; the selected row also gets the selection background.
fn draw_row(
    out: &mut String,
    row: &Row,
    row_y: usize,
    content_width: usize,
    cursor: bool,
    spinner_tick: u32,
    p: &Theme,
) {
    let row_bg = cursor.then_some(p.selection_bg);
    match row {
        Row::Session {
            name,
            activity,
            is_current,
        } => {
            let (dot, dot_color) = activity_dot(*activity, p);
            let emphasized = cursor || *is_current;
            let name_color = if emphasized { p.text } else { p.muted };
            let label = truncate_text(name, content_width.saturating_sub(3));
            let mut name_span = Span::new(label, name_color);
            name_span = if emphasized {
                name_span.bold()
            } else {
                name_span.dim()
            };
            render_row(
                out,
                0,
                row_y,
                content_width,
                row_bg,
                &[
                    Span::new(" ", p.text),
                    Span::new(dot, dot_color),
                    Span::new(" ", p.text),
                    name_span,
                ],
            );
        },
        Row::Agent(entry) => {
            let (icon, icon_color) = agent_icon(entry.state, entry.seen, spinner_tick, p);
            let emphasized = cursor || entry.is_active;
            let name_color = if emphasized { p.text } else { p.muted };
            // Agent icons align under the session dots (their own section now,
            // not nested), so they share the session row's one-space indent.
            let label = truncate_text(&entry.label, content_width.saturating_sub(3));
            let mut name_span = Span::new(label, name_color);
            name_span = if emphasized {
                name_span.bold()
            } else {
                name_span.dim()
            };
            render_row(
                out,
                0,
                row_y,
                content_width,
                row_bg,
                &[
                    Span::new(" ", p.text),
                    Span::new(icon, icon_color),
                    Span::new(" ", p.text),
                    name_span,
                ],
            );
        },
    }
}

/// Render the compact icon rail used when the pane is too narrow for labels: a
/// centered vertical column of session activity dots. Agent detail stays in the
/// expanded view; mini mode is a workspace overview only.
fn render_thin(
    mut out: String,
    input: &RenderInput,
    rows_data: &[Row],
    selected: usize,
) -> RenderOutput {
    let p = input.palette;
    let cols = input.cols;
    let rows = input.rows;
    let mut click_map = Vec::new();

    // Lay the rail out as: glyph | divider | right padding. The divider sits
    // `RAIL_HPAD` columns in from the right edge so it gets a little breathing
    // room from the content pane rather than butting against it; the glyph lives
    // in the columns to its left.
    let (rail_width, divider_x) = divider_geometry(cols);

    // One glyph per session row. Sessions are the leading run in `rows_data`, so
    // their local indices are still the same selection indices used by targets.
    let session_count = rows_data
        .iter()
        .take_while(|row| matches!(row, Row::Session { .. }))
        .count();
    let selected = clamp_selection(selected, session_count);
    let glyphs: Vec<(&'static str, PaletteColor)> = rows_data
        .iter()
        .take(session_count)
        .map(|row| match row {
            Row::Session { activity, .. } => activity_dot(*activity, p),
            Row::Agent(_) => unreachable!("agent rows are outside the sessions prefix"),
        })
        .collect();

    // Keep a little breathing room above and below the glyphs so they don't sit
    // flush against the pane's top and bottom edges.
    let top = RAIL_VPAD.min(rows);
    let body_height = rows.saturating_sub(RAIL_VPAD * 2);

    // Scroll the rail so the selected session glyph stays within the padded body.
    let mut scroll = input
        .scroll_sessions
        .min(glyphs.len().saturating_sub(body_height.max(1)));
    if selected < scroll {
        scroll = selected;
    } else if body_height > 0 && selected >= scroll + body_height {
        scroll = selected + 1 - body_height;
    }

    // Center the single glyph within the rail (the columns left of the divider).
    let pad = rail_width.saturating_sub(1) / 2;
    for (index, &(glyph, color)) in glyphs.iter().enumerate().skip(scroll).take(body_height) {
        let y = top + (index - scroll);
        let cursor = index == selected && input.focused;
        let row_bg = cursor.then_some(p.selection_bg);
        let mut glyph_span = Span::new(glyph, color);
        if cursor {
            glyph_span = glyph_span.bold();
        }
        let mut spans = Vec::new();
        if pad > 0 {
            spans.push(Span::new(" ".repeat(pad), p.text));
        }
        spans.push(glyph_span);
        render_row(&mut out, 0, y, rail_width, row_bg, &spans);
        click_map.push(ClickTarget { row: y, index });
    }

    render_divider(&mut out, divider_x, rows, p);

    RenderOutput {
        ansi: out,
        selected,
        scroll_sessions: scroll,
        scroll_agents: input.scroll_agents,
        click_map,
    }
}

fn divider_geometry(cols: usize) -> (usize, usize) {
    let divider_x = cols.saturating_sub(1 + RAIL_HPAD);
    (divider_x.max(1), divider_x)
}

/// Draw a continuous vertical divider down the right edge, inset by
/// [`RAIL_HPAD`] so it has breathing room from the content pane beside it.
fn render_divider(out: &mut String, divider_x: usize, rows: usize, p: &Theme) {
    if divider_x >= 1 {
        for y in 0..rows {
            render_row(
                out,
                divider_x,
                y,
                1,
                None,
                &[Span::new("│", p.separator).dim()],
            );
        }
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
        let is_thumb = i >= thumb_top && i < thumb_top + thumb_len;
        let (symbol, color) = if is_thumb {
            ("▐", p.accent)
        } else {
            ("▕", p.separator)
        };
        let mut span = Span::new(symbol, color);
        if !is_thumb {
            // The track stays subtle; only the thumb is full-strength accent.
            span = span.dim();
        }
        render_row(out, x, body_start + i, 1, None, &[span]);
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
    fn sidebar_mode_toggles_between_open_and_closed() {
        assert_eq!(SidebarMode::default(), SidebarMode::Open);
        assert_eq!(SidebarMode::Open.toggled(), SidebarMode::Closed);
        assert_eq!(SidebarMode::Closed.toggled(), SidebarMode::Open);
    }

    fn sess(name: &str, root: &str) -> SessionInfo {
        let mut s = SessionInfo::new(name.to_string());
        s.workspace_root = std::path::PathBuf::from(root);
        s
    }

    #[test]
    fn ordered_sessions_sort_by_workspace_root_with_unknown_last() {
        let sessions = vec![
            sess("a", "/home/u/proj"),
            sess("b", "/home/u/proj"),
            sess("c", ""),
            sess("d", "/home/u/other"),
        ];
        let ordered = ordered_sessions(&sessions);
        // Non-empty paths sort lexically; same-path keeps original order; the
        // unknown (empty) workspace trails.
        let names: Vec<&str> = ordered.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["d", "a", "b", "c"]);
    }

    #[test]
    fn closed_sidebar_mode_uses_rail_even_when_wide() {
        let panes = PaneManifest::default();
        let tabs = Vec::new();
        let agents = BTreeMap::new();
        let sessions = vec![sess("workspace-a", "/home/u/proj")];
        let palette = Theme::default();

        let output = render(RenderInput {
            permissions_granted: true,
            panes: &panes,
            tabs: &tabs,
            agents: &agents,
            sessions: &sessions,
            palette: &palette,
            sidebar_mode: SidebarMode::Closed,
            focused: false,
            selected: 0,
            scroll_sessions: 0,
            scroll_agents: 0,
            spinner_tick: 0,
            rows: 8,
            cols: 40,
        });

        assert!(!output.ansi.contains("workspaces"));
        assert!(!output.ansi.contains("workspace-a"));
        assert!(output.click_map.iter().any(|hit| hit.index == 0));
    }

    #[test]
    fn open_sidebar_mode_draws_divider() {
        let panes = PaneManifest::default();
        let tabs = Vec::new();
        let agents = BTreeMap::new();
        let sessions = vec![sess("workspace-a", "/home/u/proj")];
        let palette = Theme::default();

        let output = render(RenderInput {
            permissions_granted: true,
            panes: &panes,
            tabs: &tabs,
            agents: &agents,
            sessions: &sessions,
            palette: &palette,
            sidebar_mode: SidebarMode::Open,
            focused: false,
            selected: 0,
            scroll_sessions: 0,
            scroll_agents: 0,
            spinner_tick: 0,
            rows: 8,
            cols: 40,
        });

        assert!(output.ansi.contains("│"));
    }

    #[test]
    fn closed_sidebar_mode_renders_only_session_indicators() {
        use crate::detect::Agent;

        let panes = PaneManifest {
            panes: std::collections::HashMap::from([(
                0,
                vec![zellij_tile::prelude::PaneInfo {
                    id: 7,
                    is_plugin: false,
                    ..Default::default()
                }],
            )]),
        };
        let tabs = vec![TabInfo {
            position: 0,
            active: true,
            ..Default::default()
        }];
        let mut agents = BTreeMap::new();
        agents.insert(
            PaneId::Terminal(7),
            agent_pane(Agent::Codex, AgentState::Working, true),
        );
        let mut current_session = sess("workspace-a", "/home/u/proj");
        current_session.is_current_session = true;
        let sessions = vec![current_session];
        let palette = Theme::default();

        let output = render(RenderInput {
            permissions_granted: true,
            panes: &panes,
            tabs: &tabs,
            agents: &agents,
            sessions: &sessions,
            palette: &palette,
            sidebar_mode: SidebarMode::Closed,
            focused: false,
            selected: 1,
            scroll_sessions: 0,
            scroll_agents: 0,
            spinner_tick: 0,
            rows: 8,
            cols: 40,
        });

        assert_eq!(output.selected, 0);
        assert_eq!(output.click_map.len(), 1);
        assert_eq!(output.click_map[0].index, 0);
    }

    #[test]
    fn navigable_targets_follow_grouped_session_order() {
        let sessions = vec![
            sess("a", "/home/u/proj"),
            sess("c", ""),
            sess("d", "/home/u/other"),
        ];
        let targets = navigable_targets(&PaneManifest::default(), &[], &BTreeMap::new(), &sessions);
        assert_eq!(
            targets,
            vec![
                Target::Session("d".to_string()), // /home/u/other
                Target::Session("a".to_string()), // /home/u/proj
                Target::Session("c".to_string()), // unknown, last
            ]
        );
    }

    fn agent_pane(agent: crate::detect::Agent, state: AgentState, seen: bool) -> PaneAgentState {
        let mut pane = PaneAgentState::new();
        pane.detected_agent = Some(agent);
        pane.state = state;
        pane.seen = seen;
        pane
    }

    #[test]
    fn current_session_activity_rolls_up_by_attention_priority() {
        use crate::detect::Agent;
        let mut agents: BTreeMap<PaneId, PaneAgentState> = BTreeMap::new();

        // No agents → None.
        assert_eq!(current_session_activity(&agents), SessionActivity::None);

        // A single idle, seen agent → Stopped (present, nothing to do).
        agents.insert(
            PaneId::Terminal(1),
            agent_pane(Agent::Codex, AgentState::Idle, true),
        );
        assert_eq!(current_session_activity(&agents), SessionActivity::Stopped);

        // Add a working agent → Running outranks idle.
        agents.insert(
            PaneId::Terminal(2),
            agent_pane(Agent::Claude, AgentState::Working, true),
        );
        assert_eq!(current_session_activity(&agents), SessionActivity::Running);

        // Add an unseen completion → Done-unseen outranks working.
        agents.insert(
            PaneId::Terminal(3),
            agent_pane(Agent::Pi, AgentState::Idle, false),
        );
        assert_eq!(
            current_session_activity(&agents),
            SessionActivity::DoneUnseen
        );

        // Add a blocked agent → Blocked wins over everything.
        agents.insert(
            PaneId::Terminal(4),
            agent_pane(Agent::Codex, AgentState::Blocked, true),
        );
        assert_eq!(current_session_activity(&agents), SessionActivity::Blocked);
    }

    fn status(state: AgentRunState, seen: bool) -> PaneAgentStatus {
        PaneAgentStatus {
            state,
            label: "agent".to_owned(),
            seen,
        }
    }

    #[test]
    fn session_activity_from_states_buckets_cross_session_state() {
        let mut states: BTreeMap<PaneId, PaneAgentStatus> = BTreeMap::new();

        // No published agents → None.
        assert_eq!(session_activity_from_states(&states), SessionActivity::None);

        // An idle, seen agent → Stopped.
        states.insert(PaneId::Terminal(1), status(AgentRunState::Idle, true));
        assert_eq!(
            session_activity_from_states(&states),
            SessionActivity::Stopped
        );

        // A working agent in another session → Running (detectable now that the
        // state crosses the bus).
        states.insert(PaneId::Terminal(2), status(AgentRunState::Working, true));
        assert_eq!(
            session_activity_from_states(&states),
            SessionActivity::Running
        );

        // A blocked agent in another session → Blocked wins, so a workspace
        // waiting on the user shows its red ◉ here. This is the cross-session
        // win the richer rollup unlocks.
        states.insert(PaneId::Terminal(3), status(AgentRunState::Blocked, false));
        assert_eq!(
            session_activity_from_states(&states),
            SessionActivity::Blocked
        );
    }

    #[test]
    fn blocked_session_gets_a_distinct_red_dot() {
        let p = Theme::default();
        let (blocked_icon, blocked_color) = activity_dot(SessionActivity::Blocked, &p);
        let (idle_icon, idle_color) = activity_dot(SessionActivity::Stopped, &p);
        // Blocked is visually distinct from a merely-stopped session.
        assert_eq!(blocked_icon, "◉");
        assert_eq!(blocked_color, p.red);
        assert_ne!((blocked_icon, blocked_color), (idle_icon, idle_color));
    }
}
