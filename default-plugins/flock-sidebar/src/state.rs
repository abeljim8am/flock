//! Per-pane agent-state arbitration, ported from herdr's `terminal/state.rs`.
//!
//! herdr centralizes effective-state arbitration so that an agent's own hook
//! reports are the default authority for its internal state, while a narrow set
//! of strong *visible* screen signals can veto stale, non-blocked hook reports.
//! Precedence is:
//!
//! ```text
//! hook blocked > strong visible blocker > visible working override
//!     > visible idle stales hook > hook > screen fallback
//! ```
//!
//! This is a focused port: it keeps the arbitration precedence and the
//! `stabilize_agent_detection` hold/grace timers, but drops herdr's persistence,
//! session-ref, and metadata machinery (none of which a Zellij plugin owns). The
//! async polling task that drove herdr's timers becomes Zellij `Timer` events —
//! see `tick()`, which the plugin calls on each timer fire so the Claude
//! working-hold and the stale-hook-idle grace window expire even when no new
//! render report arrives.

use std::time::{Duration, Instant};

use crate::detect::{self, Agent, AgentDetection, AgentState};

/// How long Claude's `Working` state is held after the screen first looks idle,
/// to ride out the brief flicker between a tool finishing and the next spinner.
const CLAUDE_WORKING_HOLD: Duration = Duration::from_millis(1200);
/// How long a visible idle screen must persist before it is allowed to override
/// a `Working` hook report (guards against a hook that missed its stop event).
const STALE_HOOK_IDLE_GRACE: Duration = Duration::from_secs(2);

/// An agent's self-reported state, delivered via the Phase 5 hook channel
/// (`zellij pipe`). This is the authority for the agent's internal state unless
/// a strong visible screen signal vetoes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookAuthority {
    pub source: String,
    pub agent_label: String,
    pub state: AgentState,
    pub message: Option<String>,
    pub reported_at: Instant,
}

/// The arbitrated agent state for a single pane.
pub struct PaneAgentState {
    /// Agent identified from the pane's running command (`CommandChanged`).
    pub detected_agent: Option<Agent>,
    /// Latest screen-derived state, after Claude working-hold stabilization.
    pub fallback_state: AgentState,
    fallback_visible_blocker: bool,
    fallback_visible_idle: bool,
    fallback_visible_working: bool,
    fallback_observed_at: Option<Instant>,
    stale_hook_idle_since: Option<Instant>,
    /// Latest agent self-report, if any (Phase 5).
    pub hook_authority: Option<HookAuthority>,
    last_claude_working_at: Option<Instant>,
    /// Raw screen detection from the last render report, re-evaluated on `tick()`
    /// so time-based holds/grace windows expire without a new screen update.
    last_detection: Option<AgentDetection>,
    /// The arbitrated effective state.
    pub state: AgentState,
}

impl Default for PaneAgentState {
    fn default() -> Self {
        Self {
            detected_agent: None,
            fallback_state: AgentState::Unknown,
            fallback_visible_blocker: false,
            fallback_visible_idle: false,
            fallback_visible_working: false,
            fallback_observed_at: None,
            stale_hook_idle_since: None,
            hook_authority: None,
            last_claude_working_at: None,
            last_detection: None,
            state: AgentState::Unknown,
        }
    }
}

impl PaneAgentState {
    #[allow(dead_code)] // used in tests; the plugin constructs via `or_default()`.
    pub fn new() -> Self {
        Self::default()
    }

    /// True once we have any agent signal for this pane (a detected agent or a
    /// hook report). Panes that never run an agent stay false and are hidden.
    pub fn is_agent(&self) -> bool {
        self.detected_agent.is_some() || self.hook_authority.is_some()
    }

    /// The agent label to display: a hook report's label takes precedence over
    /// the process-detected agent (a hook may name a custom agent we don't know).
    pub fn effective_agent_label(&self) -> Option<String> {
        self.hook_authority
            .as_ref()
            .map(|authority| authority.agent_label.clone())
            .or_else(|| self.detected_agent.map(|a| detect::agent_label(a).to_string()))
    }

