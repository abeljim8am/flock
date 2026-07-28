//! The Sessions mode's data model and ranking: the live session list reduced
//! to pickable entries, fuzzy-filtered like the other modes.
//!
//! Entries are built in [`crate::State`] from the cross-session
//! `SessionUpdate` list (selector throwaway sessions excluded — switching into
//! one strands the user in a pane-less shell). Ranking mirrors
//! [`crate::codespaces::rank`]: fuzzy over the session name and its
//! home-shortened workspace path with a name-hit bonus. Fuzzy ties break by
//! agent attention (a session whose agent is blocked on the user lists first),
//! so an empty query — where everything scores 0 — orders the whole list by
//! attention. Sessions that enter the same attention bucket are kept in FIFO
//! order, so the tab doubles as an attention queue instead of reshuffling
//! alphabetically whenever agent state changes.

use std::collections::BTreeMap;

use zellij_tile::prelude::{AgentRunState, PaneAgentStatus, PaneId};

use crate::fuzzy::fuzzy_match;

/// Small bonus for a name hit over a path-only hit, mirroring the project
/// ranking's name-over-path preference.
const NAME_MATCH_BONUS: i32 = 8;

/// A session's agent attention bucket. Selector priority is Blocked >
/// Done-unseen > Idle(stopped) > Working > none: an agent that is still running
/// does not need checking before one that has stopped. Ordered by ascending
/// priority so the highest discriminant wins.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum SessionActivity {
    /// No agents published for the session (also: its flock-sidebar isn't
    /// running, so nothing was published).
    #[default]
    None,
    /// At least one agent is actively working.
    Running,
    /// At least one agent is idle and already seen.
    Stopped,
    /// At least one agent finished in the background and hasn't been looked at
    /// yet (and none is blocked) — worth a glance.
    DoneUnseen,
    /// At least one agent is blocked waiting on the user — the most
    /// attention-worthy state, so it wins over everything else.
    Blocked,
}

/// Roll a session's published per-pane agent state (the cross-session bus
/// carried on `SessionInfo.agent_states`) into its attention bucket. Empty
/// map ⇒ [`SessionActivity::None`].
pub fn session_activity(states: &BTreeMap<PaneId, PaneAgentStatus>) -> SessionActivity {
    let mut activity = SessionActivity::None;
    for status in states.values() {
        let this = match status.state {
            AgentRunState::Blocked => SessionActivity::Blocked,
            AgentRunState::Working => SessionActivity::Running,
            AgentRunState::Idle if !status.seen => SessionActivity::DoneUnseen,
            // Idle-seen or Unknown: an agent is present but needs no attention.
            _ => SessionActivity::Stopped,
        };
        activity = activity.max(this);
    }
    activity
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct QueuedActivity {
    activity: SessionActivity,
    order: u64,
}

/// Stable FIFO positions for sessions within each activity bucket.
///
/// A new session, or a session whose rolled-up activity changes, joins the back
/// of its new bucket. Repeated snapshots with the same activity preserve its
/// position. The caller controls the order of newly observed sessions, which
/// lets startup seed the queue by session age even though the host snapshot is
/// name-sorted.
#[derive(Debug, Default)]
pub struct ActivityQueue {
    entries: BTreeMap<String, QueuedActivity>,
    next_order: u64,
}

impl ActivityQueue {
    pub fn update<'a>(&mut self, activities: impl IntoIterator<Item = (&'a str, SessionActivity)>) {
        let activities: Vec<(&str, SessionActivity)> = activities.into_iter().collect();
        self.entries
            .retain(|name, _| activities.iter().any(|(live, _)| live == name));

        for (name, activity) in activities {
            let activity_changed = self
                .entries
                .get(name)
                .is_none_or(|queued| queued.activity != activity);
            if activity_changed {
                self.entries.insert(
                    name.to_owned(),
                    QueuedActivity {
                        activity,
                        order: self.next_order,
                    },
                );
                self.next_order = self.next_order.saturating_add(1);
            }
        }
    }

    pub fn order(&self, name: &str) -> u64 {
        self.entries
            .get(name)
            .map(|queued| queued.order)
            .unwrap_or(u64::MAX)
    }
}

