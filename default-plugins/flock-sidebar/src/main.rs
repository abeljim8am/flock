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
//!
//! Phase 6 gives each session a stable workspace identity. The forked server
//! records the folder it was launched in as `SessionInfo.workspace_root`, and
//! the sidebar groups sessions under that folder (see [`ui::group_sessions`])
//! instead of guessing from pane cwds.

mod detect;
mod hook;
mod palette;
mod sessionizer;
mod state;
mod ui;

use std::collections::{BTreeMap, HashSet};
use std::time::Instant;

use detect::{detect_agent, identify_agent_from_command, AgentState};
use hook::{parse_hook_report, HookReport, HOOK_PIPE_NAME};
use palette::Theme;
use sessionizer::SessionizerConfig;
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
/// How often the sidebar asks the host to rescan live sessions. `SessionUpdate`
/// events only reflect the server's cached view; this command refreshes that
/// cache from the live socket/session-metadata files so the workspace section
/// contains every running session.
const SESSION_REFRESH_SECS: f64 = 1.0;

/// Pipe message name (sent by a `MessagePlugin` keybind, e.g. Super b) that
/// toggles the sidebar between its slim rail and an expanded width. We resize
/// our *own* pane rather than swap the layout, so the user's content panes keep
/// their arrangement — only the sidebar/content split ratio changes.
const WIDTH_TOGGLE_PIPE: &str = "flock-toggle-width";
/// Width (cols) below which we treat the sidebar as collapsed and expand on
/// toggle; at or above it we collapse back to the rail. Sits between the slim
/// rail (~5) and the full-view threshold (16).
const WIDTH_EXPAND_THRESHOLD: usize = 14;
/// Target widths (cols) for the toggle. Fixed column counts — not a screen
/// relative percent — so the expanded sidebar is the same size on a laptop and
/// on an ultrawide rather than stretching to fill.
const SIDEBAR_SLIM_COLS: usize = 3;
const SIDEBAR_EXPANDED_COLS: usize = 40;

/// Session name used by the flock-selector cold-shell entry point (set via its
/// `session_name` layout arg). It's the picker's throwaway host session, not a
/// workspace, so the sidebar always hides it from the workspace list. Must match
/// the `session_name` value in the bundled `flock-selector` layout.
const HIDDEN_SESSION_NAME: &str = "flock-selector";

#[derive(Default)]
struct State {
    /// Whether our permission request has been granted yet. Until it is, we
    /// can't read pane contents / application state, so we render a hint.
    permissions_granted: bool,
    /// Latest pane manifest for our own session.
    panes: PaneManifest,
    /// Latest tab list for our own session.
    tabs: Vec<TabInfo>,
    /// Latest cross-session list, grouped by `workspace_root` in the sidebar.
    sessions: Vec<SessionInfo>,
    /// Optional sessionizer-style filter. When configured with the same
    /// `individual_dirs` / `root_dirs` args as flock-selector, the workspace
    /// list only shows sessions that belong to those projects.
    sessionizer: SessionizerConfig,
    /// Last time we explicitly refreshed the cross-session list via
    /// `get_session_list`.
    last_session_refresh: Option<Instant>,
    /// Per-pane agent detection + arbitrated state, keyed by pane id.
    agents: BTreeMap<PaneId, PaneAgentState>,
    /// The last per-pane agent status we published to the cross-session bus
    /// (Phase 7). Diffed against the freshly-built status on each update so we
    /// only `publish_agent_state` — and thus only re-serialize the session
    /// metadata to disk — when the published picture actually changes.
    last_published: BTreeMap<PaneId, PaneAgentStatus>,
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
    /// Our own plugin id (from `get_plugin_ids`), used to find our pane in the
    /// manifest so the selection cursor only shows while the sidebar is focused.
    own_plugin_id: u32,
    /// Whether our own plugin pane is the focused pane in the active tab. The
    /// selection cursor is hidden when this is false, so an unfocused ambient
    /// rail shows only status — no cursor.
    focused: bool,
    /// Whether we've applied the one-time default width after the layout first
    /// reports our geometry. The flock layout opens the sidebar at a resizable
    /// percent (so Super b can toggle it in place); once we know the real
    /// geometry we resize to the fixed expanded width so the sidebar starts in
    /// the full labeled view rather than at whatever the percent happens to be.
    default_width_applied: bool,
}

