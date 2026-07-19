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

mod coder;
mod codespace;
mod detect;
mod devcontainer;
mod hook;
mod palette;
mod sessionizer;
mod state;
mod ui;

use std::collections::{BTreeMap, HashSet};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use detect::{detect_agent, identify_agent_from_command, identify_agent_from_screen, AgentState};
use hook::{parse_hook_report, HookReport, HOOK_PIPE_NAME};
use palette::Theme;
use sessionizer::SessionizerConfig;
use state::PaneAgentState;
use ui::{ClickTarget, SidebarMode, Target};
use zellij_tile::prelude::*;

/// How often we re-evaluate time-based holds/grace windows when nothing is
/// animating. herdr polled every 300ms; we only need a tick frequent enough to
/// expire the 1.2s Claude hold and the 2s stale-hook window without a new render
/// report.
const STATE_TICK_SECS: f64 = 0.5;
/// Faster cadence used while at least one agent is working, so the spinner
/// animates smoothly (~8 frames/sec).
const SPINNER_TICK_SECS: f64 = 0.12;
/// How long without a pushed render report before we treat our session as
/// backgrounded (no client attached) and start pulling agent pane contents on
/// the timer instead. Comfortably above the slow `STATE_TICK_SECS` cadence so a
/// merely-idle foreground session keeps using the cheaper pushed reports.
const RENDER_REPORT_STALE_SECS: f64 = 1.5;
/// How often the sidebar asks the host to rescan live sessions. `SessionUpdate`
/// events only reflect the server's cached view; this command refreshes that
/// cache from the live socket/session-metadata files so the workspace section
/// contains every running session.
const SESSION_REFRESH_SECS: f64 = 1.0;
/// How often to reconcile pane command identity outside PaneUpdate. Session
/// switches can leave the plugin rendering before a fresh command event arrives.
const AGENT_COMMAND_SYNC_SECS: f64 = 1.0;
/// How often to poll the devcontainer hook files (`docker exec … cat`) when
/// this session is devcontainer-bound. Each poll forks a docker client, so it
/// rides a slower cadence than the state tick; a few seconds of state latency
/// is fine for a status dot.
const DEVCONTAINER_HOOK_POLL_SECS: f64 = 3.0;
const CODER_SNAPSHOT_POLL_SECS: f64 = 3.0;
const CODER_SNAPSHOT_STALE_SECS: f64 = 7.0;

/// Pipe message name (sent by a `MessagePlugin` keybind, e.g. Super b) that
/// toggles the sidebar between its slim rail and an expanded width. We resize
/// our *own* pane rather than swap the layout, so the user's content panes keep
/// their arrangement — only the sidebar/content split ratio changes.
const WIDTH_TOGGLE_PIPE: &str = "flock-toggle-width";
/// Floor (cols) for the expanded width on small terminals: the half-the-tab
/// cap in `set_width_for_mode` never shrinks the expanded sidebar below this.
/// Sits between the slim rail (~5) and the full-view threshold (16).
const WIDTH_EXPAND_THRESHOLD: usize = 14;
/// Target widths (cols) for the toggle. Fixed column counts — not a screen
/// relative percent — so the expanded sidebar is the same size on a laptop and
/// on an ultrawide rather than stretching to fill.
const SIDEBAR_SLIM_COLS: usize = 5;
const SIDEBAR_EXPANDED_COLS: usize = 40;

/// Session name used by the flock-selector cold-shell entry point (set via its
/// `session_name` layout arg). It's the picker's throwaway host session, not a
/// workspace, so the sidebar always hides it from the workspace list. Must match
/// the `session_name` value in the bundled `flock-selector` layout.
const HIDDEN_SESSION_NAME: &str = "flock-selector";

#[derive(Default)]
struct State {
    /// A remote instance can run invisibly as the authoritative tracker for a
    /// durable Coder session and answer snapshot pipe requests.
    tracker_only: bool,
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
    /// Last time we reconciled pane ids with their foreground commands.
    last_agent_command_sync: Option<Instant>,
    /// Panes whose foreground command we've already resolved — either via a
    /// live `CommandChanged` event or one `get_pane_running_command` host
    /// query. Each host query forks a full `ps` process-table scan on the pty
    /// thread, so the manifest sync only queries panes it hasn't answered yet
    /// instead of every terminal pane on every tick; `CommandChanged` is the
    /// live path for later foreground changes.
    command_synced: HashSet<PaneId>,
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
    /// User-requested sidebar presentation. Width follows this state, and the
    /// renderer uses it for both the workspaces and agents sections.
    sidebar_mode: SidebarMode,
    /// Timestamp attached to the last local/adopted sidebar mode. Cross-session
    /// sync uses this so every live sidebar converges on the newest toggle.
    sidebar_state_updated_at_millis: u64,
    /// The last sidebar mode state we published to the cross-session metadata
    /// bus. Diffed to avoid rewriting session metadata on every event.
    last_published_sidebar_state: Option<FlockSidebarState>,
    /// Unified keyboard selection cursor over the sessions then the agents.
    selected: usize,
    /// Scroll offset into the workspaces (sessions) section.
    scroll_sessions: usize,
    /// Scroll offset into the agents section.
    scroll_agents: usize,
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
    /// When we last received a pushed `PaneRenderReportWithAnsi`. The host only
    /// emits those while a client is attached to our session, so once they go
    /// stale (we've been switched away from) we fall back to *pulling* each
    /// agent pane's contents on the timer — see [`State::pull_agent_screens`] —
    /// keeping a backgrounded session's agent state live cross-session.
    last_render_report_at: Option<Instant>,
    /// When we last pulled agent pane contents ourselves. Pulls serialize each
    /// pane's grid across the wasm boundary, so they're clamped to the slow
    /// state cadence even when the timer runs at the spinner cadence.
    last_screen_pull: Option<Instant>,
    /// Whether we've applied the one-time default width after the layout first
    /// reports our geometry. The flock layout opens the sidebar at a resizable
    /// percent (so Super b can toggle it in place); once we know the real
    /// geometry we resize to the fixed expanded width so the sidebar starts in
    /// the full labeled view rather than at whatever the percent happens to be.
    default_width_applied: bool,
    /// When we last polled the devcontainer hook files (bound sessions only).
    last_devcontainer_hook_poll: Option<Instant>,
    /// The resolved container id of this session's devcontainer, cached
    /// between polls; cleared when a poll says the container is gone.
    devcontainer_container_id: Option<String>,
    /// Latest authoritative agent snapshot from the durable Zellij server in
    /// this gateway's Coder workspace.
    coder_snapshot: Option<coder::Snapshot>,
    coder_snapshot_workspace: Option<String>,
    last_coder_snapshot_poll: Option<Instant>,
    last_coder_snapshot_received: Option<Instant>,
    coder_snapshot_poll_in_flight: bool,
}

register_plugin!(State);