/// One live session, reduced to what the picker shows and switches to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEntry {
    /// The session name (the switch target).
    pub name: String,
    /// The session's workspace root, home-shortened for display. Empty when
    /// the root is unknown.
    pub display_path: String,
    /// Whether this is the session the picker is running in.
    pub is_current: bool,
    /// The session's rolled-up agent attention, the ranking tiebreak under
    /// the fuzzy score and the row's status dot.
    pub activity: SessionActivity,
    /// FIFO position assigned when the session entered its current activity.
    pub queue_order: u64,
}

/// A session paired with its rank and the match ranges to highlight.
#[derive(Debug, Clone, PartialEq)]
pub struct RankedSession<'a> {
    pub entry: &'a SessionEntry,
    pub rank: i32,
    /// Matched byte ranges within `entry.name`.
    pub name_ranges: Vec<(usize, usize)>,
    /// Matched byte ranges within `entry.display_path`.
    pub path_ranges: Vec<(usize, usize)>,
}

/// Rank `entries` for `query`, best-first. Non-matches are dropped for a
/// non-empty query; an empty query keeps everything, ordered by agent
/// attention (blocked first) then FIFO position.
pub fn rank<'a>(entries: &'a [SessionEntry], query: &str) -> Vec<RankedSession<'a>> {
    let query = query.trim();
    let mut ranked: Vec<RankedSession<'a>> = Vec::with_capacity(entries.len());
    for entry in entries {
        let name_match = fuzzy_match(query, &entry.name);
        let path_match = fuzzy_match(query, &entry.display_path);
        if !query.is_empty() && name_match.is_none() && path_match.is_none() {
            continue;
        }
        let name_score = name_match.as_ref().map(|m| m.score + NAME_MATCH_BONUS);
        let path_score = path_match.as_ref().map(|m| m.score);
        let rank = name_score.into_iter().chain(path_score).max().unwrap_or(0);
        ranked.push(RankedSession {
            entry,
            rank,
            name_ranges: name_match.map(|m| m.ranges).unwrap_or_default(),
            path_ranges: path_match.map(|m| m.ranges).unwrap_or_default(),
        });
    }
    // Best fuzzy match first, then the most attention-worthy agents. The
    // current-session flag intentionally does not participate: sinking it used
    // to hide a blocked current agent below inactive sessions. Within an
    // activity bucket, older transitions are served first.
    ranked.sort_by(|a, b| {
        b.rank
            .cmp(&a.rank)
            .then_with(|| b.entry.activity.cmp(&a.entry.activity))
            .then_with(|| a.entry.queue_order.cmp(&b.entry.queue_order))
            .then_with(|| a.entry.name.cmp(&b.entry.name))
    });
    ranked
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, path: &str, is_current: bool) -> SessionEntry {
        entry_with_activity(name, path, is_current, SessionActivity::None)
    }

    fn entry_with_activity(
        name: &str,
        path: &str,
        is_current: bool,
        activity: SessionActivity,
    ) -> SessionEntry {
        SessionEntry {
            name: name.to_owned(),
            display_path: path.to_owned(),
            is_current,
            activity,
            queue_order: 0,
        }
    }

    #[test]
    fn empty_query_uses_fifo_order_regardless_of_current_session() {
        let entries = vec![
            SessionEntry {
                queue_order: 2,
                ..entry("zeta", "~/zeta", false)
            },
            SessionEntry {
                queue_order: 0,
                ..entry("alpha", "~/alpha", true)
            },
            SessionEntry {
                queue_order: 1,
                ..entry("mid", "~/mid", false)
            },
        ];
        let ranked = rank(&entries, "");
        let names: Vec<&str> = ranked.iter().map(|r| r.entry.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "mid", "zeta"]);
    }

    #[test]
    fn filters_non_matches_and_prefers_name_hits() {
        let entries = vec![
            entry("webapp", "~/work/webapp", false),
            entry("unrelated", "~/work/webapp-tools", false),
            entry("zzz", "~/zzz", false),
        ];
        let ranked = rank(&entries, "webapp");
        let names: Vec<&str> = ranked.iter().map(|r| r.entry.name.as_str()).collect();
        assert_eq!(names, vec!["webapp", "unrelated"]);
        assert!(!ranked[0].name_ranges.is_empty());
        assert!(!ranked[1].path_ranges.is_empty());
    }

    #[test]
    fn empty_query_orders_by_attention_then_fifo() {
        let entries = vec![
            SessionEntry {
                queue_order: 5,
                ..entry_with_activity("idle", "~/idle", false, SessionActivity::Stopped)
            },
            SessionEntry {
                queue_order: 4,
                ..entry_with_activity("no-agents", "~/none", false, SessionActivity::None)
            },
            SessionEntry {
                queue_order: 3,
                ..entry_with_activity("busy", "~/busy", false, SessionActivity::Running)
            },
            SessionEntry {
                queue_order: 1,
                ..entry_with_activity("zz-stuck", "~/stuck", false, SessionActivity::Blocked)
            },
            SessionEntry {
                queue_order: 2,
                ..entry_with_activity("aa-stuck", "~/stuck2", false, SessionActivity::Blocked)
            },
            SessionEntry {
                queue_order: 0,
                ..entry_with_activity("done", "~/done", false, SessionActivity::DoneUnseen)
            },
        ];
        let ranked = rank(&entries, "");
        let names: Vec<&str> = ranked.iter().map(|r| r.entry.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["zz-stuck", "aa-stuck", "done", "idle", "busy", "no-agents"]
        );
    }

    #[test]
    fn blocked_current_session_still_wins_by_attention() {
        let entries = vec![
            entry_with_activity("here", "~/here", true, SessionActivity::Blocked),
            entry_with_activity("calm", "~/calm", false, SessionActivity::None),
        ];
        let ranked = rank(&entries, "");
        let names: Vec<&str> = ranked.iter().map(|r| r.entry.name.as_str()).collect();
        assert_eq!(names, vec!["here", "calm"]);
    }

    #[test]
    fn fuzzy_score_beats_attention_for_a_query() {
        let entries = vec![
            entry_with_activity("webapp", "~/work/webapp", false, SessionActivity::None),
            entry_with_activity(
                "unrelated",
                "~/work/webapp-tools",
                false,
                SessionActivity::Blocked,
            ),
        ];
        let ranked = rank(&entries, "webapp");
        let names: Vec<&str> = ranked.iter().map(|r| r.entry.name.as_str()).collect();
        assert_eq!(names, vec!["webapp", "unrelated"]);
    }

    #[test]
    fn rollup_picks_most_attention_worthy_state() {
        use zellij_tile::prelude::PaneId;

        let status = |state, seen| PaneAgentStatus {
            state,
            label: "claude".to_owned(),
            seen,
        };
        let mut states = BTreeMap::new();
        assert_eq!(session_activity(&states), SessionActivity::None);

        states.insert(PaneId::Terminal(1), status(AgentRunState::Idle, true));
        assert_eq!(session_activity(&states), SessionActivity::Stopped);

        states.clear();
        states.insert(PaneId::Terminal(2), status(AgentRunState::Working, true));
        assert_eq!(session_activity(&states), SessionActivity::Running);

        states.insert(PaneId::Terminal(1), status(AgentRunState::Idle, true));
        assert_eq!(session_activity(&states), SessionActivity::Stopped);

        states.insert(PaneId::Terminal(3), status(AgentRunState::Idle, false));
        assert_eq!(session_activity(&states), SessionActivity::DoneUnseen);

        states.insert(PaneId::Terminal(4), status(AgentRunState::Blocked, false));
        assert_eq!(session_activity(&states), SessionActivity::Blocked);
    }

    #[test]
    fn activity_changes_join_the_back_of_the_new_bucket() {
        let mut queue = ActivityQueue::default();
        queue.update([
            ("older", SessionActivity::Blocked),
            ("newer", SessionActivity::Running),
        ]);
        let older_first_order = queue.order("older");

        queue.update([
            ("older", SessionActivity::Blocked),
            ("newer", SessionActivity::Blocked),
        ]);

        assert_eq!(queue.order("older"), older_first_order);
        assert!(queue.order("older") < queue.order("newer"));
    }

    #[test]
    fn unchanged_snapshots_preserve_fifo_positions_and_removed_names_are_fresh() {
        let mut queue = ActivityQueue::default();
        queue.update([
            ("first", SessionActivity::Running),
            ("second", SessionActivity::Running),
        ]);
        let first_order = queue.order("first");
        let second_order = queue.order("second");

        queue.update([
            ("second", SessionActivity::Running),
            ("first", SessionActivity::Running),
        ]);
        assert_eq!(queue.order("first"), first_order);
        assert_eq!(queue.order("second"), second_order);

        queue.update([("second", SessionActivity::Running)]);
        queue.update([
            ("first", SessionActivity::Running),
            ("second", SessionActivity::Running),
        ]);
        assert!(queue.order("second") < queue.order("first"));
    }
}
