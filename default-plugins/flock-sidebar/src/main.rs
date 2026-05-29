//! flock-sidebar — an agent-aware sidebar plugin for Zellij.
//!
//! This is the Phase 1 skeleton: it stands up the plugin crate, requests the
//! permissions the later phases need, subscribes to the events that drive agent
//! detection, and renders a placeholder sidebar. There is no detection logic
//! yet — that arrives in Phase 2.

use std::collections::BTreeMap;
use zellij_tile::prelude::*;

#[derive(Default)]
struct State {
    /// Whether our permission request has been granted yet. Until it is, we
    /// can't read pane contents / application state, so we render a hint.
    permissions_granted: bool,
    /// Latest pane manifest for our own session (populated in later phases).
    panes: PaneManifest,
    /// Latest tab list for our own session.
    tabs: Vec<TabInfo>,
    /// Latest cross-session list (used for workspace grouping in later phases).
    sessions: Vec<SessionInfo>,
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
        ]);
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
            // Detection events — subscribed now, handled in Phase 2.
            Event::CommandChanged(..) => {},
            Event::PaneRenderReportWithAnsi(_) => {},
            Event::Mouse(_) => {},
            Event::Key(key) => {
                // Esc closes the sidebar when it's focused (e.g. a floating pane).
                if key.bare_key == BareKey::Esc && key.has_no_modifiers() {
                    close_self();
                }
            },
            Event::Visible(_) => {},
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

        // Phase 1 placeholder: prove the sidebar renders and reflects live
        // session/tab/pane counts. Real agent detection lands in Phase 2.
        let pane_count: usize = self.panes.panes.values().map(|p| p.len()).sum();
        let summary = format!(
            "{} session(s) · {} tab(s) · {} pane(s)",
            self.sessions.len().max(1),
            self.tabs.len(),
            pane_count
        );
        print_text_with_coordinates(Text::new(summary), 0, 2, Some(cols), None);

        let placeholder = Text::new("no agents detected yet").color_range(0, ..);
        print_text_with_coordinates(placeholder, 0, 4, Some(cols), None);
    }
}