impl ZellijPlugin for State {
    fn load(&mut self, configuration: BTreeMap<String, String>) {
        self.sessionizer = SessionizerConfig::from_args(&configuration);
        self.tracker_only = configuration
            .get("tracker_only")
            .is_some_and(|value| value.trim().eq_ignore_ascii_case("true"));
        if self.tracker_only {
            hide_self();
        }

        // Exclude the sidebar from focus navigation, like zellij's own tab-bar /
        // status-bar: Ctrl-h/l skip over it instead of landing on it, and it's a
        // glance-and-click ambient rail (mouse clicks still work) rather than a
        // keyboard-focusable pane.
        set_selectable(false);

        // Permissions needed across all phases:
        // - ReadApplicationState: pane/tab/session manifests
        // - ReadPaneContents: PaneRenderReportWithAnsi screen scraping (Phase 2)
        // - ChangeApplicationState: switch session / focus pane on activation,
        //   resize our pane, and publish cross-session sidebar state
        // - ReadCliPipes: agent hook reports via `zellij pipe` (Phase 5)
        // - RunCommands: polling devcontainer hook files via docker ps/exec
        //   (the in-container agents can't reach `zellij pipe`)
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
            EventType::RunCommandResult,
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
                let now = Instant::now();
                self.panes = manifest;
                // Drop tracked state for panes that no longer exist.
                self.prune_closed_panes();
                // Re-seed agent identity from the live pane manifest / process
                // table. When a client switches away and back, the previous
                // CommandChanged event may not replay for an already-running
                // agent, so this keeps the detail list from falling back to none.
                self.sync_agents_from_manifest(now);
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
                self.sync_sidebar_mode_from_sessions();
                should_render = true;
            },
            Event::CommandChanged(pane_id, command, is_foreground, _focused_clients) => {
                if self.apply_command_changed(pane_id, &command, is_foreground, Instant::now()) {
                    should_render = true;
                }
            },
            Event::PaneRenderReportWithAnsi(pane_contents) => {
                let now = Instant::now();
                self.last_render_report_at = Some(now);
                for (pane_id, contents) in pane_contents {
                    let screen = screen_text(&contents);
                    if self.observe_pane_screen(pane_id, &screen, now) {
                        should_render = true;
                    }
                }
            },
            Event::RunCommandResult(exit_code, stdout, stderr, context) => {
                if self.sessionizer.coder_enabled()
                    && context.contains_key(coder::SNAPSHOT_CONTEXT_KEY)
                {
                    self.coder_snapshot_poll_in_flight = false;
                    if exit_code == Some(0) {
                        match coder::parse_snapshot(&String::from_utf8_lossy(&stdout)) {
                            Ok(snapshot) => {
                                if let Some(identifier) = context.get(coder::SNAPSHOT_CONTEXT_KEY) {
                                    coder::save_cached(identifier, &snapshot);
                                }
                                self.coder_snapshot = Some(snapshot);
                                self.last_coder_snapshot_received = Some(Instant::now());
                                should_render = true;
                            },
                            Err(reason) => {
                                eprintln!("flock-sidebar: invalid Coder snapshot: {reason}");
                            },
                        }
                    } else if !stderr.is_empty() {
                        eprintln!(
                            "flock-sidebar: Coder snapshot failed: {}",
                            String::from_utf8_lossy(&stderr).trim()
                        );
                    }
                } else if self.sessionizer.devcontainers_enabled()
                    && context.contains_key(devcontainer::PS_CONTEXT_KEY)
                {
                    let id = String::from_utf8_lossy(&stdout)
                        .lines()
                        .next()
                        .map(str::trim)
                        .unwrap_or_default()
                        .to_string();
                    if exit_code == Some(0) && !id.is_empty() {
                        // Read the hook files right away rather than waiting a
                        // full poll cycle behind the id lookup.
                        self.devcontainer_container_id = Some(id.clone());
                        self.fire_devcontainer_hooks_read(&id);
                    }
                } else if self.sessionizer.devcontainers_enabled()
                    && context.contains_key(devcontainer::HOOKS_CONTEXT_KEY)
                {
                    // Parse whatever printed regardless of exit code: a bound
                    // session with no reports yet exits 1 (the glob matched
                    // nothing) with empty output.
                    should_render =
                        self.apply_devcontainer_hook_lines(&String::from_utf8_lossy(&stdout));
                    if exit_code != Some(0) {
                        let stderr_lower = String::from_utf8_lossy(&stderr).to_lowercase();
                        if stderr_lower.contains("container") {
                            // docker says the container is gone/stopped —
                            // re-resolve the id on the next poll (a pane's
                            // self-healing `up` may bring it back).
                            self.devcontainer_container_id = None;
                        }
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
                if self.should_sync_agent_commands(now) {
                    should_render |= self.sync_agents_from_manifest(now);
                }
                // When pushed render reports have gone stale — i.e. no client is
                // attached because we've been switched away from — pull each
                // agent pane's screen ourselves so detection keeps running and we
                // keep publishing live state for the cross-session view. Pulls
                // are bounded to the slow state cadence so a working spinner
                // never turns into an 8Hz scrollback poll of a session nobody
                // is looking at.
                let reports_are_stale = self.render_reports_are_stale(now);
                if reports_are_stale && self.should_pull_agent_screens(now) {
                    self.last_screen_pull = Some(now);
                    should_render |= self.pull_agent_screens(now);
                }
                // While anything is working, animate the spinner and tick faster;
                // otherwise fall back to the slow hold/grace cadence. With stale
                // reports no client is attached, so there is no spinner to
                // animate — stay on the slow cadence.
                let working = self.any_working();
                if working {
                    self.spinner_tick = self.spinner_tick.wrapping_add(1);
                    should_render = true;
                }
                if self.should_refresh_session_list(now) {
                    should_render |= self.refresh_session_list(now);
                }
                // In a devcontainer-bound session, poll the in-container hook
                // files (results arrive as RunCommandResult events).
                self.maybe_poll_devcontainer_hooks(now);
                self.maybe_poll_coder_snapshot(now);
                // Catch the resize if permissions/geometry weren't ready when
                // the first PaneUpdate arrived (runs once, gated by the flag).
                self.maybe_set_default_width();
                set_timeout(if working && !reports_are_stale {
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
                        // Do not close on Esc: one plugin instance backs the
                        // sidebar panes across tabs, so close_self() here tears
                        // down every sidebar and looks like a plugin crash.
                        _ => {},
                    }
                }
            },
            _ => {},
        }
        // Any handled event may have changed an agent's state; mirror the latest
        // picture onto the cross-session bus (no-op when unchanged).
        self.publish_state_if_changed();
        self.publish_sidebar_state_if_changed();
        should_render
    }