    /// The known [`Agent`] backing the effective state, if any.
    #[allow(dead_code)] // exercised in tests; consumed by the Phase 3 UI.
    pub fn effective_known_agent(&self) -> Option<Agent> {
        if let Some(authority) = &self.hook_authority {
            return detect::parse_agent_label(&authority.agent_label);
        }
        self.detected_agent
    }

    /// Update the detected agent from a pane's running command. Clears the
    /// screen fallback when the agent changes (the old chrome no longer applies).
    /// Returns whether the arbitrated state or label changed.
    pub fn set_detected_agent(&mut self, agent: Option<Agent>, now: Instant) -> bool {
        if agent == self.detected_agent {
            return false;
        }
        let snapshot = self.snapshot();
        self.detected_agent = agent;
        // The previous screen detection belonged to the old agent; discard it.
        self.last_detection = None;
        self.fallback_state = AgentState::Unknown;
        self.fallback_visible_blocker = false;
        self.fallback_visible_idle = false;
        self.fallback_visible_working = false;
        self.fallback_observed_at = None;
        self.maybe_clear_conflicting_hook(agent, now);
        self.update_stale_hook_idle_window(now);
        self.recompute(now);
        self.changed_since(&snapshot)
    }

    /// Feed a fresh screen detection (from `PaneRenderReportWithAnsi`). Applies
    /// the Claude working-hold, records the visible signals, and re-arbitrates.
    /// Returns whether the arbitrated state or label changed.
    pub fn observe_screen(
        &mut self,
        agent: Option<Agent>,
        detection: AgentDetection,
        now: Instant,
    ) -> bool {
        let snapshot = self.snapshot();
        self.detected_agent = agent;
        self.last_detection = Some(detection);
        let stabilized = stabilize_agent_detection(
            agent,
            self.state,
            detection,
            false,
            now,
            &mut self.last_claude_working_at,
        );
        self.apply_screen_signals(stabilized, detection, now);
        self.changed_since(&snapshot)
    }

    /// Record an agent self-report (Phase 5 hook channel). Returns whether the
    /// arbitrated state or label changed.
    #[allow(dead_code)] // wired up by the Phase 5 hook channel (`pipe()`).
    pub fn set_hook_authority(
        &mut self,
        source: String,
        agent_label: String,
        state: AgentState,
        message: Option<String>,
        now: Instant,
    ) -> bool {
        let snapshot = self.snapshot();
        self.hook_authority = Some(HookAuthority {
            source,
            agent_label,
            state,
            message,
            reported_at: now,
        });
        self.stale_hook_idle_since = None;
        self.recompute(now);
        self.changed_since(&snapshot)
    }

    /// Clear any hook authority (e.g. on agent exit). Returns whether the
    /// arbitrated state or label changed.
    #[allow(dead_code)] // wired up by the Phase 5 hook channel (`pipe()`).
    pub fn clear_hook_authority(&mut self, now: Instant) -> bool {
        if self.hook_authority.is_none() {
            return false;
        }
        let snapshot = self.snapshot();
        self.hook_authority = None;
        self.stale_hook_idle_since = None;
        self.recompute(now);
        self.changed_since(&snapshot)
    }

    /// Re-evaluate time-based holds/grace windows on a `Timer` fire, re-running
    /// the last screen detection so the Claude working-hold and stale-hook-idle
    /// grace can expire without a new render report. Returns whether the
    /// arbitrated state or label changed.
    pub fn tick(&mut self, now: Instant) -> bool {
        let snapshot = self.snapshot();
        if let Some(detection) = self.last_detection {
            let agent = self.detected_agent;
            let stabilized = stabilize_agent_detection(
                agent,
                self.state,
                detection,
                false,
                now,
                &mut self.last_claude_working_at,
            );
            self.apply_screen_signals(stabilized, detection, now);
        } else {
            self.update_stale_hook_idle_window(now);
            self.recompute(now);
        }
        self.changed_since(&snapshot)
    }

