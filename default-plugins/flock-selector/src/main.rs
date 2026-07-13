//! flock-selector — a frecency-ranked project launcher for Zellij.
//!
//! A standalone, floating picker that lists every configured project and
//! fuzzy-filters them in a reverse layout (input on the bottom, best match just
//! above it). It is a drop-in replacement for `laperlej/zellij-sessionizer`:
//! it reads the same KDL args (`individual_dirs`, `root_dirs`, `session_layout`,
//! `cwd` — see [`config`]) so the user's existing nix options feed it unchanged,
//! and ships a bundled `flock-selector` layout that floats it on a cold shell
//! (for a `zf = "zellij --layout flock-selector"` alias) as well as supporting a
//! user-bound `LaunchOrFocusPlugin "flock-selector"` keybind.
//!
//! Phase 8 delivers discovery + the fuzzy UI:
//! - **Discovery** ([`discovery`]): each `individual_dirs` entry is a project;
//!   each `root_dirs` entry is scanned one level deep (via the plugin host's
//!   `run_command`) and its immediate subdirectories become projects. The merged,
//!   de-duplicated set refreshes on a `Timer`.
//! - **Ranking** ([`ranking`] + [`fuzzy`] + [`frecency`]): a fuzzy score over the
//!   basename and the home-shortened path, blended with a zoxide-style frecency
//!   signal persisted to the plugin's `/data` dir.
//! - **UI** ([`ui`]): the reverse-layout picker, with matched ranges highlighted
//!   and projects that already have a live session badged (matched against the
//!   Phase 6 `SessionInfo.workspace_root`).
//!
//! Phase 9 wires confirmation: pressing `Enter` resolves the chosen project into
//! a switch-or-create action ([`session`]) — attach to the session already
//! rooted at that folder, or launch a new one there with the configured
//! `session_layout` — and bumps the frecency db.

mod codespaces;
mod config;
mod devcontainers;
mod discovery;
mod frecency;
mod fuzzy;
mod live_sessions;
mod palette;
mod ranking;
mod session;
mod ui;

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use codespaces::{Codespace, GhError};
use config::SelectorConfig;
use devcontainers::{DevcontainerPhase, PendingDevcontainer};
use discovery::{
    merge_candidates, parse_scan_output, scan_argv, scan_context, shorten_home, Project,
    SCAN_CONTEXT_KEY,
};
use frecency::{now_secs, FrecencyDb};
use live_sessions::SessionEntry;
use palette::Theme;
use session::{ExistingSession, OpenAction};
use ui::PickerMode;
use zellij_tile::prelude::*;

/// How often to re-scan the root dirs so newly-created project folders surface
/// without reopening the picker.
const REFRESH_SECS: f64 = 10.0;

/// Refresh the codespace list every this-many refresh ticks (× [`REFRESH_SECS`]
/// = ~30s) while the Codespaces mode is showing — `gh codespace list` is a
/// network call, so it shouldn't ride the 10s project-scan cadence.
const CODESPACE_REFRESH_TICKS: u8 = 3;

/// Session name used by the cold-shell entry layout (its `session_name` arg).
/// Sessions with this name are selector throwaways, not project sessions: their
/// `workspace_root` is whatever folder zellij happened to be launched from, so
/// they must never be treated as "the session rooted at" that folder. Must
/// match `HIDDEN_SESSION_NAME` in `flock-sidebar/src/main.rs`, which hides the
/// same session from the sidebar's workspace list.
const SELECTOR_SESSION_NAME: &str = "flock-selector";