    fn pipe(&mut self, pipe_message: PipeMessage) -> bool {
        // The width-toggle channel (Super b → MessagePlugin) resizes our pane.
        if pipe_message.name == WIDTH_TOGGLE_PIPE {
            self.toggle_width();
            self.publish_sidebar_state_if_changed();
            return false; // the resize itself triggers a re-render
        }
        if pipe_message.name == coder::SNAPSHOT_PIPE_NAME {
            self.publish_state_if_changed();
            let mut snapshot = coder::Snapshot::from_states(
                now_millis(),
                self.current_session_name()
                    .unwrap_or_else(|| "flock".to_owned()),
                &self.last_published,
            );
            for pane in &mut snapshot.panes {
                pane.focused = self.pane_is_focused(pane.pane_id);
            }
            match serde_json::to_string(&snapshot) {
                Ok(json) => cli_pipe_output(&pipe_message.name, &json),
                Err(reason) => eprintln!("flock-sidebar: snapshot serialization failed: {reason}"),
            }
            return false;
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
        if self.tracker_only {
            return;
        }
        self.rows = rows;
        self.cols = cols;
        let sessions = self.render_sessions();

        let output = ui::render(ui::RenderInput {
            permissions_granted: self.permissions_granted,
            panes: &self.panes,
            tabs: &self.tabs,
            agents: &self.agents,
            remote_agents: &self.remote_agent_entries(),
            sessions: &sessions,
            palette: &self.palette,
            sidebar_mode: self.sidebar_mode,
            focused: self.focused,
            selected: self.selected,
            scroll_sessions: self.scroll_sessions,
            scroll_agents: self.scroll_agents,
            spinner_tick: self.spinner_tick,
            rows,
            cols,
        });
        self.selected = output.selected;
        self.scroll_sessions = output.scroll_sessions;
        self.scroll_agents = output.scroll_agents;
        self.click_map = output.click_map;
        print!("{}", output.ansi);
    }
}

impl State {
    fn pane_is_focused(&self, pane_id: PaneId) -> bool {
        let active_tab = self
            .tabs
            .iter()
            .find(|tab| tab.active)
            .map(|tab| tab.position);
        active_tab
            .and_then(|tab| self.panes.panes.get(&tab))
            .is_some_and(|panes| {
                panes.iter().any(|pane| {
                    let candidate = if pane.is_plugin {
                        PaneId::Plugin(pane.id)
                    } else {
                        PaneId::Terminal(pane.id)
                    };
                    candidate == pane_id && pane.is_focused
                })
            })
    }
    fn current_session_name(&self) -> Option<String> {
        self.sessions
            .iter()
            .find(|session| session.is_current_session)
            .map(|session| session.name.clone())
    }

    fn current_coder_workspace(&self) -> Option<String> {
        if !self.sessionizer.coder_enabled() {
            return None;
        }
        self.sessions
            .iter()
            .find(|session| session.is_current_session)
            .and_then(|session| session.default_command.as_deref())
            .and_then(coder::parse_coder_ssh)
            .map(str::to_owned)
    }

    fn maybe_poll_coder_snapshot(&mut self, now: Instant) {
        if self.tracker_only {
            return;
        }
        let Some(identifier) = self.current_coder_workspace() else {
            self.coder_snapshot_workspace = None;
            self.coder_snapshot = None;
            self.last_coder_snapshot_received = None;
            self.coder_snapshot_poll_in_flight = false;
            return;
        };
        if self.coder_snapshot_workspace.as_deref() != Some(identifier.as_str()) {
            self.coder_snapshot_workspace = Some(identifier.clone());
            self.coder_snapshot = coder::load_cached(&identifier);
            self.last_coder_snapshot_poll = None;
            self.last_coder_snapshot_received = None;
            self.coder_snapshot_poll_in_flight = false;
        }
        if self.coder_snapshot_poll_in_flight {
            return;
        }
        if self
            .last_coder_snapshot_poll
            .is_some_and(|last| now.duration_since(last).as_secs_f64() < CODER_SNAPSHOT_POLL_SECS)
        {
            return;
        }
        self.last_coder_snapshot_poll = Some(now);
        self.coder_snapshot_poll_in_flight = true;
        let argv = coder::snapshot_argv(&identifier);
        let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
        run_command(&refs, coder::snapshot_context(&identifier));
    }

    fn coder_snapshot_stale(&self, now: Instant) -> bool {
        self.current_coder_workspace().is_some()
            && self.last_coder_snapshot_received.is_none_or(|received| {
                now.duration_since(received).as_secs_f64() >= CODER_SNAPSHOT_STALE_SECS
            })
    }
    /// Whether any agent — in this session or any other — is currently Working.
    /// Drives the faster spinner-animation cadence; the cross-session check keeps
    /// the spinner animating for working agents shown from the published bus, not
    /// just our own panes.
    fn any_working(&self) -> bool {
        self.agents
            .values()
            .any(|st| st.is_agent() && st.state == AgentState::Working)
            || self.sessions.iter().any(|session| {
                session
                    .agent_states
                    .values()
                    .any(|status| matches!(status.state, AgentRunState::Working))
            })
    }

    /// Whether pushed render reports have gone stale (no client attached). When
    /// true we drive detection by pulling instead — see [`pull_agent_screens`].
    fn render_reports_are_stale(&self, now: Instant) -> bool {
        self.last_render_report_at
            .is_none_or(|last| now.duration_since(last).as_secs_f64() >= RENDER_REPORT_STALE_SECS)
    }

    /// Pull each tracked agent pane's on-screen contents and re-run detection.
    /// The host serves `get_pane_scrollback` straight from the pane's grid, which
    /// it maintains regardless of whether a client is attached — so this keeps a
    /// backgrounded session's agent state live when the pushed
    /// `PaneRenderReportWithAnsi` events have dried up. Returns whether any
    /// agent's state changed. Panes that have since closed return an error and
    /// are skipped (pruning removes them on the next `PaneUpdate`).
    fn pull_agent_screens(&mut self, now: Instant) -> bool {
        // Remote panes are pulled even before an agent is identified — the
        // screen is their only identification source, so a backgrounded bound
        // session must keep probing or a remote agent never appears.
        let pane_ids: Vec<PaneId> = self
            .agents
            .iter()
            .filter(|(_, st)| st.is_agent() || st.remote)
            .map(|(pane_id, _)| *pane_id)
            .collect();
        let mut changed = false;
        for pane_id in pane_ids {
            let Ok(contents) = get_pane_scrollback(pane_id, false) else {
                continue;
            };
            let screen = screen_text(&contents);
            if self.observe_pane_screen(pane_id, &screen, now) {
                changed = true;
            }
        }
        changed
    }

