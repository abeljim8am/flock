//! flock-sidebar — an agent-aware sidebar plugin for Zellij.
//!
//! Phase 2 added agent detection for the plugin's own session: it identifies
//! which panes run AI coding agents (from their `CommandChanged` argv) and
//! classifies each one's live state (Idle / Working / Blocked) by matching the
//! pane's on-screen chrome via the ported herdr detectors. The herdr async
//! polling loop becomes event-driven — `PaneRenderReportWithAnsi` pushes screen
//! content, `CommandChanged` pushes the running command, and a recurring `Timer`
//! drives the Claude working-hold / stale-hook grace windows.
//!
//! Phase 3 renders that detected state as herdr's sidebar, re-targeted from
//! `ratatui` onto the plugin's raw-ANSI output (see [`ui`]): a scrollable list
//! of per-pane agent + state rows with herdr's exact state icons/colors, plus
//! mouse scroll and click-to-focus. The same `Timer` now also advances a spinner
//! for working agents.
//!
//! Phase 4 adds unseen / notification tracking: when an agent pane finishes in
//! the background (a Working/Blocked → Idle transition while it is *not* the
//! focused pane), it shows herdr's Done-unseen icon (teal `●`) until the user
//! focuses it, then reverts to the seen icon (green `✓`). Focus is tracked from
//! `PaneUpdate`/`TabUpdate` (`is_focused` + the active tab) and fed into each
//! pane's seen arbitration — see [`State::sync_focus`] and
//! [`state::PaneAgentState::set_focused`].
//!
//! Phase 5 adds the hook channel: agents report their own state directly through
//! a `zellij pipe --name flock-state` message (requires the `ReadCliPipes`
//! permission), which [`State::pipe`] parses (see [`hook`]) and applies to the
//! target pane as a hook authority. The Phase 2 arbitration already favors a
//! hook report over screen detection — with strong visible signals still able to
//! veto a stale, non-blocked hook — so a self-report overrides screen detection
//! per that precedence. The bundled `assets/*/flock-agent-state.sh` hooks are
//! ported from herdr's, retargeted from its socket onto `zellij pipe`.

mod detect;
mod hook;
mod palette;
mod state;
mod ui;

use std::collections::{BTreeMap, HashSet};
use std::time::Instant;

use detect::{detect_agent, identify_agent_from_command, AgentState};
use hook::{parse_hook_report, HookReport, HOOK_PIPE_NAME};
use palette::Theme;
use state::PaneAgentState;
use ui::{ClickTarget, Target};
use zellij_tile::prelude::*;

/// How often we re-evaluate time-based holds/grace windows when nothing is
/// animating. herdr polled every 300ms; we only need a tick frequent enough to
/// expire the 1.2s Claude hold and the 2s stale-hook window without a new render
/// report.
const STATE_TICK_SECS: f64 = 0.5;
/// Faster cadence used while at least one agent is working, so the spinner
/// animates smoothly (~8 frames/sec).
const SPINNER_TICK_SECS: f64 = 0.12;

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
    /// Sidebar colors, resolved from the user's active zellij theme (updated on
    /// each `ModeUpdate`).
    palette: Theme,
    /// Unified keyboard selection cursor over the sessions then the agents.
    selected: usize,
    /// Scroll offset into the agent list.
    scroll: usize,
    /// Spinner animation frame counter, advanced by the timer while working.
    spinner_tick: u32,
    /// Row → selection-index map from the last render, for mouse hit-testing.
    click_map: Vec<ClickTarget>,
    /// Plugin pane dimensions from the last render, for mouse hit-testing.
    rows: usize,
    cols: usize,
}

register_plugin!(State);

