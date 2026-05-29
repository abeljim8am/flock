//! Phase 5 hook channel — agent self-reporting via `zellij pipe`.
//!
//! herdr lets an agent report its own state directly: each integration hook
//! writes a JSON-RPC `pane.report_agent` / `pane.release_agent` request to
//! herdr's unix socket (see `herdr/src/integration/assets/*/herdr-agent-state.sh`
//! and `herdr/src/events.rs::AppEvent::HookStateReported`). That self-report is
//! the authority for the agent's internal state unless a strong visible screen
//! signal vetoes it — the arbitration ported into [`crate::state`].
//!
//! In Zellij there is no herdr socket; the equivalent transport is a CLI pipe.
//! Agents invoke
//!
//! ```text
//! zellij pipe --name flock-state \
//!   --args 'pane_id=3,state=blocked,agent=claude,source=flock:claude'
//! ```
//!
//! which the plugin receives in [`ZellijPlugin::pipe`] as a [`PipeMessage`] whose
//! `args` Zellij has already split into a `key=value` map. This module turns that
//! map into a typed [`HookReport`]; the plugin then applies it to the target
//! pane's [`PaneAgentState`](crate::state::PaneAgentState) via `set_hook_authority`
//! / `clear_hook_authority`, exactly as herdr's main loop applies
//! `HookStateReported` / `HookAuthorityCleared`.
//!
//! Keeping the parse here (not inline in `pipe()`) lets it be unit-tested off the
//! wasm target — `pipe()` itself can only run inside Zellij.

use std::collections::BTreeMap;

use zellij_tile::prelude::PaneId;

use crate::detect::AgentState;

/// The pipe name agents publish their state under — the `zellij pipe --name
/// <NAME>` value the bundled hooks emit.
pub const HOOK_PIPE_NAME: &str = "flock-state";

/// Fallback agent label when a report omits `agent` (e.g. a hand-run
/// `zellij pipe ... 'pane_id=3,state=blocked'`). An unrecognized label can't be
/// said to conflict with the process-detected agent, so screen detection won't
/// veto it — see [`crate::state::PaneAgentState`].
const DEFAULT_AGENT_LABEL: &str = "agent";

/// Default report source when a report omits `source`.
const DEFAULT_SOURCE: &str = "flock";

/// A parsed agent self-report from a `flock-state` pipe message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookReport {
    /// Set or refresh the pane's hook authority (herdr's `pane.report_agent`).
    State {
        pane_id: PaneId,
        source: String,
        agent_label: String,
        state: AgentState,
        message: Option<String>,
    },
    /// Release the pane back to the shell, clearing hook authority (herdr's
    /// `pane.release_agent`).
    Release { pane_id: PaneId },
}

/// Parse a `flock-state` pipe message's args into a [`HookReport`].
///
/// Required: `pane_id` (the target pane) and `state`. The `state` value is one
/// of `idle`/`working`/`blocked`/`unknown` (mirroring herdr's CLI
/// `parse_pane_agent_state`) or `release` to clear authority. Optional `agent`,
/// `source`, and `message` carry display/arbitration metadata.
///
/// Returns `Err` with a human-readable reason on a malformed report so the
/// caller can log it; a bad report is dropped rather than mutating any pane.
pub fn parse_hook_report(args: &BTreeMap<String, String>) -> Result<HookReport, String> {
    let pane_id = args
        .get("pane_id")
        .or_else(|| args.get("pane"))
        .ok_or_else(|| "missing pane_id".to_string())
        .and_then(|raw| parse_pane_id(raw))?;

    let state_raw = args
        .get("state")
        .map(String::as_str)
        .ok_or_else(|| "missing state".to_string())?;

    if state_raw.eq_ignore_ascii_case("release") {
        return Ok(HookReport::Release { pane_id });
    }

    let state = parse_state(state_raw)?;
    let agent_label = args
        .get("agent")
        .map(String::as_str)
        .filter(|label| !label.trim().is_empty())
        .unwrap_or(DEFAULT_AGENT_LABEL)
        .to_string();
    let source = args
        .get("source")
        .map(String::as_str)
        .filter(|source| !source.trim().is_empty())
        .unwrap_or(DEFAULT_SOURCE)
        .to_string();
    let message = args
        .get("message")
        .map(|message| message.trim())
        .filter(|message| !message.is_empty())
        .map(str::to_string);

    Ok(HookReport::State {
        pane_id,
        source,
        agent_label,
        state,
        message,
    })
}

/// Parse the `pane_id` arg into a terminal [`PaneId`].
///
/// Zellij exports the running pane's id as `$ZELLIJ_PANE_ID` (a bare integer;
/// see `zellij-server/src/os_input_output_unix.rs`), which the hooks forward
/// verbatim. herdr's own hooks prefix it `p_<n>`, so tolerate that prefix too
/// for operators reusing herdr's scripts unchanged.
fn parse_pane_id(raw: &str) -> Result<PaneId, String> {
    let digits = raw.trim().strip_prefix("p_").unwrap_or(raw.trim());
    digits
        .parse::<u32>()
        .map(PaneId::Terminal)
        .map_err(|_| format!("invalid pane_id: {raw:?}"))
}