    /// Feed one pane's screen text through identification (remote panes) and
    /// state detection. For a remote (codespace SSH) pane the screen is the
    /// only agent-identity source: confident agent chrome (re)identifies the
    /// pane's agent and cancels any pending absence window, while a live frame
    /// with no recognizable chrome opens it (release happens in `tick` after
    /// the remote grace). Returns whether the arbitrated state changed.
    fn observe_pane_screen(&mut self, pane_id: PaneId, screen: &str, now: Instant) -> bool {
        let entry = self.agents.entry(pane_id).or_default();
        let mut agent = entry.detected_agent;
        if entry.remote {
            match identify_agent_from_screen(screen) {
                Some(identified) => {
                    agent = Some(identified);
                    entry.set_detected_agent(agent, now);
                },
                None => {
                    // An overlay screen (transcript viewer, model picker)
                    // hides the chrome without saying the agent is gone.
                    if agent.is_some() && !detect_agent(agent, screen).skip_state_update {
                        entry.mark_agent_missing(now);
                    }
                },
            }
        }
        let detection = detect_agent(agent, screen);
        entry.observe_screen(agent, detection, now)
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

    /// Whether enough time has elapsed since the last self-initiated screen
    /// pull. Keeps the pull cadence at `STATE_TICK_SECS` even when the timer
    /// itself runs at the (much faster) spinner cadence.
    fn should_pull_agent_screens(&self, now: Instant) -> bool {
        self.last_screen_pull
            .is_none_or(|last| now.duration_since(last).as_secs_f64() >= STATE_TICK_SECS)
    }

    fn should_sync_agent_commands(&self, now: Instant) -> bool {
        self.permissions_granted
            && self.last_agent_command_sync.is_none_or(|last| {
                now.duration_since(last).as_secs_f64() >= AGENT_COMMAND_SYNC_SECS
            })
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
                    self.sync_sidebar_mode_from_sessions();
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
        ui::navigable_targets(
            &self.panes,
            &self.tabs,
            &self.agents,
            &sessions,
            &self.remote_agent_entries(),
        )
    }

    fn remote_agent_entries(&self) -> Vec<ui::RemoteAgentEntry> {
        let Some(snapshot) = self.coder_snapshot.as_ref() else {
            return Vec::new();
        };
        let stale = self.coder_snapshot_stale(Instant::now());
        snapshot
            .panes
            .iter()
            .map(|pane| ui::RemoteAgentEntry {
                pane_id: pane_id_string(pane.pane_id),
                label: if stale {
                    format!("{} · offline", pane.label)
                } else {
                    pane.label.clone()
                },
                state: if stale {
                    AgentState::Unknown
                } else {
                    run_state_to_detected(pane.status.state)
                },
                seen: pane.status.seen,
                is_active: !stale && pane.focused,
            })
            .collect()
    }

    /// Sessions visible in the workspace section. The flock-selector's cold-shell
    /// entry session (named [`HIDDEN_SESSION_NAME`]) is always hidden — it's the
    /// picker's throwaway host, not a workspace. With no sessionizer config, every
    /// other live session remains visible for backwards-compatible default
    /// behavior; otherwise only sessions whose workspace is in the configured set —
    /// plus remote-bound sessions: a codespace's `workspace_root` is never a
    /// configured project folder (the workspace lives inside the codespace), and
    /// a devcontainer session's usually is, but keeping the binding check makes
    /// it visible even when the sidebar's dir args diverge from the selector's.
    fn visible_sessions(&self) -> Vec<SessionInfo> {
        self.sessions
            .iter()
            .filter(|session| session.name != HIDDEN_SESSION_NAME)
            .filter(|session| {
                !self.sessionizer.is_configured()
                    || self.sessionizer.contains_workspace(&session.workspace_root)
                    || session
                        .default_command
                        .as_deref()
                        .and_then(|command| self.parse_enabled_remote_binding(command))
                        .is_some()
            })
            .cloned()
            .map(|mut session| {
                if session.default_command.as_deref().is_some_and(|command| {
                    parse_remote_binding(command).is_some()
                        && self.parse_enabled_remote_binding(command).is_none()
                }) {
                    // UI row construction deliberately knows only binding
                    // shapes. Scrubbing disabled bindings here prevents badges
                    // and remote behavior from leaking through that layer.
                    session.default_command = None;
                }
                session
            })
            .collect()
    }

    /// The session list used for rendering, with this plugin's last published
    /// state overlaid onto the current session. The cross-session snapshot can
    /// lag a refresh behind after switching sessions; using the local publish
    /// cache avoids a one-frame Unknown icon while screen detection catches up.
    fn render_sessions(&self) -> Vec<SessionInfo> {
        let mut sessions = self.visible_sessions();
        self.overlay_last_published_agent_state(&mut sessions);
        sessions
    }

    fn overlay_last_published_agent_state(&self, sessions: &mut [SessionInfo]) {
        if self.last_published.is_empty() {
            return;
        }
        let Some(current) = sessions
            .iter_mut()
            .find(|session| session.is_current_session)
        else {
            return;
        };
        for (pane_id, status) in &self.last_published {
            if status.state == AgentRunState::Unknown {
                continue;
            }
            let should_overlay = current.agent_states.get(pane_id).is_none_or(|current| {
                current.state == AgentRunState::Unknown
                    && labels_compatible(&current.label, &status.label)
            });
            if should_overlay {
                current.agent_states.insert(*pane_id, status.clone());
            }
        }
    }

    /// Toggle the sidebar between its slim rail and an expanded width by
    /// resizing our *own* pane. Resizing only shifts the split between the
    /// sidebar and the content area beside it — content panes keep their
    /// arrangement (unlike a swap layout, which re-fits everything). Direction
    /// follows the stored sidebar mode so every render section shares the same
    /// open/closed state.
    fn toggle_width(&mut self) {
        self.sidebar_mode = self.sidebar_mode.toggled();
        self.sidebar_state_updated_at_millis = now_millis();
        self.default_width_applied = true;
        self.set_width_for_mode(self.sidebar_mode);
    }

    /// Adopt the newest sidebar mode published by any live session. Sessions
    /// republish adopted states, so a single toggle eventually propagates to
    /// every flock sidebar through the existing session-list refresh loop.
    fn sync_sidebar_mode_from_sessions(&mut self) -> bool {
        let Some(sidebar_state) = self
            .sessions
            .iter()
            .filter_map(|session| session.flock_sidebar_state)
            .max_by_key(|state| state.updated_at_millis)
        else {
            return false;
        };
        if sidebar_state.updated_at_millis <= self.sidebar_state_updated_at_millis {
            return false;
        }
        self.sidebar_state_updated_at_millis = sidebar_state.updated_at_millis;
        let mode = SidebarMode::from(sidebar_state.mode);
        if mode == self.sidebar_mode {
            return false;
        }
        self.sidebar_mode = mode;
        self.default_width_applied = true;
        self.set_width_for_mode(mode);
        true
    }

    /// Resize the sidebar to the target width for `mode` — a fixed column
    /// count, not a screen-relative percent, so it's the same on a laptop and
    /// an ultrawide. The expanded width is capped to half the tab on small
    /// terminals so the sidebar never crowds out the content.
    fn set_width_for_mode(&self, mode: SidebarMode) {
        let (_, total) = self.sidebar_and_tab_cols();
        let target = match mode {
            SidebarMode::Open => SIDEBAR_EXPANDED_COLS
                .min(total / 2)
                .max(WIDTH_EXPAND_THRESHOLD),
            SidebarMode::Closed => SIDEBAR_SLIM_COLS,
        };
        self.set_width(target);
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
        self.sidebar_mode = SidebarMode::Open;
        self.set_width_for_mode(SidebarMode::Open);
    }

    /// Set our own pane to an exact `target` column width. Uses the fixed-width
    /// resize so the sidebar lands on precisely `target` columns on any screen —
    /// the increment resize can't do this, as it steps by ~5% of the screen and
    /// won't shrink a percent-sized pane below 5% of the screen width (which is
    /// many columns on an ultrawide, leaving the slim rail far too wide).
    fn set_width(&self, target: usize) {
        let own = PaneId::Plugin(self.own_plugin_id);
        resize_pane_id_to_fixed_width(own, target as u32);
    }

    /// The sidebar's current width and the active tab's total width (cols), read
    /// from the pane manifest. Falls back to the last render width if the
    /// manifest geometry isn't available yet.
    fn sidebar_and_tab_cols(&self) -> (usize, usize) {
        let active = self
            .tabs
            .iter()
            .find(|tab| tab.active)
            .map(|tab| tab.position);
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
            Some(Target::RemotePane(pane_id)) => {
                if let Some(identifier) = self.current_coder_workspace() {
                    let argv = coder::focus_argv(&identifier, &pane_id);
                    let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
                    run_command(&refs, BTreeMap::new());
                }
            },
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
                agent_label,
                state,
            } => {
                let entry = self.agents.entry(pane_id).or_default();
                entry.set_hook_authority(agent_label, state, now)
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
        if self.current_coder_workspace().is_some() {
            if let Some(snapshot) = &self.coder_snapshot {
                let stale = self.coder_snapshot_stale(Instant::now());
                for (index, pane) in snapshot.panes.iter().enumerate() {
                    let mut status = pane.status.clone();
                    if stale {
                        status.state = AgentRunState::Unknown;
                        status.label = format!("{} · offline", status.label);
                        status.seen = true;
                    }
                    states.insert(PaneId::Plugin(u32::MAX - index as u32), status);
                }
            }
        } else {
            for (pane_id, st) in &self.agents {
                if !st.is_agent() {
                    continue;
                }
                states.insert(*pane_id, self.status_to_publish(pane_id, st));
            }
        }
        if states != self.last_published {
            self.last_published = states.clone();
            publish_agent_state(states);
        }
    }

    /// Publish this session's sidebar presentation state when it changes, so
    /// other sessions' sidebars can adopt the newest toggle.
    fn publish_sidebar_state_if_changed(&mut self) {
        if !self.permissions_granted {
            return;
        }
        let state = FlockSidebarState {
            mode: self.sidebar_mode.into(),
            updated_at_millis: self.sidebar_state_updated_at_millis,
        };
        if Some(state) != self.last_published_sidebar_state {
            self.last_published_sidebar_state = Some(state);
            publish_flock_sidebar_state(state);
        }
    }

    fn status_to_publish(&self, pane_id: &PaneId, st: &PaneAgentState) -> PaneAgentStatus {
        let next = PaneAgentStatus {
            state: to_run_state(st.state),
            label: st.effective_agent_label().unwrap_or_default(),
            seen: st.seen,
        };
        if next.state == AgentRunState::Unknown {
            if let Some(previous) = self.last_published.get(pane_id) {
                if previous.state != AgentRunState::Unknown
                    && labels_compatible(&next.label, &previous.label)
                {
                    return previous.clone();
                }
            }
        }
        next
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

    /// Reconcile tracked agent identities with the panes currently present in
    /// the manifest. `CommandChanged` remains the main live path, but it is not
    /// replayed just because a client switches back to an existing session. This
    /// pass lets an already-running agent reappear after reconnect/re-attach by
    /// asking the host for the current foreground command, falling back to the
    /// layout command stored in `PaneInfo`.
    fn sync_agents_from_manifest(&mut self, now: Instant) -> bool {
        if !self.permissions_granted {
            return false;
        }
        self.last_agent_command_sync = Some(now);
        let panes: Vec<(PaneId, Option<String>)> = self
            .panes
            .panes
            .values()
            .flatten()
            .filter(|pane| !pane.is_plugin)
            .map(|pane| (PaneId::Terminal(pane.id), pane.terminal_command.clone()))
            .filter(|(pane_id, _)| !self.command_synced.contains(pane_id))
            .collect();

        let mut changed = false;
        for (pane_id, terminal_command) in panes {
            let command = get_pane_running_command(pane_id)
                .ok()
                .filter(|command| !command.is_empty())
                .or_else(|| {
                    terminal_command
                        .as_deref()
                        .map(argv_from_terminal_command)
                        .filter(|command| !command.is_empty())
                });
            // One answer (even "no command") is enough: later foreground
            // changes arrive as CommandChanged events.
            self.command_synced.insert(pane_id);
            let Some(command) = command else {
                continue;
            };
            if self.seed_agent_command(pane_id, &command, now) {
                changed = true;
            }
        }
        changed
    }

    /// Apply a live `CommandChanged` event to the pane's tracked agent state.
    /// Returns whether a repaint is needed.
    fn apply_command_changed(
        &mut self,
        pane_id: PaneId,
        command: &[String],
        is_foreground: bool,
        now: Instant,
    ) -> bool {
        // A live event is authoritative — no need for the manifest sync to
        // ever host-query this pane.
        self.command_synced.insert(pane_id);
        if is_foreground {
            // The foreground command is the program actually running in the
            // pane; only it determines the agent.
            let agent = identify_agent_from_command(command);
            let remote_transport = agent.is_none() && self.command_is_remote_transport(command);
            let entry = self.agents.entry(pane_id).or_default();
            if agent.is_none() && (remote_transport || entry.remote) {
                // A remote transport's local argv (`gh codespace ssh …`, or an
                // ssh child of it) carries no agent identity — identification
                // and release are screen-driven (see `observe_pane_screen`),
                // so argv must neither set nor clear the agent here.
                entry.remote = true;
                return false;
            }
            if agent.is_some() {
                // A local agent took the pane's foreground over (e.g. run
                // directly inside a bound session) — argv identity wins again.
                entry.remote = false;
            }
            if agent.is_none() && entry.detected_agent.is_some() {
                // A non-agent foreground report while an agent is tracked can
                // be a transient scan miss: under a resident env wrapper
                // (devenv/nix develop) the host falls back to reporting the
                // wrapper process when the agent's line is missed. Confirm
                // through the same grace window as process exit instead of
                // clearing outright — a fresh agent report cancels it, and a
                // real exit (the wrapper's inner shell becomes the foreground
                // leader) expires the window and releases the agent.
                entry.mark_agent_missing(now);
                return false;
            }
            entry.set_detected_agent(agent, now)
        } else {
            // `is_foreground == false` means the pane's shell has no foreground
            // child at all (the host falls back to reporting the shell
            // command) — the agent process exited while the pane stayed open.
            // This is the only live signal for that transition, so open the
            // agent-missing grace window; the timer releases the agent unless
            // a fresh detection lands first (the host scan can transiently
            // miss a live process).
            if let Some(entry) = self.agents.get_mut(&pane_id) {
                entry.mark_agent_missing(now);
            }
            false
        }
    }

    fn seed_agent_command(&mut self, pane_id: PaneId, command: &[String], now: Instant) -> bool {
        let Some(agent) = identify_agent_from_command(command) else {
            // A remote transport snapshot marks the pane remote so screen
            // identification takes over; any other non-agent snapshot is
            // ignored (it must not clear an already-tracked agent either).
            if self.command_is_remote_transport(command) {
                self.agents.entry(pane_id).or_default().remote = true;
            }
            return false;
        };
        self.agents
            .entry(pane_id)
            .or_default()
            .set_detected_agent(Some(agent), now)
    }

    /// Whether a pane's argv is a remote-transport command rather than a local
    /// program: a remote binding itself (codespace SSH or the devcontainer
    /// wrapper), or — inside a bound session, where every default pane is the
    /// transport (possibly reported with rewritten argv: an ssh child, or the
    /// devcontainer CLI's node process after the wrapper's `exec`) — any
    /// command that isn't a recognized agent.
    fn command_is_remote_transport(&self, command: &[String]) -> bool {
        self.parse_enabled_remote_binding(command).is_some()
            || (self.current_session_is_bound() && identify_agent_from_command(command).is_none())
    }

    fn parse_enabled_remote_binding(&self, argv: &[String]) -> Option<RemoteBinding> {
        match parse_remote_binding(argv) {
            Some(RemoteBinding::Codespace) if self.sessionizer.codespaces_enabled() => {
                Some(RemoteBinding::Codespace)
            },
            Some(RemoteBinding::Devcontainer) if self.sessionizer.devcontainers_enabled() => {
                Some(RemoteBinding::Devcontainer)
            },
            Some(RemoteBinding::Coder) if self.sessionizer.coder_enabled() => {
                Some(RemoteBinding::Coder)
            },
            _ => None,
        }
    }

    /// Whether the session this sidebar runs in is remote-bound (its
    /// `default_command` carries the codespace SSH or devcontainer binding).
    fn current_session_is_bound(&self) -> bool {
        self.sessions.iter().any(|session| {
            session.is_current_session
                && session
                    .default_command
                    .as_deref()
                    .and_then(|command| self.parse_enabled_remote_binding(command))
                    .is_some()
        })
    }

    /// The workspace folder of this session's devcontainer binding, if the
    /// current session is devcontainer-bound.
    fn current_devcontainer_workspace(&self) -> Option<String> {
        if !self.sessionizer.devcontainers_enabled() {
            return None;
        }
        self.sessions
            .iter()
            .find(|session| session.is_current_session)
            .and_then(|session| {
                session
                    .default_command
                    .as_deref()
                    .and_then(devcontainer::parse_devcontainer_command)
                    .map(str::to_owned)
            })
    }

    /// In a devcontainer-bound session, periodically read the in-container
    /// hook files (see the bridge notes in [`devcontainer`]): resolve the
    /// container id by its devcontainer label once, then `docker exec … cat`
    /// on each poll. Results arrive as `RunCommandResult` events.
    fn maybe_poll_devcontainer_hooks(&mut self, now: Instant) {
        if !self.permissions_granted || !self.sessionizer.devcontainers_enabled() {
            return;
        }
        let Some(workspace) = self.current_devcontainer_workspace() else {
            return;
        };
        let due = self.last_devcontainer_hook_poll.is_none_or(|last| {
            now.duration_since(last).as_secs_f64() >= DEVCONTAINER_HOOK_POLL_SECS
        });
        if !due {
            return;
        }
        self.last_devcontainer_hook_poll = Some(now);
        match self.devcontainer_container_id.clone() {
            Some(id) => self.fire_devcontainer_hooks_read(&id),
            None => {
                let argv = devcontainer::ps_argv(&workspace);
                let argv_refs: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();
                run_command(
                    &argv_refs,
                    BTreeMap::from_iter([(
                        devcontainer::PS_CONTEXT_KEY.to_owned(),
                        workspace.clone(),
                    )]),
                );
            },
        }
    }

    /// Fire the `docker exec … cat` that dumps the container's hook files.
    fn fire_devcontainer_hooks_read(&self, container_id: &str) {
        let argv = devcontainer::hooks_cat_argv(container_id);
        let argv_refs: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();
        run_command(
            &argv_refs,
            BTreeMap::from_iter([(
                devcontainer::HOOKS_CONTEXT_KEY.to_owned(),
                container_id.to_owned(),
            )]),
        );
    }

    /// Apply the polled hook-file contents: each fresh line goes through the
    /// ordinary hook parser and lands as hook authority, exactly like a
    /// `zellij pipe` report. Lines for unknown panes (closed, or another
    /// scope's leftovers) and stale non-idle reports are dropped.
    fn apply_devcontainer_hook_lines(&mut self, stdout: &str) -> bool {
        let live: HashSet<PaneId> = self
            .panes
            .panes
            .values()
            .flatten()
            .filter(|pane| !pane.is_plugin)
            .map(|pane| PaneId::Terminal(pane.id))
            .collect();
        let now_epoch_secs = now_millis() / 1000;
        let mut changed = false;
        for args in devcontainer::parse_state_lines(stdout) {
            if !devcontainer::report_is_fresh(&args, now_epoch_secs) {
                continue;
            }
            let Ok(report) = parse_hook_report(&args) else {
                continue;
            };
            let pane_id = match &report {
                HookReport::State { pane_id, .. } => *pane_id,
                HookReport::Release { pane_id } => *pane_id,
            };
            if !live.contains(&pane_id) {
                continue;
            }
            if self.apply_hook_report(report) {
                changed = true;
            }
        }
        changed
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
        // Pane ids are reused; forgetting closed panes lets the manifest sync
        // re-query a fresh pane that takes over an old id.
        self.command_synced.retain(|pane_id| live.contains(pane_id));
    }
}

/// Which remote transport a session/pane binding uses. Both kinds behave the
/// same everywhere in the sidebar (remote flag, screen-driven identity, wider
/// gone-grace); the variant only picks the workspace-row badge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RemoteBinding {
    Codespace,
    Devcontainer,
    Coder,
}

/// Recognize any remote binding in an argv (a session's `default_command` or
/// a pane's command).
pub(crate) fn parse_remote_binding(argv: &[String]) -> Option<RemoteBinding> {
    if codespace::parse_codespace_ssh(argv).is_some() {
        return Some(RemoteBinding::Codespace);
    }
    if devcontainer::parse_devcontainer_command(argv).is_some() {
        return Some(RemoteBinding::Devcontainer);
    }
    if coder::parse_coder_ssh(argv).is_some() {
        return Some(RemoteBinding::Coder);
    }
    None
}

fn argv_from_terminal_command(command: &str) -> Vec<String> {
    command.split_whitespace().map(String::from).collect()
}

fn labels_compatible(left: &str, right: &str) -> bool {
    left.is_empty() || right.is_empty() || left == right
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

fn run_state_to_detected(state: AgentRunState) -> AgentState {
    match state {
        AgentRunState::Idle => AgentState::Idle,
        AgentRunState::Working => AgentState::Working,
        AgentRunState::Blocked => AgentState::Blocked,
        AgentRunState::Unknown => AgentState::Unknown,
    }
}

fn pane_id_string(pane_id: PaneId) -> String {
    match pane_id {
        PaneId::Terminal(id) => format!("terminal_{id}"),
        PaneId::Plugin(id) => format!("plugin_{id}"),
    }
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::Agent;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    fn state_with_provider(key: &str) -> State {
        let configuration = BTreeMap::from_iter([(key.to_owned(), "true".to_owned())]);
        State {
            sessionizer: SessionizerConfig::from_args(&configuration),
            ..State::default()
        }
    }

    #[test]
    fn disabled_remote_bindings_are_not_recognized_or_badged() {
        let mut state = State::default();
        let command = argv(&["coder", "ssh", "alice/api"]);
        let pane_id = PaneId::Terminal(3);
        state.apply_command_changed(pane_id, &command, true, Instant::now());
        assert!(!state.agents.get(&pane_id).unwrap().remote);

        let mut session = SessionInfo::new("api".into());
        session.default_command = Some(command);
        state.sessions = vec![session];
        let visible = state.visible_sessions();
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].default_command, None);
    }