register_plugin!(State);

impl ZellijPlugin for State {
    fn load(&mut self, configuration: BTreeMap<String, String>) {
        self.sessionizer = SessionizerConfig::from_args(&configuration);

        // Exclude the sidebar from focus navigation, like zellij's own tab-bar /
        // status-bar: Ctrl-h/l skip over it instead of landing on it, and it's a
        // glance-and-click ambient rail (mouse clicks still work) rather than a
        // keyboard-focusable pane.
        set_selectable(false);

        // Permissions needed across all phases:
        // - ReadApplicationState: pane/tab/session manifests
        // - ReadPaneContents: PaneRenderReportWithAnsi screen scraping (Phase 2)
        // - ChangeApplicationState: switch session / focus pane on activation
        // - ReadCliPipes: agent hook reports via `zellij pipe` (Phase 5)
        request_permission(&[
            PermissionType::ReadApplicationState,
            PermissionType::ReadPaneContents,
            PermissionType::ChangeApplicationState,
            PermissionType::ReadCliPipes,
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

        // Our own pane id, to detect when the sidebar itself is focused.
        self.own_plugin_id = get_plugin_ids().plugin_id;

        // Drive the time-based stabilization windows. Re-armed on each Timer.
        set_timeout(STATE_TICK_SECS);
        self.timer_running = true;
    }

    fn update(&mut self, event: Event) -> bool {
        let mut should_render = false;
        match event {
            Event::PermissionRequestResult(result) => {
                self.permissions_granted = matches!(result, PermissionStatus::Granted);
                if self.permissions_granted {
                    self.refresh_session_list(Instant::now());
                }
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
                // First time we see the real geometry, size to the default view.
                self.maybe_set_default_width();
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
                if self.should_refresh_session_list(now) {
                    should_render |= self.refresh_session_list(now);
                }
                // Catch the resize if permissions/geometry weren't ready when
                // the first PaneUpdate arrived (runs once, gated by the flag).
                self.maybe_set_default_width();
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
        // Any handled event may have changed an agent's state; mirror the latest
        // picture onto the cross-session bus (no-op when unchanged).
        self.publish_state_if_changed();
        should_render
    }

    fn pipe(&mut self, pipe_message: PipeMessage) -> bool {
        // The width-toggle channel (Super b → MessagePlugin) resizes our pane.
        if pipe_message.name == WIDTH_TOGGLE_PIPE {
            self.toggle_width();
            return false; // the resize itself triggers a re-render
        }
        // Only the agent self-report channel concerns us otherwise; ignore the
        // rest so we don't claim pipes meant for other plugins.
        if pipe_message.name != HOOK_PIPE_NAME {
            return false;
        }
        let should_render = match parse_hook_report(&pipe_message.args) {
            Ok(report) => self.apply_hook_report(report),
            Err(reason) => {
                // A malformed report is dropped, not applied — log for the
                // operator and leave every pane's state untouched.
                eprintln!("flock-sidebar: ignoring {HOOK_PIPE_NAME} report: {reason}");
                false
            },
        };
        // A hook report can change agent state; publish it cross-session.
        self.publish_state_if_changed();
        should_render
    }

    fn render(&mut self, rows: usize, cols: usize) {
        self.rows = rows;
        self.cols = cols;
        let sessions = self.visible_sessions();

        let output = ui::render(ui::RenderInput {
            permissions_granted: self.permissions_granted,
            panes: &self.panes,
            tabs: &self.tabs,
            agents: &self.agents,
            sessions: &sessions,
            palette: &self.palette,
            focused: self.focused,
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

    /// Whether enough time has elapsed to refresh the cross-session list. Kept
    /// separate from the animation cadence so a working spinner doesn't turn
    /// into an aggressive disk/socket poll.
    fn should_refresh_session_list(&self, now: Instant) -> bool {
        self.permissions_granted
            && self
                .last_session_refresh
                .is_none_or(|last| now.duration_since(last).as_secs_f64() >= SESSION_REFRESH_SECS)
    }

    /// Ask the host for a fresh live-session snapshot. The host also feeds the
    /// result back into the server's `SessionUpdate` cache, but updating our
    /// local copy here avoids waiting for the round trip.
    fn refresh_session_list(&mut self, now: Instant) -> bool {
        self.last_session_refresh = Some(now);
        match get_session_list() {
            Ok(snapshot) => {
                if self.sessions == snapshot.live_sessions {
                    false
                } else {
                    self.sessions = snapshot.live_sessions;
                    true
                }
            },
            Err(reason) => {
                eprintln!("flock-sidebar: failed to refresh session list: {reason}");
                false
            },
        }
    }

    /// The ordered navigable targets (sessions then agents). Rebuilt on demand;
    /// the same ordering drives the render, so indices line up.
    fn targets(&self) -> Vec<Target> {
        let sessions = self.visible_sessions();
        ui::navigable_targets(&self.panes, &self.tabs, &self.agents, &sessions)
    }

    /// Sessions visible in the workspace section. The flock-selector's cold-shell
    /// entry session (named [`HIDDEN_SESSION_NAME`]) is always hidden — it's the
    /// picker's throwaway host, not a workspace. With no sessionizer config, every
    /// other live session remains visible for backwards-compatible default
    /// behavior; otherwise only sessions whose workspace is in the configured set.
    fn visible_sessions(&self) -> Vec<SessionInfo> {
        self.sessions
            .iter()
            .filter(|session| session.name != HIDDEN_SESSION_NAME)
            .filter(|session| {
                !self.sessionizer.is_configured()
                    || self.sessionizer.contains_workspace(&session.workspace_root)
            })
            .cloned()
            .collect()
    }

    /// Toggle the sidebar between its slim rail and an expanded width by
    /// resizing our *own* pane. Resizing only shifts the split between the
    /// sidebar and the content area beside it — content panes keep their
    /// arrangement (unlike a swap layout, which re-fits everything). Direction
    /// is chosen from the current width so it self-corrects rather than relying
    /// on a stored flag.
    fn toggle_width(&self) {
        let (_, total) = self.sidebar_and_tab_cols();
        // Decide purely from the sidebar's *actual* current width. Use our own
        // last-rendered width (`self.cols`) rather than the pane manifest: the
        // plugin re-renders at the new size right after a resize, so this is
        // fresh, whereas the manifest can still report the previous width.
        // Narrower than the midpoint between slim and expanded ⇒ expand;
        // otherwise collapse. No stored flag to get out of sync.
        let current = self.cols.max(1);
        let midpoint = (SIDEBAR_SLIM_COLS + SIDEBAR_EXPANDED_COLS) / 2;
        let expanding = current < midpoint;
        // Toward a fixed target column count (capped to half the tab on small
        // terminals so it never crowds out the content) — a fixed width, not a
        // screen-relative percent, so it's the same on a laptop and an ultrawide.
        let target = if expanding {
            SIDEBAR_EXPANDED_COLS.min(total / 2).max(WIDTH_EXPAND_THRESHOLD)
        } else {
            SIDEBAR_SLIM_COLS
        };
        self.resize_toward(target, current, total);
    }

    /// Once we know the real layout geometry, resize the sidebar to its default
    /// expanded width so it starts in the full labeled view. The flock layout
    /// opens the rail at a resizable percent (kept a percent so Super b can
    /// later toggle it in place); the first time we see a content pane beside us
    /// we resize to the fixed expanded target — the same width Super b expands
    /// to. Runs once — it must not fight a later Super b toggle.
    fn maybe_set_default_width(&mut self) {
        if self.default_width_applied || !self.permissions_granted {
            return;
        }
        let (current, total) = self.sidebar_and_tab_cols();
        // Wait until the manifest reports a content pane beside us (total wider
        // than our own width) before trusting the geometry.
        if total <= current {
            return;
        }
        self.default_width_applied = true;
        // Cap to half the tab on small terminals so the sidebar never crowds out
        // the content — matching the expand branch of `toggle_width`.
        let target = SIDEBAR_EXPANDED_COLS.min(total / 2).max(WIDTH_EXPAND_THRESHOLD);
        if target != current {
            self.resize_toward(target, current, total);
        }
    }

    /// Resize our own pane toward `target` columns, given the sidebar's
    /// `current` width and the tab `total`. Each resize step is ~5% of the tab
    /// width; the column delta is converted into a step count so we land near
    /// the target. At least one step so a toggle always moves.
    fn resize_toward(&self, target: usize, current: usize, total: usize) {
        let own = PaneId::Plugin(self.own_plugin_id);
        let expanding = target > current;
        let step_cols = ((total as f64) * 0.05).max(1.0);
        let delta = (target as i64 - current as i64).unsigned_abs() as f64;
        let steps = ((delta / step_cols).round() as usize).max(1);
        let resize = if expanding {
            Resize::Increase
        } else {
            Resize::Decrease
        };
        let strategy = ResizeStrategy::new(resize, Some(Direction::Right));
        for _ in 0..steps {
            resize_pane_with_id(strategy, own);
        }
    }

    /// The sidebar's current width and the active tab's total width (cols), read
    /// from the pane manifest. Falls back to the last render width if the
    /// manifest geometry isn't available yet.
    fn sidebar_and_tab_cols(&self) -> (usize, usize) {
        let active = self.tabs.iter().find(|tab| tab.active).map(|tab| tab.position);
        let mut current = self.cols.max(1);
        let mut total = self.cols.max(1);
        if let Some(panes) = active.and_then(|idx| self.panes.panes.get(&idx)) {
            for pane in panes {
                let right = pane.pane_x + pane.pane_columns;
                if right > total {
                    total = right;
                }
                if pane.is_plugin && pane.id == self.own_plugin_id {
                    current = pane.pane_columns;
                }
            }
        }
        (current, total)
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

    /// Build this session's per-pane agent status from the live tracked state
    /// and, if it differs from what we last published, push it to the server's
    /// cross-session bus (Phase 7). The server stores it on this session's
    /// `SessionInfo`, where every other session's sidebar reads it (via the
    /// session-list poll) to render this workspace's agents in full fidelity.
    /// Only agent panes are published; the diff guard means a republish with no
    /// change does not re-serialize the session metadata to disk.
    fn publish_state_if_changed(&mut self) {
        let mut states = BTreeMap::new();
        for (pane_id, st) in &self.agents {
            if !st.is_agent() {
                continue;
            }
            states.insert(
                *pane_id,
                PaneAgentStatus {
                    state: to_run_state(st.state),
                    label: st.effective_agent_label().unwrap_or_default(),
                    seen: st.seen,
                },
            );
        }
        if states != self.last_published {
            self.last_published = states.clone();
            publish_agent_state(states);
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
        let own = self.own_plugin_id;
        let mut self_focused = false;
        for (tab_idx, panes_in_tab) in &self.panes.panes {
            let tab_is_active = active_tab == Some(*tab_idx);
            for pane in panes_in_tab {
                let pane_id = if pane.is_plugin {
                    PaneId::Plugin(pane.id)
                } else {
                    PaneId::Terminal(pane.id)
                };
                // Our own pane being focused in the active tab enables the cursor.
                if pane.is_plugin && pane.id == own && tab_is_active && pane.is_focused {
                    self_focused = true;
                }
                if let Some(entry) = self.agents.get_mut(&pane_id) {
                    entry.set_focused(tab_is_active && pane.is_focused);
                }
            }
        }
        self.focused = self_focused;
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

/// Map the plugin's internal agent state to the serializable, cross-session
/// [`AgentRunState`] carried on `SessionInfo` (Phase 7).
fn to_run_state(state: AgentState) -> AgentRunState {
    match state {
        AgentState::Idle => AgentRunState::Idle,
        AgentState::Working => AgentRunState::Working,
        AgentState::Blocked => AgentRunState::Blocked,
        AgentState::Unknown => AgentRunState::Unknown,
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