#[derive(Default)]
struct State {
    /// Granted once our permission request resolves; until then we can't scan or
    /// read the session list, so we render a hint.
    permissions_granted: bool,
    /// Parsed plugin args (folder sources, session layout, cwd base).
    config: SelectorConfig,
    /// Latest scan results per root dir (its immediate subdirectories).
    scanned: BTreeMap<String, Vec<PathBuf>>,
    /// The merged, de-duplicated candidate set (individual dirs + scanned subdirs).
    projects: Vec<Project>,
    /// Persisted usage db informing frecency ranking.
    frecency: FrecencyDb,
    /// Latest cross-session list, for the open-session badge (matched against
    /// each `SessionInfo.workspace_root`).
    sessions: Vec<SessionInfo>,
    /// The typed query.
    query: String,
    /// Selection cursor: index into the ranked results (0 = best, bottom-most).
    selected: usize,
    /// Scroll offset (index of the bottom-most visible result).
    scroll: usize,
    /// Picker colors, resolved from the active zellij theme.
    palette: Theme,
    /// Row → result-index map from the last frame, for mouse hit-testing.
    row_map: Vec<(usize, usize)>,
    /// Whether the refresh timer is currently armed.
    timer_running: bool,
    /// Which list is showing (Tab cycles Sessions → Projects → Codespaces).
    mode: PickerMode,
    /// The latest codespace list — the `/data` cache at load, then live `gh`
    /// results as they land.
    codespaces: Vec<Codespace>,
    /// The latest `gh` failure, surfaced as a hint line in Codespaces mode.
    codespaces_error: Option<GhError>,
    /// Whether a live `gh codespace list` is in flight.
    codespaces_refreshing: bool,
    /// The codespace a `gh codespace stop` is in flight for, if any.
    pending_stop: Option<String>,
    /// Ticks left until the next codespace list refresh (see
    /// [`CODESPACE_REFRESH_TICKS`]).
    codespace_refresh_ticks: u8,
    /// The user's codespace layout base (the `codespace_session_layout` file's
    /// content), once read off the host. `None` (unset, unreadable, or not yet
    /// loaded) falls back to the built-in flock chrome mirror. Devcontainer
    /// sessions share this base — both bindings want the same chrome.
    codespace_layout_base: Option<String>,
    /// Projects whose folder carries a `.devcontainer` marker, per scan scope
    /// (see [`devcontainers::SCAN_CONTEXT_KEY`]), so the Enter-time prompt
    /// check is a set lookup.
    devcontainer_projects: BTreeMap<String, HashSet<PathBuf>>,
    /// The devcontainer prompt/up currently owning the picker's keyboard.
    pending_devcontainer: Option<PendingDevcontainer>,
}

register_plugin!(State);

impl ZellijPlugin for State {
    fn load(&mut self, configuration: BTreeMap<String, String>) {
        self.config = SelectorConfig::from_args(&configuration);
        // When launched as the cold-shell entry point, the layout passes a fixed
        // `session_name` so the picker's throwaway session always carries the
        // same stable name (which the sidebar hides) rather than a random one. A
        // keybind launch omits it, so we never rename the user's working session.
        if let Some(name) = &self.config.session_name {
            rename_session(name);
        }
        self.frecency = FrecencyDb::load();
        // The cached list renders the Codespaces mode instantly; a live
        // refresh replaces it once permissions land.
        self.codespaces = codespaces::load_cache();
        // Individual dirs are projects directly, so show them immediately; a
        // root scan fills in the subdirectories once permissions land.
        self.rebuild_projects();

        // - ReadApplicationState: the cross-session list for the open badge.
        // - RunCommands: scanning root dirs (`find` one level deep).
        // - ChangeApplicationState: switch/create sessions on confirm (Phase 9;
        //   requested now so confirming later doesn't trigger a fresh prompt).
        request_permission(&[
            PermissionType::ReadApplicationState,
            PermissionType::RunCommands,
            PermissionType::ChangeApplicationState,
        ]);

        subscribe(&[
            EventType::ModeUpdate,
            EventType::SessionUpdate,
            EventType::Key,
            EventType::RunCommandResult,
            EventType::Timer,
            EventType::PermissionRequestResult,
        ]);

        rename_plugin_pane(get_plugin_ids().plugin_id, "Project Selector");
    }