    #[test]
    fn enabled_coder_binding_uses_remote_screen_detection() {
        let mut state = state_with_provider("coder_enabled");
        let pane_id = PaneId::Terminal(3);
        state.apply_command_changed(
            pane_id,
            &argv(&["coder", "ssh", "alice/api"]),
            true,
            Instant::now(),
        );
        assert!(state.agents.get(&pane_id).unwrap().remote);
    }

    #[test]
    fn disabled_devcontainer_binding_cannot_start_polling() {
        let mut state = State::default();
        let mut session = SessionInfo::new("api".into());
        session.is_current_session = true;
        session.default_command = Some(devcontainer_binding("/work/api"));
        state.sessions = vec![session];
        assert_eq!(state.current_devcontainer_workspace(), None);
    }

    #[test]
    fn transport_argv_marks_pane_remote_without_agent() {
        let mut state = state_with_provider("codespaces_enabled");
        let now = Instant::now();
        let pane_id = PaneId::Terminal(3);

        let changed = state.apply_command_changed(
            pane_id,
            &argv(&["gh", "codespace", "ssh", "-c", "my-cs"]),
            true,
            now,
        );
        assert!(!changed);
        let entry = state.agents.get(&pane_id).unwrap();
        assert!(entry.remote);
        assert!(!entry.is_agent());
    }