    // -- internals ----------------------------------------------------------

    fn apply_screen_signals(
        &mut self,
        fallback_state: AgentState,
        detection: AgentDetection,
        now: Instant,
    ) {
        self.fallback_state = fallback_state;
        self.fallback_visible_blocker =
            detection.visible_blocker && fallback_state == AgentState::Blocked;
        self.fallback_visible_idle =
            detection.visible_idle && fallback_state == AgentState::Idle;
        self.fallback_visible_working =
            detection.visible_working && fallback_state == AgentState::Working;
        self.fallback_observed_at = Some(now);
        self.maybe_clear_conflicting_hook(self.detected_agent, now);
        self.update_stale_hook_idle_window(now);
        self.recompute(now);
    }

    /// A hook naming a *different* known agent than the one now detected is
    /// stale — drop it so screen detection takes over (mirrors herdr).
    fn maybe_clear_conflicting_hook(&mut self, agent: Option<Agent>, now: Instant) {
        if self.hook_authority_not_newer_than(now)
            && self.hook_authority_conflicts_with_detected_agent(agent)
        {
            self.hook_authority = None;
            self.stale_hook_idle_since = None;
        }
    }

    fn recompute(&mut self, now: Instant) {
        let state = if self
            .hook_authority
            .as_ref()
            .is_some_and(|authority| authority.state == AgentState::Blocked)
            || self.visible_blocker_overrides_hook()
        {
            AgentState::Blocked
        } else if self.visible_working_overrides_hook() {
            AgentState::Working
        } else if self.visible_idle_stales_hook(now) {
            AgentState::Idle
        } else {
            self.hook_authority
                .as_ref()
                .map(|authority| authority.state)
                .unwrap_or(self.fallback_state)
        };
        self.state = state;
    }

    fn visible_blocker_overrides_hook(&self) -> bool {
        self.fallback_visible_blocker
            && self.fallback_not_older_than_hook()
            && self.hook_authority.as_ref().is_some_and(|authority| {
                authority.state != AgentState::Blocked
                    && detect::parse_agent_label(&authority.agent_label) == self.detected_agent
            })
    }

    fn visible_working_overrides_hook(&self) -> bool {
        self.fallback_visible_working
            && self.fallback_not_older_than_hook()
            && self.hook_authority.as_ref().is_some_and(|authority| {
                authority.state == AgentState::Idle
                    && detect::parse_agent_label(&authority.agent_label) == self.detected_agent
            })
    }

    fn visible_idle_stales_hook(&self, now: Instant) -> bool {
        self.stale_hook_idle_since
            .is_some_and(|since| now.duration_since(since) >= STALE_HOOK_IDLE_GRACE)
    }

    fn update_stale_hook_idle_window(&mut self, now: Instant) {
        let visible_idle_stales_hook = self.fallback_visible_idle
            && self.fallback_not_older_than_hook()
            && self.hook_authority.as_ref().is_some_and(|authority| {
                authority.state == AgentState::Working
                    && detect::parse_agent_label(&authority.agent_label) == self.detected_agent
            });

        if visible_idle_stales_hook {
            self.stale_hook_idle_since.get_or_insert(now);
        } else {
            self.stale_hook_idle_since = None;
        }
    }

    fn hook_authority_not_newer_than(&self, observed_at: Instant) -> bool {
        self.hook_authority
            .as_ref()
            .is_none_or(|authority| authority.reported_at <= observed_at)
    }

    fn fallback_not_older_than_hook(&self) -> bool {
        self.hook_authority.as_ref().is_none_or(|authority| {
            self.fallback_observed_at
                .is_some_and(|observed_at| authority.reported_at <= observed_at)
        })
    }