    fn update(&mut self, event: Event) -> bool {
        let mut should_render = false;
        match event {
            Event::PermissionRequestResult(result) => {
                self.permissions_granted = matches!(result, PermissionStatus::Granted);
                if self.permissions_granted {
                    self.fire_scans();
                    self.fire_devcontainer_scans();
                    // Pre-warm the codespace list so Tab-ing over doesn't wait
                    // on a cold `gh` network call.
                    self.fire_codespace_list();
                    self.fire_codespace_layout_read();
                    self.arm_refresh_timer();
                }
                should_render = true;
            },
            Event::ModeUpdate(mode_info) => {
                self.palette = Theme::from_style(&mode_info.style);
                should_render = true;
            },
            Event::SessionUpdate(session_infos, _resurrectable) => {
                self.sessions = session_infos;
                should_render = true;
            },
            Event::Timer(_) => {
                self.timer_running = false;
                if self.permissions_granted {
                    self.fire_scans();
                    self.fire_devcontainer_scans();
                    // The codespace list refreshes on a slower cadence, and
                    // only while its mode is showing.
                    if self.mode == PickerMode::Codespaces {
                        self.codespace_refresh_ticks =
                            self.codespace_refresh_ticks.saturating_sub(1);
                        if self.codespace_refresh_ticks == 0 {
                            self.fire_codespace_list();
                        }
                    }
                    self.arm_refresh_timer();
                }
            },
            Event::RunCommandResult(exit_code, stdout, stderr, context) => {
                if let Some(root) = context.get(SCAN_CONTEXT_KEY) {
                    if exit_code == Some(0) {
                        let subdirs = parse_scan_output(&String::from_utf8_lossy(&stdout));
                        self.scanned.insert(root.clone(), subdirs);
                    } else {
                        // A missing/inaccessible root contributes nothing.
                        self.scanned.insert(root.clone(), Vec::new());
                    }
                    self.rebuild_projects();
                    should_render = true;
                } else if context.contains_key(codespaces::LIST_CONTEXT_KEY) {
                    self.codespaces_refreshing = false;
                    if exit_code == Some(0) {
                        match codespaces::parse_list_json(&String::from_utf8_lossy(&stdout)) {
                            Ok(list) => {
                                self.codespaces = list;
                                self.codespaces_error = None;
                                codespaces::save_cache(&self.codespaces);
                            },
                            Err(detail) => {
                                self.codespaces_error = Some(GhError::Other(detail));
                            },
                        }
                    } else {
                        self.codespaces_error = Some(codespaces::classify_error(
                            exit_code,
                            &String::from_utf8_lossy(&stderr),
                        ));
                    }
                    should_render = true;
                } else if context.contains_key(codespaces::LAYOUT_CONTEXT_KEY) {
                    // The user's codespace layout base. A failed read (missing
                    // file, bad path) just leaves the built-in mirror in place.
                    if exit_code == Some(0) {
                        let content = String::from_utf8_lossy(&stdout).to_string();
                        if !content.trim().is_empty() {
                            self.codespace_layout_base = Some(content);
                        }
                    }
                } else if context.contains_key(codespaces::STOP_CONTEXT_KEY) {
                    self.pending_stop = None;
                    if exit_code != Some(0) {
                        self.codespaces_error = Some(codespaces::classify_error(
                            exit_code,
                            &String::from_utf8_lossy(&stderr),
                        ));
                    }
                    // Whatever happened, re-list so the shown state reconciles
                    // with reality.
                    self.fire_codespace_list();
                    should_render = true;
                } else if let Some(scope) = context.get(devcontainers::SCAN_CONTEXT_KEY) {
                    // Parsed regardless of exit code: `find` over several start
                    // paths exits nonzero when any one is missing but still
                    // prints valid hits for the rest.
                    self.devcontainer_projects.insert(
                        scope.clone(),
                        devcontainers::parse_scan_output(&String::from_utf8_lossy(&stdout)),
                    );
                } else if let Some(path_str) = context.get(devcontainers::UP_CONTEXT_KEY) {
                    should_render = self.handle_devcontainer_up_result(
                        path_str,
                        exit_code,
                        &String::from_utf8_lossy(&stderr),
                    );
                }
            },
            Event::Key(key) => {
                should_render = self.handle_key(key);
            },
            _ => {},
        }
        should_render
    }

    fn render(&mut self, rows: usize, cols: usize) {
        let now = now_secs();
        let results = ranking::rank(&self.projects, &self.query, &self.frecency, now);
        let open_paths = self.open_session_paths();
        let session_entries = self.session_entries();
        let session_results = live_sessions::rank(&session_entries, &self.query);
        let codespace_results = codespaces::rank(&self.codespaces, &self.query);
        let bound_codespaces = self.bound_codespace_names();

        let output = ui::render(ui::RenderInput {
            permissions_granted: self.permissions_granted,
            configured: !self.config.individual_dirs.is_empty()
                || !self.config.root_dirs.is_empty(),
            query: &self.query,
            mode: self.mode,
            session_results: &session_results,
            results: &results,
            open_paths: &open_paths,
            codespace_results: &codespace_results,
            bound_codespaces: &bound_codespaces,
            codespaces_error: self.codespaces_error.as_ref(),
            codespaces_refreshing: self.codespaces_refreshing,
            pending_stop: self.pending_stop.as_deref(),
            pending_devcontainer: self.pending_devcontainer.as_ref(),
            palette: &self.palette,
            selected: self.selected,
            scroll: self.scroll,
            total_candidates: match self.mode {
                PickerMode::Sessions => session_entries.len(),
                PickerMode::Projects => self.projects.len(),
                PickerMode::Codespaces => self.codespaces.len(),
            },
            rows,
            cols,
        });
        self.selected = output.selected;
        self.scroll = output.scroll;
        self.row_map = output.row_map;
        print!("{}", output.ansi);
    }
}