    #[test]
    fn remote_pane_identifies_agent_from_screen_and_releases_on_absence() {
        let mut state = state_with_provider("codespaces_enabled");
        let now = Instant::now();
        let pane_id = PaneId::Terminal(3);
        state.apply_command_changed(
            pane_id,
            &argv(&["gh", "codespace", "ssh", "-c", "my-cs"]),
            true,
            now,
        );

        // Claude's chrome renders over the SSH transport → identified + tracked.
        let claude_screen = "✳ Simplifying…\n─────────\n❯ \n─────────";
        assert!(state.observe_pane_screen(pane_id, claude_screen, now));
        let entry = state.agents.get(&pane_id).unwrap();
        assert_eq!(entry.detected_agent, Some(Agent::Claude));
        assert_eq!(entry.state, AgentState::Working);

        // The chrome disappears (agent exited to the remote shell): the
        // absence window opens, and the remote grace releases the agent.
        state.observe_pane_screen(pane_id, "user@codespace:~/repo$ ", now);
        let entry = state.agents.get_mut(&pane_id).unwrap();
        assert!(entry.is_agent(), "still tracked inside the grace window");
        assert!(entry.tick(now + state::REMOTE_AGENT_GONE_GRACE));
        let entry = state.agents.get(&pane_id).unwrap();
        assert!(!entry.is_agent());
        assert!(
            entry.remote,
            "the transport stays remote for re-identification"
        );
    }