    fn hook_authority_conflicts_with_detected_agent(&self, detected_agent: Option<Agent>) -> bool {
        let Some(detected_agent) = detected_agent else {
            return false;
        };
        self.hook_authority.as_ref().is_some_and(|authority| {
            detect::parse_agent_label(&authority.agent_label)
                .is_some_and(|hook_agent| hook_agent != detected_agent)
        })
    }

    fn snapshot(&self) -> (AgentState, Option<String>) {
        (self.state, self.effective_agent_label())
    }

    fn changed_since(&self, snapshot: &(AgentState, Option<String>)) -> bool {
        self.snapshot() != *snapshot
    }
}

/// Hold Claude's `Working` state briefly after the screen first looks idle, to
/// ride out the flicker between a tool result and the next spinner frame. Other
/// agents pass through unchanged. Ported verbatim from herdr.
pub(crate) fn stabilize_agent_state(
    agent: Option<Agent>,
    previous: AgentState,
    raw: AgentState,
    now: Instant,
    last_claude_working_at: &mut Option<Instant>,
) -> AgentState {
    if agent != Some(Agent::Claude) {
        return raw;
    }

    match raw {
        AgentState::Working => {
            *last_claude_working_at = Some(now);
            AgentState::Working
        }
        AgentState::Blocked => AgentState::Blocked,
        AgentState::Idle if previous == AgentState::Working => {
            if last_claude_working_at
                .is_some_and(|last_working| now.duration_since(last_working) < CLAUDE_WORKING_HOLD)
            {
                AgentState::Working
            } else {
                AgentState::Idle
            }
        }
        _ => raw,
    }
}