impl State {
    /// Re-derive the candidate project set from the individual dirs plus the
    /// latest per-root scan results, and re-clamp the selection.
    fn rebuild_projects(&mut self) {
        self.projects = merge_candidates(&self.config.individual_dirs, &self.scanned);
        let max = self.projects.len().saturating_sub(1);
        if self.selected > max {
            self.selected = 0;
            self.scroll = 0;
        }
    }

    /// Kick off a one-level-deep scan of each configured root dir. Results arrive
    /// asynchronously as `RunCommandResult`s tagged with the root.
    fn fire_scans(&self) {
        for root in &self.config.root_dirs {
            let root = root.to_string_lossy().to_string();
            let argv = scan_argv(&root);
            let argv_refs: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();
            run_command(&argv_refs, scan_context(&root));
        }
    }

    /// (Re)arm the refresh timer if it isn't already running.
    fn arm_refresh_timer(&mut self) {
        if !self.timer_running {
            set_timeout(REFRESH_SECS);
            self.timer_running = true;
        }
    }

    /// Whether `session` is a selector entry session rather than a project
    /// session: the fixed cold-shell name (shared with the sidebar's hiding
    /// rule), whatever name this instance was configured to take, or — when we
    /// *are* the cold-shell entry (`session_name` set) — our own session even
    /// under a random name (the rename can fail if a previous throwaway still
    /// holds the fixed name).
    fn is_selector_session(&self, session: &SessionInfo) -> bool {
        session.name == SELECTOR_SESSION_NAME
            || Some(session.name.as_str()) == self.config.session_name.as_deref()
            || (self.config.session_name.is_some() && session.is_current_session)
    }

    /// Absolute paths of folders that currently have a live session, matched
    /// against each session's `workspace_root` (the Phase 6 fork field).
    /// Selector throwaway sessions don't count — their root is just the folder
    /// zellij was launched from.
    fn open_session_paths(&self) -> HashSet<String> {
        self.sessions
            .iter()
            .filter(|s| !self.is_selector_session(s))
            .map(|s| config::normalize(&s.workspace_root).to_string_lossy().to_string())
            .filter(|p| !p.is_empty())
            .collect()
    }

    fn handle_key(&mut self, key: KeyWithModifier) -> bool {
        // A pending devcontainer prompt/up owns the keyboard: nothing leaks
        // into the query or list navigation until it resolves.
        if self.pending_devcontainer.is_some() {
            return self.handle_devcontainer_key(key);
        }
        // Navigation: in the reverse layout the best result sits at the bottom
        // (selected = 0), so Up moves toward worse results (higher on screen) and
        // Down moves toward the best (just above the input).
        if key.has_no_modifiers() {
            match key.bare_key {
                BareKey::Up => {
                    self.select_worse();
                    return true;
                },
                BareKey::Down => {
                    self.select_better();
                    return true;
                },
                BareKey::Enter => {
                    self.confirm_selection();
                    return true;
                },
                BareKey::Tab => {
                    self.toggle_mode();
                    return true;
                },
                BareKey::Esc => {
                    close_self();
                    return false;
                },
                BareKey::Backspace => {
                    if self.query.pop().is_some() {
                        self.reset_selection();
                    }
                    return true;
                },
                BareKey::Char(c) => {
                    self.query.push(c);
                    self.reset_selection();
                    return true;
                },
                _ => return false,
            }
        }

        // Emacs-style editing/navigation that doesn't collide with typing.
        if key.has_modifiers(&[KeyModifier::Ctrl]) {
            match key.bare_key {
                BareKey::Char('n') => {
                    self.select_better();
                    return true;
                },
                BareKey::Char('p') => {
                    self.select_worse();
                    return true;
                },
                BareKey::Char('w') => {
                    self.delete_word();
                    self.reset_selection();
                    return true;
                },
                BareKey::Char('u') => {
                    self.query.clear();
                    self.reset_selection();
                    return true;
                },
                BareKey::Char('c') => {
                    close_self();
                    return false;
                },
                BareKey::Char('x') if self.mode == PickerMode::Codespaces => {
                    return self.stop_selected_codespace();
                },
                BareKey::Char('x') if self.mode == PickerMode::Sessions => {
                    return self.kill_selected_session();
                },
                _ => return false,
            }
        }
        false
    }

    /// How many candidates the active mode has (the render pass clamps the
    /// cursor to the *filtered* result count).
    fn candidate_count(&self) -> usize {
        match self.mode {
            PickerMode::Sessions => self.session_entries().len(),
            PickerMode::Projects => self.projects.len(),
            PickerMode::Codespaces => self.codespaces.len(),
        }
    }

