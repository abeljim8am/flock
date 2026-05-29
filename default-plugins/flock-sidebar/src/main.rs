//! flock-sidebar — an agent-aware sidebar plugin for Zellij.
//!
//! Phase 2 adds agent detection for the plugin's own session: it identifies
//! which panes run AI coding agents (from their `CommandChanged` argv) and
//! classifies each one's live state (Idle / Working / Blocked) by matching the
//! pane's on-screen chrome via the ported herdr detectors. The herdr async
//! polling loop becomes event-driven — `PaneRenderReportWithAnsi` pushes screen
//! content, `CommandChanged` pushes the running command, and a recurring `Timer`
//! drives the Claude working-hold / stale-hook grace windows.
//!
//! The sidebar render is still a debug list; the herdr-fidelity UI is Phase 3.

mod detect;
mod state;

use std::collections::{BTreeMap, HashSet};
use std::time::Instant;

use detect::{detect_agent, identify_agent_from_command, AgentState};
use state::PaneAgentState;
use zellij_tile::prelude::*;

/// How often we re-evaluate time-based holds/grace windows. herdr polled every
/// 300ms; we only need a tick frequent enough to expire the 1.2s Claude hold and
/// the 2s stale-hook window without a new render report.
const STATE_TICK_SECS: f64 = 0.5;

#[derive(Default)]
struct State {
    /// Whether our permission request has been granted yet. Until it is, we
    /// can't read pane contents / application state, so we render a hint.
    permissions_granted: bool,
    /// Latest pane manifest for our own session.
    panes: PaneManifest,
    /// Latest tab list for our own session.
    tabs: Vec<TabInfo>,
    /// Latest cross-session list (used for workspace grouping in later phases).
    sessions: Vec<SessionInfo>,
    /// Per-pane agent detection + arbitrated state, keyed by pane id.
    agents: BTreeMap<PaneId, PaneAgentState>,
    /// Whether the recurring state tick timer has been armed.
    timer_running: bool,
    /// Plugin pane dimensions from the last render, for mouse hit-testing later.
    rows: usize,
    cols: usize,
}

register_plugin!(State);

impl ZellijPlugin for State {
    fn load(&mut self, _configuration: BTreeMap<String, String>) {
        // Permissions needed across all phases:
        // - ReadApplicationState: pane/tab/session manifests
        // - ReadPaneContents: PaneRenderReportWithAnsi screen scraping (Phase 2)
        // - ReadCliPipes: agent hook reports via `zellij pipe` (Phase 5)
        // - RunCommands: git branch / ahead-behind (Phase 6)
        request_permission(&[
            PermissionType::ReadApplicationState,
            PermissionType::ReadPaneContents,
            PermissionType::ReadCliPipes,
            PermissionType::RunCommands,
        ]);

        subscribe(&[
            EventType::PaneUpdate,
            EventType::TabUpdate,
            EventType::SessionUpdate,
            EventType::CommandChanged,
            EventType::PaneRenderReportWithAnsi,
            EventType::Mouse,
            EventType::Key,
            EventType::PermissionRequestResult,
            EventType::Visible,
            EventType::Timer,
        ]);

        // Drive the time-based stabilization windows. Re-armed on each Timer.
        set_timeout(STATE_TICK_SECS);
        self.timer_running = true;
    }

    fn update(&mut self, event: Event) -> bool {
        let mut should_render = false;
        match event {
            Event::PermissionRequestResult(result) => {
                self.permissions_granted = matches!(result, PermissionStatus::Granted);
                should_render = true;
            },
            Event::PaneUpdate(manifest) => {
                self.panes = manifest;
                // Drop tracked state for panes that no longer exist.
                self.prune_closed_panes();
                should_render = true;
            },
            Event::TabUpdate(tabs) => {
                self.tabs = tabs;
                should_render = true;
            },
            Event::SessionUpdate(sessions, _resurrectable) => {
                self.sessions = sessions;
                should_render = true;
            },
            Event::CommandChanged(pane_id, command, is_foreground, _focused_clients) => {
                // The foreground command is the program actually running in the
                // pane; only it determines the agent. A background change (e.g. a
                // job control bump) shouldn't reassign the pane's agent.
                if is_foreground {
                    let agent = identify_agent_from_command(&command);
                    let entry = self.agents.entry(pane_id).or_default();
                    if entry.set_detected_agent(agent, Instant::now()) {
                        should_render = true;
                    }
                }
            },
            Event::PaneRenderReportWithAnsi(pane_contents) => {
                let now = Instant::now();
                for (pane_id, contents) in pane_contents {
                    let screen = screen_text(&contents);
                    let entry = self.agents.entry(pane_id).or_default();
                    let agent = entry.detected_agent;
                    let detection = detect_agent(agent, &screen);
                    if entry.observe_screen(agent, detection, now) {
                        should_render = true;
                    }
                }
            },
            Event::Timer(_) => {
                let now = Instant::now();
                for entry in self.agents.values_mut() {
                    if entry.tick(now) {
                        should_render = true;
                    }
                }
                // Re-arm the recurring tick.
                set_timeout(STATE_TICK_SECS);
            },
            // Subscribed now, handled in later phases.
            Event::Mouse(_) => {},
            Event::Visible(_) => {},
            Event::Key(key) => {
                // Esc closes the sidebar when it's focused (e.g. a floating pane).
                if key.bare_key == BareKey::Esc && key.has_no_modifiers() {
                    close_self();
                }
            },
            _ => {},
        }
        should_render
    }