/// Parse a hook `state` value, mirroring herdr's `cli::parse_pane_agent_state`.
fn parse_state(raw: &str) -> Result<AgentState, String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "idle" => Ok(AgentState::Idle),
        "working" => Ok(AgentState::Working),
        "blocked" => Ok(AgentState::Blocked),
        "unknown" => Ok(AgentState::Unknown),
        other => Err(format!(
            "invalid state: {other:?} (expected idle, working, blocked, unknown, or release)"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn parses_a_full_state_report() {
        let report = parse_hook_report(&args(&[
            ("pane_id", "7"),
            ("state", "blocked"),
            ("agent", "claude"),
            ("source", "flock:claude"),
            ("message", "needs approval"),
        ]))
        .expect("valid report");

        assert_eq!(
            report,
            HookReport::State {
                pane_id: PaneId::Terminal(7),
                source: "flock:claude".into(),
                agent_label: "claude".into(),
                state: AgentState::Blocked,
                message: Some("needs approval".into()),
            }
        );
    }

    #[test]
    fn defaults_agent_and_source_when_omitted() {
        // The Phase 5 verification step runs exactly this minimal report.
        let report = parse_hook_report(&args(&[("pane_id", "3"), ("state", "blocked")]))
            .expect("valid report");

        assert_eq!(
            report,
            HookReport::State {
                pane_id: PaneId::Terminal(3),
                source: DEFAULT_SOURCE.into(),
                agent_label: DEFAULT_AGENT_LABEL.into(),
                state: AgentState::Blocked,
                message: None,
            }
        );
    }

    #[test]
    fn release_clears_authority() {
        let report = parse_hook_report(&args(&[("pane_id", "5"), ("state", "release")]))
            .expect("valid report");
        assert_eq!(report, HookReport::Release { pane_id: PaneId::Terminal(5) });
    }

    #[test]
    fn release_is_case_insensitive_and_ignores_agent() {
        let report = parse_hook_report(&args(&[
            ("pane_id", "5"),
            ("state", "RELEASE"),
            ("agent", "claude"),
        ]))
        .expect("valid report");
        assert_eq!(report, HookReport::Release { pane_id: PaneId::Terminal(5) });
    }

    #[test]
    fn tolerates_herdr_style_pane_prefix() {
        let report = parse_hook_report(&args(&[("pane_id", "p_12"), ("state", "working")]))
            .expect("valid report");
        match report {
            HookReport::State { pane_id, .. } => assert_eq!(pane_id, PaneId::Terminal(12)),
            other => panic!("expected a state report, got {other:?}"),
        }
    }

    #[test]
    fn accepts_pane_alias_key() {
        let report =
            parse_hook_report(&args(&[("pane", "9"), ("state", "idle")])).expect("valid report");
        match report {
            HookReport::State { pane_id, state, .. } => {
                assert_eq!(pane_id, PaneId::Terminal(9));
                assert_eq!(state, AgentState::Idle);
            },
            other => panic!("expected a state report, got {other:?}"),
        }
    }

    #[test]
    fn rejects_missing_pane_id() {
        assert!(parse_hook_report(&args(&[("state", "idle")])).is_err());
    }

    #[test]
    fn rejects_missing_state() {
        assert!(parse_hook_report(&args(&[("pane_id", "1")])).is_err());
    }

    #[test]
    fn rejects_unparseable_pane_id() {
        assert!(parse_hook_report(&args(&[("pane_id", "abc"), ("state", "idle")])).is_err());
    }

    #[test]
    fn rejects_unknown_state() {
        assert!(parse_hook_report(&args(&[("pane_id", "1"), ("state", "ka-boom")])).is_err());
    }

    #[test]
    fn state_value_is_case_insensitive() {
        let report = parse_hook_report(&args(&[("pane_id", "1"), ("state", "Working")]))
            .expect("valid report");
        match report {
            HookReport::State { state, .. } => assert_eq!(state, AgentState::Working),
            other => panic!("expected a state report, got {other:?}"),
        }
    }

    #[test]
    fn blank_agent_and_source_fall_back_to_defaults() {
        let report = parse_hook_report(&args(&[
            ("pane_id", "1"),
            ("state", "working"),
            ("agent", "   "),
            ("source", ""),
        ]))
        .expect("valid report");
        match report {
            HookReport::State {
                agent_label, source, ..
            } => {
                assert_eq!(agent_label, DEFAULT_AGENT_LABEL);
                assert_eq!(source, DEFAULT_SOURCE);
            },
            other => panic!("expected a state report, got {other:?}"),
        }
    }
}