    #[test]
    fn remote_pane_constant_transport_argv_never_clears_identified_agent() {
        let mut state = state_with_provider("codespaces_enabled");
        let now = Instant::now();
        let pane_id = PaneId::Terminal(3);
        let transport = argv(&["gh", "codespace", "ssh", "-c", "my-cs"]);
        state.apply_command_changed(pane_id, &transport, true, now);
        let claude_screen = "some output\n─────────\n❯ \n─────────";
        state.observe_pane_screen(pane_id, claude_screen, now);
        assert_eq!(
            state.agents.get(&pane_id).and_then(|e| e.detected_agent),
            Some(Agent::Claude)
        );

        // The host keeps reporting the transport argv — that says nothing
        // about the remote agent and must not open the missing window.
        assert!(!state.apply_command_changed(pane_id, &transport, true, now));
        assert!(!state
            .agents
            .get_mut(&pane_id)
            .unwrap()
            .tick(now + state::REMOTE_AGENT_GONE_GRACE));
        assert_eq!(
            state.agents.get(&pane_id).and_then(|e| e.detected_agent),
            Some(Agent::Claude)
        );
    }

    #[test]
    fn local_agent_argv_wins_back_a_remote_pane() {
        let mut state = state_with_provider("codespaces_enabled");
        let now = Instant::now();
        let pane_id = PaneId::Terminal(3);
        state.apply_command_changed(
            pane_id,
            &argv(&["gh", "codespace", "ssh", "-c", "my-cs"]),
            true,
            now,
        );
        assert!(state.agents.get(&pane_id).unwrap().remote);

        // A local agent takes the foreground (e.g. run from a local shell
        // pane) — argv identity applies again and the remote flag drops.
        assert!(state.apply_command_changed(pane_id, &argv(&["claude"]), true, now));
        let entry = state.agents.get(&pane_id).unwrap();
        assert!(!entry.remote);
        assert_eq!(entry.detected_agent, Some(Agent::Claude));
    }

    #[test]
    fn seed_marks_transport_snapshot_remote() {
        let mut state = state_with_provider("codespaces_enabled");
        let now = Instant::now();
        let pane_id = PaneId::Terminal(9);
        assert!(!state.seed_agent_command(
            pane_id,
            &argv(&["gh", "codespace", "ssh", "-c", "my-cs"]),
            now
        ));
        assert!(state.agents.get(&pane_id).unwrap().remote);
    }

    fn devcontainer_binding(path: &str) -> Vec<String> {
        argv(&[
            "sh",
            "-c",
            devcontainer::WRAPPER_SCRIPT,
            devcontainer::WRAPPER_ARG0,
            path,
        ])
    }

    #[test]
    fn devcontainer_wrapper_argv_marks_pane_remote_without_agent() {
        let mut state = state_with_provider("devcontainers_enabled");
        let now = Instant::now();
        let pane_id = PaneId::Terminal(3);

        let changed =
            state.apply_command_changed(pane_id, &devcontainer_binding("/work/app"), true, now);
        assert!(!changed);
        let entry = state.agents.get(&pane_id).unwrap();
        assert!(entry.remote);
        assert!(!entry.is_agent());
    }

    /// After the wrapper's `exec`, the host process walk reports the
    /// devcontainer CLI's node argv — not the binding shape — so the pane must
    /// be marked remote through the bound-*session* branch (the session's
    /// `default_command` still carries the wrapper).
    #[test]
    fn session_bound_devcontainer_marks_rewritten_node_argv_remote() {
        let mut state = state_with_provider("devcontainers_enabled");
        let now = Instant::now();
        let mut session = SessionInfo::new("app".to_string());
        session.is_current_session = true;
        session.default_command = Some(devcontainer_binding("/work/app"));
        state.sessions = vec![session];

        let pane_id = PaneId::Terminal(4);
        let node_argv = argv(&[
            "node",
            "/usr/local/lib/node_modules/@devcontainers/cli/devcontainer.js",
            "exec",
            "--workspace-folder",
            "/work/app",
        ]);
        assert!(!state.apply_command_changed(pane_id, &node_argv, true, now));
        let entry = state.agents.get(&pane_id).unwrap();
        assert!(entry.remote);
        assert!(!entry.is_agent());
    }

    #[test]
    fn seed_marks_devcontainer_transport_snapshot_remote() {
        let mut state = state_with_provider("devcontainers_enabled");
        let now = Instant::now();
        let pane_id = PaneId::Terminal(9);
        assert!(!state.seed_agent_command(pane_id, &devcontainer_binding("/work/app"), now));
        assert!(state.agents.get(&pane_id).unwrap().remote);
    }

    /// Polled hook-file lines act exactly like pipe reports: applied to live
    /// panes, dropped for unknown panes and for stale non-idle reports.
    #[test]
    fn devcontainer_hook_lines_apply_to_live_panes_only() {
        let mut state = State::default();
        state.panes.panes.insert(
            0,
            vec![
                PaneInfo {
                    id: 3,
                    is_plugin: false,
                    ..Default::default()
                },
                PaneInfo {
                    id: 5,
                    is_plugin: false,
                    ..Default::default()
                },
            ],
        );
        let now_epoch = now_millis() / 1000;
        let stdout = format!(
            "pane_id=3,state=blocked,agent=opencode,source=flock:opencode,ts={fresh}\n\
             pane_id=5,state=working,agent=opencode,ts={stale}\n\
             pane_id=42,state=working,agent=opencode,ts={fresh}\n",
            fresh = now_epoch,
            stale = now_epoch.saturating_sub(devcontainer::HOOK_STALE_SECS + 60),
        );

        assert!(state.apply_devcontainer_hook_lines(&stdout));
        // Pane 3: fresh report applied as hook authority.
        let entry = state.agents.get(&PaneId::Terminal(3)).unwrap();
        assert!(entry.is_agent());
        assert_eq!(entry.state, AgentState::Blocked);
        // Pane 5: stale "working" dropped — no entry materializes.
        assert!(state.agents.get(&PaneId::Terminal(5)).is_none());
        // Pane 42 isn't in the manifest — dropped.
        assert!(state.agents.get(&PaneId::Terminal(42)).is_none());
    }