/// Stabilize a fresh detection. A process-exit observation bypasses the hold so
/// a finished agent settles immediately. Ported verbatim from herdr.
pub(crate) fn stabilize_agent_detection(
    agent: Option<Agent>,
    previous: AgentState,
    detection: AgentDetection,
    process_exited: bool,
    now: Instant,
    last_claude_working_at: &mut Option<Instant>,
) -> AgentState {
    if process_exited {
        return detection.state;
    }

    stabilize_agent_state(agent, previous, detection.state, now, last_claude_working_at)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detection(state: AgentState) -> AgentDetection {
        AgentDetection {
            state,
            visible_blocker: false,
            visible_idle: false,
            visible_working: false,
        }
    }

    fn visible(state: AgentState, blocker: bool, idle: bool, working: bool) -> AgentDetection {
        AgentDetection {
            state,
            visible_blocker: blocker,
            visible_idle: idle,
            visible_working: working,
        }
    }

    // ---- stabilization (Claude working-hold) ----

    #[test]
    fn claude_working_is_sticky_for_short_gap() {
        let now = Instant::now();
        let mut last_working = None;

        let working = stabilize_agent_state(
            Some(Agent::Claude),
            AgentState::Idle,
            AgentState::Working,
            now,
            &mut last_working,
        );
        assert_eq!(working, AgentState::Working);

        let still_working = stabilize_agent_state(
            Some(Agent::Claude),
            AgentState::Working,
            AgentState::Idle,
            now + Duration::from_millis(400),
            &mut last_working,
        );
        assert_eq!(still_working, AgentState::Working);
    }

    #[test]
    fn claude_transitions_to_idle_after_hold_expires() {
        let now = Instant::now();
        let mut last_working = Some(now);

        let state = stabilize_agent_state(
            Some(Agent::Claude),
            AgentState::Working,
            AgentState::Idle,
            now + CLAUDE_WORKING_HOLD + Duration::from_millis(1),
            &mut last_working,
        );
        assert_eq!(state, AgentState::Idle);
    }

    #[test]
    fn process_exit_idle_bypasses_claude_working_hold() {
        let now = Instant::now();
        let mut last_working = Some(now);

        let state = stabilize_agent_detection(
            Some(Agent::Claude),
            AgentState::Working,
            detection(AgentState::Idle),
            true,
            now + Duration::from_millis(100),
            &mut last_working,
        );
        assert_eq!(state, AgentState::Idle);
    }

    #[test]
    fn visible_idle_does_not_bypass_claude_working_hold() {
        let now = Instant::now();
        let mut last_working = Some(now);

        let state = stabilize_agent_detection(
            Some(Agent::Claude),
            AgentState::Working,
            visible(AgentState::Idle, false, true, false),
            false,
            now + Duration::from_millis(100),
            &mut last_working,
        );
        assert_eq!(state, AgentState::Working);
    }

    #[test]
    fn non_claude_states_are_unchanged() {
        let now = Instant::now();
        let mut last_working = None;

        let state = stabilize_agent_state(
            Some(Agent::Codex),
            AgentState::Working,
            AgentState::Idle,
            now,
            &mut last_working,
        );
        assert_eq!(state, AgentState::Idle);
    }

    // ---- arbitration ----

    #[test]
    fn screen_fallback_drives_state_without_hook() {
        let now = Instant::now();
        let mut pane = PaneAgentState::new();
        pane.observe_screen(Some(Agent::Pi), detection(AgentState::Working), now);
        assert_eq!(pane.state, AgentState::Working);
        assert_eq!(pane.effective_agent_label().as_deref(), Some("pi"));
        assert!(pane.is_agent());
    }

    #[test]
    fn hook_authority_overrides_fallback_for_same_agent() {
        let now = Instant::now();
        let mut pane = PaneAgentState::new();
        pane.observe_screen(Some(Agent::Pi), detection(AgentState::Idle), now);
        pane.set_hook_authority(
            "herdr:pi".into(),
            "pi".into(),
            AgentState::Working,
            None,
            now,
        );

        assert_eq!(pane.detected_agent, Some(Agent::Pi));
        assert_eq!(pane.fallback_state, AgentState::Idle);
        assert_eq!(pane.effective_agent_label().as_deref(), Some("pi"));
        assert_eq!(pane.state, AgentState::Working);
    }

    #[test]
    fn hook_authority_can_override_with_unknown_agent_label() {
        let now = Instant::now();
        let mut pane = PaneAgentState::new();
        pane.observe_screen(Some(Agent::Pi), detection(AgentState::Idle), now);
        pane.set_hook_authority(
            "herdr:custom".into(),
            "custom-agent".into(),
            AgentState::Working,
            None,
            now,
        );

        assert_eq!(pane.detected_agent, Some(Agent::Pi));
        assert_eq!(pane.effective_agent_label().as_deref(), Some("custom-agent"));
        assert_eq!(pane.effective_known_agent(), None);
        assert_eq!(pane.state, AgentState::Working);
    }

    #[test]
    fn visible_blocker_overrides_non_blocked_hook_for_same_agent() {
        let now = Instant::now();
        let mut pane = PaneAgentState::new();
        pane.observe_screen(Some(Agent::Codex), detection(AgentState::Idle), now);
        pane.set_hook_authority(
            "herdr:codex".into(),
            "codex".into(),
            AgentState::Working,
            None,
            now,
        );

        let changed = pane.observe_screen(
            Some(Agent::Codex),
            visible(AgentState::Blocked, true, false, false),
            now,
        );

        assert!(changed);
        assert_eq!(pane.fallback_state, AgentState::Blocked);
        assert_eq!(pane.state, AgentState::Blocked);
    }

    #[test]
    fn weak_blocked_fallback_does_not_override_hook_authority() {
        let now = Instant::now();
        let mut pane = PaneAgentState::new();
        pane.observe_screen(Some(Agent::Codex), detection(AgentState::Idle), now);
        pane.set_hook_authority(
            "herdr:codex".into(),
            "codex".into(),
            AgentState::Working,
            None,
            now,
        );

        pane.observe_screen(
            Some(Agent::Codex),
            visible(AgentState::Blocked, false, false, false),
            now,
        );

        assert_eq!(pane.fallback_state, AgentState::Blocked);
        assert_eq!(pane.state, AgentState::Working);
    }

    #[test]
    fn hook_blocked_wins_over_visible_blocker() {
        let now = Instant::now();
        let mut pane = PaneAgentState::new();
        pane.observe_screen(Some(Agent::Codex), detection(AgentState::Working), now);
        pane.set_hook_authority(
            "herdr:codex".into(),
            "codex".into(),
            AgentState::Blocked,
            None,
            now,
        );

        pane.observe_screen(
            Some(Agent::Codex),
            visible(AgentState::Blocked, true, false, false),
            now,
        );

        assert_eq!(pane.state, AgentState::Blocked);
        assert!(pane.hook_authority.is_some());
    }

    #[test]
    fn visible_blocker_does_not_override_different_agent_hook() {
        let now = Instant::now();
        let mut pane = PaneAgentState::new();
        pane.set_hook_authority(
            "custom:agent".into(),
            "custom-agent".into(),
            AgentState::Working,
            None,
            now,
        );

        pane.observe_screen(
            Some(Agent::Codex),
            visible(AgentState::Blocked, true, false, false),
            now,
        );

        // A hook naming a *different* known agent would be cleared as stale, but
        // an unknown-label hook stays and keeps authority over the visible
        // blocker (its agent identity can't be said to conflict).
        assert_eq!(pane.effective_agent_label().as_deref(), Some("custom-agent"));
        assert_eq!(pane.state, AgentState::Working);
    }

    #[test]
    fn conflicting_known_agent_hook_is_cleared_by_detection() {
        let now = Instant::now();
        let mut pane = PaneAgentState::new();
        pane.set_hook_authority(
            "herdr:pi".into(),
            "pi".into(),
            AgentState::Working,
            None,
            now,
        );
        // Screen now shows a different known agent — the stale pi hook is dropped.
        pane.observe_screen(Some(Agent::Codex), detection(AgentState::Idle), now);

        assert!(pane.hook_authority.is_none());
        assert_eq!(pane.effective_known_agent(), Some(Agent::Codex));
        assert_eq!(pane.state, AgentState::Idle);
    }

    #[test]
    fn visible_idle_stales_working_hook_after_grace() {
        let now = Instant::now();
        let mut pane = PaneAgentState::new();
        pane.set_hook_authority(
            "herdr:codex".into(),
            "codex".into(),
            AgentState::Working,
            None,
            now,
        );
        // Visible idle screen opens the stale-hook grace window but doesn't win yet.
        pane.observe_screen(
            Some(Agent::Codex),
            visible(AgentState::Idle, false, true, false),
            now,
        );
        assert_eq!(pane.state, AgentState::Working);

        // After the grace window, the persistent visible idle stales the hook.
        let changed = pane.tick(now + STALE_HOOK_IDLE_GRACE + Duration::from_millis(1));
        assert!(changed);
        assert_eq!(pane.state, AgentState::Idle);
    }

    #[test]
    fn visible_working_overrides_idle_hook() {
        let now = Instant::now();
        let mut pane = PaneAgentState::new();
        pane.observe_screen(Some(Agent::Codex), detection(AgentState::Working), now);
        pane.set_hook_authority(
            "herdr:codex".into(),
            "codex".into(),
            AgentState::Idle,
            None,
            now,
        );
        assert_eq!(pane.state, AgentState::Idle);

        pane.observe_screen(
            Some(Agent::Codex),
            visible(AgentState::Working, false, false, true),
            now,
        );
        assert_eq!(pane.state, AgentState::Working);
    }

    #[test]
    fn changing_detected_agent_clears_screen_fallback() {
        let now = Instant::now();
        let mut pane = PaneAgentState::new();
        pane.observe_screen(Some(Agent::Pi), detection(AgentState::Working), now);
        assert_eq!(pane.state, AgentState::Working);

        // Agent goes away (shell returns) — state settles to Unknown.
        let changed = pane.set_detected_agent(None, now);
        assert!(changed);
        assert_eq!(pane.fallback_state, AgentState::Unknown);
        assert_eq!(pane.state, AgentState::Unknown);
        assert!(!pane.is_agent());
    }
}