    /// Move the cursor toward a worse (higher-on-screen) result.
    fn select_worse(&mut self) {
        let max = self.candidate_count().saturating_sub(1);
        if self.selected < max {
            self.selected += 1;
        }
    }

    /// Move the cursor toward the best (bottom-most) result.
    fn select_better(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    /// Reset the cursor to the best result after the query changes.
    fn reset_selection(&mut self) {
        self.selected = 0;
        self.scroll = 0;
    }

    /// Delete the trailing whitespace-delimited word from the query.
    fn delete_word(&mut self) {
        let trimmed = self.query.trim_end();
        match trimmed.rfind(char::is_whitespace) {
            Some(idx) => self.query.truncate(idx + 1),
            None => self.query.clear(),
        }
    }

    /// Cycle through the Sessions, Projects, and Codespaces lists, resetting
    /// the query and cursor (a filter typed against one list is meaningless on
    /// another).
    fn toggle_mode(&mut self) {
        self.mode = match self.mode {
            PickerMode::Sessions => PickerMode::Projects,
            PickerMode::Projects => PickerMode::Codespaces,
            PickerMode::Codespaces => PickerMode::Sessions,
        };
        self.query.clear();
        self.reset_selection();
        if self.mode == PickerMode::Codespaces && self.permissions_granted {
            self.fire_codespace_list();
        }
    }

    /// Kick off a `gh codespace list` refresh. The result arrives as a
    /// `RunCommandResult` tagged with [`codespaces::LIST_CONTEXT_KEY`].
    fn fire_codespace_list(&mut self) {
        let argv = codespaces::list_argv();
        let argv_refs: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();
        run_command(&argv_refs, codespaces::list_context());
        self.codespaces_refreshing = true;
        self.codespace_refresh_ticks = CODESPACE_REFRESH_TICKS;
    }

    /// Read the configured `codespace_session_layout` file off the host, if
    /// any. Local file read — lands well before a user can pick a codespace.
    fn fire_codespace_layout_read(&self) {
        let Some(path) = &self.config.codespace_session_layout else {
            return;
        };
        let argv = codespaces::layout_read_argv(path);
        let argv_refs: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();
        run_command(&argv_refs, codespaces::layout_read_context());
    }

    /// Codespace names that currently have a live bound session (recognized by
    /// each session's `default_command`), for the open badge.
    fn bound_codespace_names(&self) -> HashSet<String> {
        self.sessions
            .iter()
            .filter_map(|s| {
                s.default_command
                    .as_deref()
                    .and_then(codespaces::parse_codespace_ssh)
                    .map(str::to_owned)
            })
            .collect()
    }

    /// The live sessions reduced to what the codespace resolution needs: every
    /// name (taken for collision-avoidance) plus the parsed binding, if any.
    fn existing_codespace_sessions(&self) -> Vec<codespaces::ExistingSession> {
        self.sessions
            .iter()
            .map(|s| codespaces::ExistingSession {
                name: s.name.clone(),
                bound_codespace: s
                    .default_command
                    .as_deref()
                    .and_then(codespaces::parse_codespace_ssh)
                    .map(str::to_owned),
            })
            .collect()
    }

    /// The live session list reduced to Sessions-mode entries: selector
    /// throwaway sessions are excluded (switching into one strands the user in
    /// a pane-less shell), everything else lists with its home-shortened
    /// workspace root.
    fn session_entries(&self) -> Vec<SessionEntry> {
        self.sessions
            .iter()
            .filter(|s| !self.is_selector_session(s))
            .map(|s| SessionEntry {
                name: s.name.clone(),
                display_path: if s.workspace_root.as_os_str().is_empty() {
                    String::new()
                } else {
                    shorten_home(&config::normalize(&s.workspace_root))
                },
                is_current: s.is_current_session,
                activity: live_sessions::session_activity(&s.agent_states),
            })
            .collect()
    }

    /// Switch to the selected live session (a no-op beyond dismissing the
    /// picker when it is the session we're already in — the server refuses
    /// attaching to the same session). Then closes the picker.
    fn confirm_session_selection(&mut self) {
        let entries = self.session_entries();
        let ranked = live_sessions::rank(&entries, &self.query);
        let Some(entry) = ranked.get(self.selected).map(|r| r.entry) else {
            return;
        };

        if !entry.is_current {
            let current_session_name = self
                .sessions
                .iter()
                .find(|s| s.is_current_session)
                .map(|s| s.name.clone());
            switch_session(Some(&entry.name));
            self.discard_throwaway_session(current_session_name.as_deref());
        }
        close_self();
    }

    /// Close the selected live session (`Ctrl-x`, mirroring the Codespaces
    /// tab's stop chord). A background session is killed
    /// in place — the picker stays open and the next `SessionUpdate` drops the
    /// row. Killing the session this client is attached to would sever it
    /// mid-picker, so for the current session the client is first handed to
    /// the next listed session (the switch shim blocks until the handoff
    /// completes, same as [`Self::discard_throwaway_session`]), then the old
    /// session is killed — taking this picker pane with it. With no other
    /// session to land on, the key is a no-op.
    fn kill_selected_session(&mut self) -> bool {
        let entries = self.session_entries();
        let ranked = live_sessions::rank(&entries, &self.query);
        let Some(entry) = ranked.get(self.selected).map(|r| r.entry) else {
            return false;
        };

        if !entry.is_current {
            let _ = kill_sessions(&[entry.name.as_str()]);
            return true;
        }

        let Some(next) = ranked.iter().find(|r| !r.entry.is_current) else {
            return false;
        };
        switch_session(Some(&next.entry.name));
        let _ = kill_sessions(&[entry.name.as_str()]);
        true
    }

    /// Open the selected codespace: switch to its bound session if one is
    /// live, otherwise create one from a generated layout that binds every new
    /// pane/tab to `gh codespace ssh` (and disables serialization, so a dead
    /// bound session is never resurrectable). Then closes the picker.
    fn confirm_codespace_selection(&mut self) {
        let ranked = codespaces::rank(&self.codespaces, &self.query);
        let Some(codespace) = ranked.get(self.selected).map(|r| r.codespace.clone()) else {
            return;
        };

        let action = codespaces::resolve_open(&codespace, &self.existing_codespace_sessions());

        let current_session_name = self
            .sessions
            .iter()
            .find(|s| s.is_current_session)
            .map(|s| s.name.clone());

        match action {
            // Already in the bound session: switching would be refused by the
            // server ("Cannot attach to same session"), so just dismiss.
            codespaces::OpenAction::Switch { name }
                if Some(&name) == current_session_name.as_ref() => {},
            codespaces::OpenAction::Switch { name } => {
                switch_session(Some(&name));
                self.discard_throwaway_session(current_session_name.as_deref());
            },
            codespaces::OpenAction::Create { name } => {
                let layout = LayoutInfo::Stringified(codespaces::layout_doc_for(
                    &codespace.name,
                    &self.config.sidebar_args,
                    self.codespace_layout_base.as_deref(),
                ));
                switch_session_with_layout(Some(&name), layout, None);
                self.discard_throwaway_session(current_session_name.as_deref());
            },
        }
        close_self();
    }

    /// Stop the selected codespace (`Ctrl-x`): kill its bound live session (if
    /// any, and not the one we're running in), then fire `gh codespace stop`.
    /// The picker stays open; the row shows "stopping…" until the re-list
    /// reconciles.
    fn stop_selected_codespace(&mut self) -> bool {
        if self.pending_stop.is_some() {
            return false; // one stop at a time — the re-list will catch up
        }
        let ranked = codespaces::rank(&self.codespaces, &self.query);
        let Some(codespace) = ranked.get(self.selected).map(|r| r.codespace.clone()) else {
            return false;
        };

        if let Some(bound) = self.sessions.iter().find(|s| {
            s.default_command
                .as_deref()
                .and_then(codespaces::parse_codespace_ssh)
                == Some(codespace.name.as_str())
        }) {
            // Killing the session we're attached to would sever this client
            // mid-picker; leave it — its panes just lose their connection.
            if !bound.is_current_session {
                let _ = kill_sessions(&[bound.name.as_str()]);
            }
        }

        let argv = codespaces::stop_argv(&codespace.name);
        let argv_refs: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();
        run_command(&argv_refs, codespaces::stop_context(&codespace.name));
        self.pending_stop = Some(codespace.name);
        true
    }

    /// Open the selected project: switch to its session if one already roots at
    /// that folder, otherwise create a new session there with the configured
    /// `session_layout`. Bumps the frecency db on the open, then closes the
    /// picker.
    fn confirm_selection(&mut self) {
        match self.mode {
            PickerMode::Sessions => {
                self.confirm_session_selection();
                return;
            },
            PickerMode::Codespaces => {
                self.confirm_codespace_selection();
                return;
            },
            PickerMode::Projects => {},
        }
        let now = now_secs();
        let results = ranking::rank(&self.projects, &self.query, &self.frecency, now);
        // Clone the path so the borrow of `self.projects` (via `results`) ends
        // before we mutably borrow `self.frecency` below.
        let Some(path) = results.get(self.selected).map(|r| r.project.path.clone()) else {
            return;
        };

        // Bump + persist frecency so this open floats the project toward the
        // input next time. Best-effort: a failed write is silently ignored.
        // (An Esc-cancelled devcontainer prompt still counts as a use — Enter
        // already expressed the intent.)
        self.frecency.bump(&path.to_string_lossy(), now);
        self.frecency.save();

        // Devcontainer divert: only when the folder has no session yet (an
        // existing session — bound or plain — always just switches) and it
        // carries a `.devcontainer` marker. The prompt takes over the
        // keyboard; the picker closes when the flow resolves.
        let action = session::resolve_open(&path, &self.existing_sessions());
        if matches!(action, OpenAction::Create { .. }) && self.project_has_devcontainer(&path) {
            let display_name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| path.to_string_lossy().to_string());
            self.pending_devcontainer = Some(PendingDevcontainer {
                path,
                display_name,
                phase: DevcontainerPhase::Prompt,
            });
            return;
        }

        self.open_project_locally(path);
    }