    fn render(&mut self, rows: usize, cols: usize) {
        self.rows = rows;
        self.cols = cols;

        let title = Text::new("🐑 flock").color_range(2, ..);
        print_text_with_coordinates(title, 0, 0, Some(cols), None);

        if !self.permissions_granted {
            let hint = Text::new("waiting for permissions…").color_range(3, ..);
            print_text_with_coordinates(hint, 0, 2, Some(cols), None);
            return;
        }

        // Build a pane-id → title lookup for labelling agent rows.
        let mut titles: BTreeMap<PaneId, String> = BTreeMap::new();
        for panes in self.panes.panes.values() {
            for pane in panes {
                let pane_id = if pane.is_plugin {
                    PaneId::Plugin(pane.id)
                } else {
                    PaneId::Terminal(pane.id)
                };
                titles.insert(pane_id, pane.title.clone());
            }
        }

        let agents: Vec<(&PaneId, &PaneAgentState)> = self
            .agents
            .iter()
            .filter(|(_, st)| st.is_agent())
            .collect();

        let summary = format!("{} agent(s)", agents.len());
        print_text_with_coordinates(Text::new(summary), 0, 2, Some(cols), None);

        if agents.is_empty() {
            let placeholder = Text::new("no agents detected yet").color_range(0, ..);
            print_text_with_coordinates(placeholder, 0, 4, Some(cols), None);
            return;
        }

        let mut row = 4;
        for (pane_id, st) in agents {
            let label = st.effective_agent_label().unwrap_or_else(|| "?".to_string());
            let (glyph, color) = state_glyph(st.state);
            let title = titles
                .get(pane_id)
                .map(|t| t.as_str())
                .unwrap_or("")
                .trim();
            let line = if title.is_empty() {
                format!("{glyph} {label}  {}", state_word(st.state))
            } else {
                format!("{glyph} {label}  {}  {title}", state_word(st.state))
            };
            // Color just the leading glyph by the state.
            let text = Text::new(line).color_range(color, 0..glyph.chars().count());
            print_text_with_coordinates(text, 0, row, Some(cols), None);
            row += 1;
        }
    }
}

impl State {
    /// Remove tracked agent state for panes that are no longer in the manifest.
    fn prune_closed_panes(&mut self) {
        let live: HashSet<PaneId> = self
            .panes
            .panes
            .values()
            .flatten()
            .map(|pane| {
                if pane.is_plugin {
                    PaneId::Plugin(pane.id)
                } else {
                    PaneId::Terminal(pane.id)
                }
            })
            .collect();
        self.agents.retain(|pane_id, _| live.contains(pane_id));
    }
}

/// Flatten a pane's viewport into a single screen-text snapshot for detection.
///
/// `PaneRenderReportWithAnsi` lines carry SGR/CSI escape sequences; herdr's
/// detectors are written for the rendered plain text (they inspect the first
/// glyph of a line, match literal chrome strings, etc.), so strip the escapes
/// first while preserving the visible glyphs and spacing.
fn screen_text(contents: &PaneContents) -> String {
    let mut out = String::new();
    for line in &contents.viewport {
        strip_ansi_into(line, &mut out);
        out.push('\n');
    }
    out
}

/// Append `line` to `out` with ANSI escape sequences removed.
fn strip_ansi_into(line: &str, out: &mut String) {
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.peek().copied() {
            // CSI: ESC [ ... <final byte 0x40–0x7E>
            Some('[') => {
                chars.next();
                for p in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&p) {
                        break;
                    }
                }
            },
            // OSC: ESC ] ... terminated by BEL or ST (ESC \)
            Some(']') => {
                chars.next();
                while let Some(p) = chars.next() {
                    if p == '\u{07}' {
                        break;
                    }
                    if p == '\u{1b}' {
                        if matches!(chars.peek(), Some('\\')) {
                            chars.next();
                        }
                        break;
                    }
                }
            },
            // Other escape: ESC <single byte>
            Some(_) => {
                chars.next();
            },
            None => {},
        }
    }
}

/// A single-glyph state indicator and its theme color index. Exact herdr colors
/// (raw ANSI red/yellow/blue/green) land with the full UI in Phase 3.
fn state_glyph(state: AgentState) -> (&'static str, usize) {
    match state {
        AgentState::Blocked => ("●", 1),
        AgentState::Working => ("●", 3),
        AgentState::Idle => ("●", 2),
        AgentState::Unknown => ("○", 0),
    }
}

fn state_word(state: AgentState) -> &'static str {
    match state {
        AgentState::Blocked => "blocked",
        AgentState::Working => "working",
        AgentState::Idle => "idle",
        AgentState::Unknown => "unknown",
    }
}