impl ZellijPlugin for State {
    fn load(&mut self, _configuration: BTreeMap<String, String>) {
        // Permissions needed across all phases:
        // - ReadApplicationState: pane/tab/session manifests
        // - ReadPaneContents: PaneRenderReportWithAnsi screen scraping (Phase 2)
        // - ChangeApplicationState: switch session / focus pane on activation
        // - ReadCliPipes: agent hook reports via `zellij pipe` (Phase 5)
        // - RunCommands: git branch / ahead-behind (Phase 6)
        request_permission(&[
            PermissionType::ReadApplicationState,
            PermissionType::ReadPaneContents,
            PermissionType::ChangeApplicationState,
            PermissionType::ReadCliPipes,
            PermissionType::RunCommands,
        ]);

        subscribe(&[
            EventType::ModeUpdate,
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
            Event::ModeUpdate(mode_info) => {
                // Track the active theme so the sidebar's colors follow it.
                self.palette = Theme::from_style(&mode_info.style);
                should_render = true;
            },
            Event::PaneUpdate(manifest) => {
                self.panes = manifest;
                // Drop tracked state for panes that no longer exist.
                self.prune_closed_panes();
                // A focus change here may clear a Done-unseen notification.
                self.sync_focus();
                should_render = true;
            },
            Event::TabUpdate(tabs) => {
                self.tabs = tabs;
                // The active tab feeds which pane counts as "viewed".
                self.sync_focus();
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
                // While anything is working, animate the spinner and tick faster;
                // otherwise fall back to the slow hold/grace cadence.
                let working = self.any_working();
                if working {
                    self.spinner_tick = self.spinner_tick.wrapping_add(1);
                    should_render = true;
                }
                set_timeout(if working {
                    SPINNER_TICK_SECS
                } else {
                    STATE_TICK_SECS
                });
            },
            Event::Mouse(mouse) => match mouse {
                // The wheel moves the keyboard cursor, so scrolling and selection
                // stay in lockstep (the agent list follows the cursor in render).
                Mouse::ScrollUp(n) => {
                    self.select_prev(n.max(1));
                    should_render = true;
                },
                Mouse::ScrollDown(n) => {
                    self.select_next(n.max(1));
                    should_render = true;
                },
                Mouse::LeftClick(line, _) => {
                    if line >= 0 {
                        if let Some(index) = ui::index_at_row(&self.click_map, line as usize) {
                            self.selected = index;
                            self.activate_selected();
                            should_render = true;
                        }
                    }
                },
                _ => {},
            },
            // Reports the *plugin's* own pane visibility, not an agent pane's,
            // so it doesn't bear on seen-tracking (that follows the focused
            // terminal pane via `PaneUpdate`). Kept subscribed for later phases.
            Event::Visible(_) => {},
            Event::Key(key) => {
                if key.has_no_modifiers() {
                    match key.bare_key {
                        // Keyboard-first navigation over sessions + agents.
                        BareKey::Up | BareKey::Char('k') => {
                            self.select_prev(1);
                            should_render = true;
                        },
                        BareKey::Down | BareKey::Char('j') => {
                            self.select_next(1);
                            should_render = true;
                        },
                        BareKey::Enter => {
                            self.activate_selected();
                            should_render = true;
                        },
                        // Esc closes the sidebar when it's focused (e.g. a float).
                        BareKey::Esc => close_self(),
                        _ => {},
                    }
                }
            },
            _ => {},
        }
        should_render
    }

    fn pipe(&mut self, pipe_message: PipeMessage) -> bool {
        // Only the agent self-report channel concerns us; ignore everything else
        // so we don't claim pipes meant for other plugins.
        if pipe_message.name != HOOK_PIPE_NAME {
            return false;
        }
        match parse_hook_report(&pipe_message.args) {
            Ok(report) => self.apply_hook_report(report),
            Err(reason) => {
                // A malformed report is dropped, not applied — log for the
                // operator and leave every pane's state untouched.
                eprintln!("flock-sidebar: ignoring {HOOK_PIPE_NAME} report: {reason}");
                false
            },
        }
    }

    fn render(&mut self, rows: usize, cols: usize) {
        self.rows = rows;
        self.cols = cols;

        let output = ui::render(ui::RenderInput {
            permissions_granted: self.permissions_granted,
            panes: &self.panes,
            tabs: &self.tabs,
            agents: &self.agents,
            sessions: &self.sessions,
            palette: &self.palette,
            selected: self.selected,
            scroll: self.scroll,
            spinner_tick: self.spinner_tick,
            rows,
            cols,
        });
        self.selected = output.selected;
        self.scroll = output.scroll;
        self.click_map = output.click_map;
        print!("{}", output.ansi);
    }
}

impl State {
    /// Whether any tracked agent is currently in the Working state (drives the
    /// faster spinner-animation timer cadence).
    fn any_working(&self) -> bool {
        self.agents
            .values()
            .any(|st| st.is_agent() && st.state == AgentState::Working)
    }