    /// Open `path` as a plain local project session: switch when a session
    /// roots there, else create one with the configured `session_layout`.
    /// Then closes the picker. (Resolution happens here — not at the earlier
    /// prompt divert — because the devcontainer flow can take minutes and the
    /// session list may have changed underneath it.)
    fn open_project_locally(&mut self, path: PathBuf) {
        let action = session::resolve_open(&path, &self.existing_sessions());
        let current_session_name = self
            .sessions
            .iter()
            .find(|s| s.is_current_session)
            .map(|s| s.name.clone());

        match action {
            // The folder's session is the one we're already in: switching would
            // be refused by the server ("Cannot attach to same session"), so
            // just dismiss the picker.
            OpenAction::Switch { name } if Some(&name) == current_session_name.as_ref() => {},
            OpenAction::Switch { name } => {
                switch_session_with_cwd(Some(&name), Some(path));
                self.discard_throwaway_session(current_session_name.as_deref());
            },
            OpenAction::Create { name } => {
                let layout =
                    LayoutInfo::File(self.config.session_layout.clone(), LayoutMetadata::default());
                switch_session_with_layout(Some(&name), layout, Some(path));
                self.discard_throwaway_session(current_session_name.as_deref());
            },
        }
        close_self();
    }

    /// Open `path` as a devcontainer-bound session, now that the picker's
    /// `devcontainer up` succeeded: switch when a session appeared for the
    /// folder while the up ran (a first build can take minutes), else create
    /// one from the generated bound layout. The explicit cwd makes the new
    /// session's `workspace_root` the project folder, so later picks switch to
    /// it like any local session. Then closes the picker.
    fn open_project_in_devcontainer(&mut self, path: PathBuf) {
        let action = session::resolve_open(&path, &self.existing_sessions());
        let current_session_name = self
            .sessions
            .iter()
            .find(|s| s.is_current_session)
            .map(|s| s.name.clone());

        match action {
            OpenAction::Switch { name } if Some(&name) == current_session_name.as_ref() => {},
            OpenAction::Switch { name } => {
                switch_session_with_cwd(Some(&name), Some(path));
                self.discard_throwaway_session(current_session_name.as_deref());
            },
            OpenAction::Create { name } => {
                let layout = LayoutInfo::Stringified(devcontainers::layout_doc_for(
                    &path,
                    &self.config.sidebar_args,
                    self.codespace_layout_base.as_deref(),
                ));
                switch_session_with_layout(Some(&name), layout, Some(path));
                self.discard_throwaway_session(current_session_name.as_deref());
            },
        }
        close_self();
    }

