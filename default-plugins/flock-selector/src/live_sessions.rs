//! The Sessions mode's data model and ranking: the live session list reduced
//! to pickable entries, fuzzy-filtered like the other modes.
//!
//! Entries are built in [`crate::State`] from the cross-session
//! `SessionUpdate` list (selector throwaway sessions excluded — switching into
//! one strands the user in a pane-less shell). Ranking mirrors
//! [`crate::codespaces::rank`]: fuzzy over the session name and its
//! home-shortened workspace path with a name-hit bonus; an empty query keeps
//! everything, ordered by name with the current session sunk to the end (it's
//! the one entry switching to does nothing for).

use crate::fuzzy::fuzzy_match;

/// Small bonus for a name hit over a path-only hit, mirroring the project
/// ranking's name-over-path preference.
const NAME_MATCH_BONUS: i32 = 8;

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
/// non-empty query; an empty query orders by name, current session last.
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
    // Best first; ties sink the current session (switching to it is a no-op),
    // then order by name for determinism.
    ranked.sort_by(|a, b| {
        b.rank
            .cmp(&a.rank)
            .then_with(|| a.entry.is_current.cmp(&b.entry.is_current))
            .then_with(|| a.entry.name.cmp(&b.entry.name))
    });
    ranked
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, path: &str, is_current: bool) -> SessionEntry {
        SessionEntry {
            name: name.to_owned(),
            display_path: path.to_owned(),
            is_current,
        }
    }

    #[test]
    fn empty_query_orders_by_name_current_last() {
        let entries = vec![
            entry("zeta", "~/zeta", false),
            entry("alpha", "~/alpha", true),
            entry("mid", "~/mid", false),
        ];
        let ranked = rank(&entries, "");
        let names: Vec<&str> = ranked.iter().map(|r| r.entry.name.as_str()).collect();
        assert_eq!(names, vec!["mid", "zeta", "alpha"]);
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
}