    /// The ordered navigable targets (sessions then agents). Rebuilt on demand;
    /// the same ordering drives the render, so indices line up.
    fn targets(&self) -> Vec<Target> {
        ui::navigable_targets(&self.panes, &self.tabs, &self.agents, &self.sessions)
    }

    /// Move the selection cursor up by `n`, clamped at the top.
    fn select_prev(&mut self, n: usize) {
        self.selected = self.selected.saturating_sub(n);
    }

    /// Move the selection cursor down by `n`, clamped at the last target.
    fn select_next(&mut self, n: usize) {
        let last = self.targets().len().saturating_sub(1);
        self.selected = self.selected.saturating_add(n).min(last);
    }

    /// Act on the selected row: switch to a session, or focus an agent pane.
    fn activate_selected(&mut self) {
        match self.targets().into_iter().nth(self.selected) {
            Some(Target::Session(name)) => switch_session(Some(&name)),
            Some(Target::Pane(PaneId::Terminal(id))) => focus_terminal_pane(id, false, false),
            Some(Target::Pane(PaneId::Plugin(id))) => focus_plugin_pane(id, false, false),
            None => {},
        }
    }

    /// Apply a parsed agent self-report (Phase 5 hook channel) to its target
    /// pane. The pane's [`PaneAgentState`] entry is created on demand — a hook
    /// can arrive before we've seen the pane's command or any render report — so
    /// a self-reporting agent shows up immediately. Returns whether the sidebar
    /// needs a repaint. The Phase 2 arbitration takes it from here: the hook is
    /// the authority unless a strong visible screen signal vetoes it.
    fn apply_hook_report(&mut self, report: HookReport) -> bool {
        let now = Instant::now();
        match report {
            HookReport::State {
                pane_id,
                source,
                agent_label,
                state,
                message,
            } => {
                let entry = self.agents.entry(pane_id).or_default();
                entry.set_hook_authority(source, agent_label, state, message, now)
            },
            HookReport::Release { pane_id } => match self.agents.get_mut(&pane_id) {
                // Releasing a pane we never tracked is a no-op.
                Some(entry) => entry.clear_hook_authority(now),
                None => false,
            },
        }
    }

    /// Push the current focus picture into each tracked pane's state: a pane is
    /// "viewed" when it is the focused pane in the active tab. Focusing a pane
    /// clears its Done-unseen notification (see [`PaneAgentState::set_focused`]),
    /// and the flag also tells the next completion whether it happened under the
    /// user's eye. Only our own session's panes are in the manifest, which is
    /// exactly the set whose screens we can observe.
    fn sync_focus(&mut self) {
        let active_tab = self
            .tabs
            .iter()
            .find(|tab| tab.active)
            .map(|tab| tab.position);
        for (tab_idx, panes_in_tab) in &self.panes.panes {
            let tab_is_active = active_tab == Some(*tab_idx);
            for pane in panes_in_tab {
                let pane_id = if pane.is_plugin {
                    PaneId::Plugin(pane.id)
                } else {
                    PaneId::Terminal(pane.id)
                };
                if let Some(entry) = self.agents.get_mut(&pane_id) {
                    entry.set_focused(tab_is_active && pane.is_focused);
                }
            }
        }
    }

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