    /// Keys while a devcontainer prompt/up owns the picker (see
    /// [`Self::handle_key`]).
    fn handle_devcontainer_key(&mut self, key: KeyWithModifier) -> bool {
        let Some(pending) = self.pending_devcontainer.as_mut() else {
            return false;
        };
        // Ctrl-c always abandons the picker, whatever the phase (an in-flight
        // up finishes host-side harmlessly; the next pick's up is idempotent).
        if key.has_modifiers(&[KeyModifier::Ctrl]) && key.bare_key == BareKey::Char('c') {
            close_self();
            return false;
        }
        match pending.phase.clone() {
            DevcontainerPhase::Prompt => match key.bare_key {
                BareKey::Char('y') | BareKey::Char('Y') if key.has_no_modifiers() => {
                    pending.phase = DevcontainerPhase::Starting;
                    let argv = devcontainers::up_argv(&pending.path);
                    let context = devcontainers::up_context(&pending.path);
                    let argv_refs: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();
                    run_command(&argv_refs, context);
                    true
                },
                BareKey::Char('n') | BareKey::Char('N') if key.has_no_modifiers() => {
                    let pending = self.pending_devcontainer.take().expect("checked above");
                    self.open_project_locally(pending.path);
                    true
                },
                BareKey::Esc => {
                    self.pending_devcontainer = None;
                    true
                },
                // Swallow everything else — the list underneath is frozen.
                _ => true,
            },
            DevcontainerPhase::Starting => match key.bare_key {
                // Esc abandons the picker but not the up: with no session
                // created yet there is nothing to unwind, and a finished up
                // just makes the next pick instant.
                BareKey::Esc => {
                    close_self();
                    false
                },
                _ => true,
            },
            DevcontainerPhase::Failed(_) => {
                // Any key dismisses the error back to the normal picker.
                self.pending_devcontainer = None;
                true
            },
        }
    }

