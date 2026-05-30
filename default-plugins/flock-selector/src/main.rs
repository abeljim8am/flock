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
//! Switching/creating sessions on confirm is **not** wired here — that is Phase
//! 9. For now confirming a project is a no-op that logs the choice.

mod config;
mod discovery;
mod frecency;
mod fuzzy;
mod palette;
mod ranking;
mod ui;

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

use config::SelectorConfig;
use discovery::{merge_candidates, parse_scan_output, scan_argv, scan_context, Project, SCAN_CONTEXT_KEY};
use frecency::{now_secs, FrecencyDb};
use palette::Theme;
use zellij_tile::prelude::*;

/// How often to re-scan the root dirs so newly-created project folders surface
/// without reopening the picker.
const REFRESH_SECS: f64 = 10.0;

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
}

register_plugin!(State);

impl ZellijPlugin for State {
    fn load(&mut self, configuration: BTreeMap<String, String>) {
        self.config = SelectorConfig::from_args(&configuration);
        self.frecency = FrecencyDb::load();
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
                    self.arm_refresh_timer();
                }
            },
            Event::RunCommandResult(exit_code, stdout, _stderr, context) => {
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

        let output = ui::render(ui::RenderInput {
            permissions_granted: self.permissions_granted,
            configured: !self.config.individual_dirs.is_empty()
                || !self.config.root_dirs.is_empty(),
            query: &self.query,
            results: &results,
            open_paths: &open_paths,
            palette: &self.palette,
            selected: self.selected,
            scroll: self.scroll,
            total_candidates: self.projects.len(),
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

    /// Absolute paths of folders that currently have a live session, matched
    /// against each session's `workspace_root` (the Phase 6 fork field).
    fn open_session_paths(&self) -> HashSet<String> {
        self.sessions
            .iter()
            .map(|s| config::normalize(&s.workspace_root).to_string_lossy().to_string())
            .filter(|p| !p.is_empty())
            .collect()
    }

    fn handle_key(&mut self, key: KeyWithModifier) -> bool {
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
                _ => return false,
            }
        }
        false
    }

    /// Move the cursor toward a worse (higher-on-screen) result.
    fn select_worse(&mut self) {
        let max = self.projects.len().saturating_sub(1);
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

    /// Phase 8: confirming a project is a no-op that logs the choice. Phase 9
    /// wires switch-or-create here.
    fn confirm_selection(&mut self) {
        let now = now_secs();
        let results = ranking::rank(&self.projects, &self.query, &self.frecency, now);
        if let Some(chosen) = results.get(self.selected) {
            eprintln!(
                "flock-selector: would open project {}",
                chosen.project.path.display()
            );
        }
    }
}