    #[test]
    fn seed_agent_command_recreates_missing_agent_entry() {
        let mut state = State::default();
        let now = Instant::now();
        let pane_id = PaneId::Terminal(7);
        let command = vec!["/opt/homebrew/bin/codex".to_string()];

        assert!(state.seed_agent_command(pane_id, &command, now));
        assert_eq!(
            state
                .agents
                .get(&pane_id)
                .and_then(|pane| pane.detected_agent),
            Some(Agent::Codex)
        );
    }

    #[test]
    fn seed_agent_command_does_not_clear_existing_agent_on_plain_shell_snapshot() {
        let mut state = State::default();
        let now = Instant::now();
        let pane_id = PaneId::Terminal(7);
        state.seed_agent_command(pane_id, &["codex".to_string()], now);

        assert!(!state.seed_agent_command(pane_id, &["zsh".to_string()], now));
        assert_eq!(
            state
                .agents
                .get(&pane_id)
                .and_then(|pane| pane.detected_agent),
            Some(Agent::Codex)
        );
        assert!(state
            .agents
            .get(&pane_id)
            .is_some_and(|pane| pane.is_agent()));
    }

    #[test]
    fn foreground_exit_releases_agent_after_grace() {
        let mut state = State::default();
        let now = Instant::now();
        let pane_id = PaneId::Terminal(7);

        assert!(state.apply_command_changed(
            pane_id,
            &["/opt/homebrew/bin/claude".to_string()],
            true,
            now
        ));
        assert!(state
            .agents
            .get(&pane_id)
            .is_some_and(|pane| pane.is_agent()));

        // Claude exits: the host reports the shell itself, no foreground child.
        assert!(!state.apply_command_changed(
            pane_id,
            &["/opt/homebrew/bin/fish".to_string()],
            false,
            now
        ));
        // Still shown inside the grace window (the scan may have missed).
        assert!(state
            .agents
            .get(&pane_id)
            .is_some_and(|pane| pane.is_agent()));

        // The timer tick past the grace window releases the agent.
        let entry = state.agents.get_mut(&pane_id).expect("tracked pane");
        assert!(entry.tick(now + crate::state::AGENT_GONE_GRACE));
        assert!(!entry.is_agent());
    }

    #[test]
    fn non_agent_foreground_report_opens_grace_for_detected_agent() {
        use std::time::Duration;
        let mut state = State::default();
        let now = Instant::now();
        let pane_id = PaneId::Terminal(7);
        assert!(state.apply_command_changed(pane_id, &["claude".to_string()], true, now));

        // The host transiently reports the resident devenv wrapper instead of
        // the agent (a scan miss) — the agent must survive the grace window.
        assert!(!state.apply_command_changed(
            pane_id,
            &["devenv".to_string(), "shell".to_string()],
            true,
            now
        ));
        assert!(state
            .agents
            .get(&pane_id)
            .is_some_and(|pane| pane.is_agent()));

        // A fresh agent report cancels the pending release.
        state.apply_command_changed(
            pane_id,
            &["claude".to_string()],
            true,
            now + Duration::from_secs(1),
        );
        let entry = state.agents.get_mut(&pane_id).expect("tracked pane");
        assert!(!entry.tick(now + crate::state::AGENT_GONE_GRACE + Duration::from_secs(1)));
        assert!(entry.is_agent());
    }

    #[test]
    fn non_agent_foreground_report_releases_agent_after_grace() {
        let mut state = State::default();
        let now = Instant::now();
        let pane_id = PaneId::Terminal(7);
        state.apply_command_changed(pane_id, &["claude".to_string()], true, now);

        // Claude exits inside the devenv shell: the wrapper's inner shell is
        // now the foreground leader, so the host keeps reporting a foreground
        // command that is not an agent.
        assert!(!state.apply_command_changed(pane_id, &["bash".to_string()], true, now));

        let entry = state.agents.get_mut(&pane_id).expect("tracked pane");
        assert!(entry.tick(now + crate::state::AGENT_GONE_GRACE));
        assert!(!entry.is_agent());
    }

    #[test]
    fn non_agent_foreground_report_keeps_hook_only_agent() {
        let mut state = State::default();
        let now = Instant::now();
        let pane_id = PaneId::Terminal(7);
        state.agents.entry(pane_id).or_default().set_hook_authority(
            "custom-agent".into(),
            AgentState::Working,
            now,
        );

        // A hook-only agent has no detected process identity; an unrelated
        // foreground command must not open the release window for it.
        assert!(!state.apply_command_changed(pane_id, &["vim".to_string()], true, now));

        let entry = state.agents.get_mut(&pane_id).expect("tracked pane");
        assert!(!entry.tick(now + crate::state::AGENT_GONE_GRACE));
        assert!(entry.is_agent());
    }

    #[test]
    fn foreground_exit_for_untracked_pane_is_ignored() {
        let mut state = State::default();
        let now = Instant::now();
        let pane_id = PaneId::Terminal(9);

        // A shell-only pane reporting "no foreground child" must not create
        // an agent entry (or crash) — it was never tracked.
        assert!(!state.apply_command_changed(pane_id, &["/bin/zsh".to_string()], false, now));
        assert!(!state.agents.contains_key(&pane_id));
        // But the answer still counts as synced: no host re-query needed.
        assert!(state.command_synced.contains(&pane_id));
    }

    #[test]
    fn terminal_command_string_can_seed_agent_identity() {
        let argv =
            argv_from_terminal_command("/opt/homebrew/bin/claude --dangerously-skip-permissions");

        assert_eq!(identify_agent_from_command(&argv), Some(Agent::Claude));
    }

    #[test]
    fn publish_preserves_previous_state_during_unknown_warmup() {
        let mut state = State::default();
        let pane_id = PaneId::Terminal(7);
        state.last_published.insert(
            pane_id,
            PaneAgentStatus {
                state: AgentRunState::Idle,
                label: "codex".to_owned(),
                seen: true,
            },
        );
        let mut pane = PaneAgentState::new();
        pane.detected_agent = Some(Agent::Codex);
        pane.state = AgentState::Unknown;

        let status = state.status_to_publish(&pane_id, &pane);

        assert_eq!(status.state, AgentRunState::Idle);
        assert_eq!(status.label, "codex");
        assert!(status.seen);
    }

    #[test]
    fn render_sessions_overlay_last_published_current_state() {
        let mut state = State::default();
        let pane_id = PaneId::Terminal(7);
        let mut current = SessionInfo::new("workspace-a".to_string());
        current.is_current_session = true;
        state.sessions = vec![current];
        state.last_published.insert(
            pane_id,
            PaneAgentStatus {
                state: AgentRunState::Idle,
                label: "codex".to_owned(),
                seen: true,
            },
        );

        let sessions = state.render_sessions();

        assert_eq!(
            sessions[0]
                .agent_states
                .get(&pane_id)
                .map(|status| status.state),
            Some(AgentRunState::Idle)
        );
    }
}