    /// Route a `devcontainer up` result: create/switch on success, show the
    /// classified error on failure. Ignores results that no longer match the
    /// pending state (the user may have Esc'd and re-picked while it ran).
    fn handle_devcontainer_up_result(
        &mut self,
        path_str: &str,
        exit_code: Option<i32>,
        stderr: &str,
    ) -> bool {
        let matches_pending = self.pending_devcontainer.as_ref().is_some_and(|p| {
            p.phase == DevcontainerPhase::Starting && p.path.to_string_lossy() == path_str
        });
        if !matches_pending {
            return false;
        }
        if exit_code == Some(0) {
            let pending = self.pending_devcontainer.take().expect("checked above");
            self.open_project_in_devcontainer(pending.path);
        } else if let Some(pending) = self.pending_devcontainer.as_mut() {
            pending.phase =
                DevcontainerPhase::Failed(devcontainers::classify_error(exit_code, stderr));
        }
        true
    }

    /// Whether `path` (a project folder) carries a `.devcontainer` marker,
    /// per the latest scans.
    fn project_has_devcontainer(&self, path: &Path) -> bool {
        let path = config::normalize(path);
        self.devcontainer_projects
            .values()
            .any(|set| set.contains(&path))
    }

    /// Kick off the `.devcontainer` marker scans: one `find` over all root
    /// dirs (markers one level under each project) and one over the
    /// individual dirs, so the Enter-time prompt check is a set lookup.
    fn fire_devcontainer_scans(&self) {
        if !self.config.root_dirs.is_empty() {
            let argv = devcontainers::scan_roots_argv(&self.config.root_dirs);
            let argv_refs: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();
            run_command(
                &argv_refs,
                devcontainers::scan_context(devcontainers::SCAN_SCOPE_ROOTS),
            );
        }
        if !self.config.individual_dirs.is_empty() {
            let argv = devcontainers::scan_individual_argv(&self.config.individual_dirs);
            let argv_refs: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();
            run_command(
                &argv_refs,
                devcontainers::scan_context(devcontainers::SCAN_SCOPE_INDIVIDUAL),
            );
        }
    }

    /// In cold-shell mode (`session_name` set) the session we ran in is a
    /// throwaway whose only purpose was hosting this picker. Once the client
    /// has been handed to the target session (the switch shim blocks until the
    /// handoff completes), kill it so it doesn't linger holding the fixed
    /// session name — a lingering throwaway makes the next cold-shell launch's
    /// rename collide, and its `workspace_root` (the launch folder) shadows
    /// that folder's real session. No-op for keybind launches, where the
    /// current session is the user's working session.
    fn discard_throwaway_session(&self, current_session_name: Option<&str>) {
        if self.config.session_name.is_none() {
            return;
        }
        if let Some(name) = current_session_name {
            let _ = kill_sessions(&[name]);
        }
    }

    /// The live sessions reduced to the fields [`session::resolve_open`] needs,
    /// dropping any whose `workspace_root` is unknown (they can't match a folder)
    /// and marking selector throwaways hidden (matchable by name only).
    fn existing_sessions(&self) -> Vec<ExistingSession> {
        self.sessions
            .iter()
            .filter(|s| !s.workspace_root.as_os_str().is_empty())
            .map(|s| ExistingSession {
                name: s.name.clone(),
                workspace_root: config::normalize(&s.workspace_root),
                hidden: self.is_selector_session(s),
            })
            .collect()
    }
}
