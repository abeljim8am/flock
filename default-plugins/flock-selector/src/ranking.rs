//! Ordering the candidate projects for the current query.
//!
//! Each candidate gets a blended rank: its fuzzy score against the query (see
//! [`crate::fuzzy`]) plus a bounded slice of its frecency score (see
//! [`crate::frecency`]). The fuzzy score dominates filtering and ordering while
//! the query is non-trivial; the frecency term acts as a tiebreak and — because
//! an empty query gives every candidate the same fuzzy score — becomes the sole
//! ordering signal when the input is empty, so the most-used projects sit
//! nearest the input on open.
//!
//! Matching considers both the basename and the home-shortened path. A name
//! match is preferred (it carries a small bonus over a path-only match), and the
//! UI highlights whichever target(s) matched.

use crate::discovery::Project;
use crate::frecency::FrecencyDb;
use crate::fuzzy::fuzzy_match;

/// Small bonus added when the query matches the basename, so a name hit ranks
/// above a path-only hit of equal shape.
const NAME_MATCH_BONUS: i32 = 8;
/// How heavily frecency weighs in. Applied to `ln(1 + frecency)` so a heavily
/// used project nudges ahead on ties without ever swamping a better text match.
const FRECENCY_WEIGHT: f64 = 4.0;

/// A candidate paired with its computed rank and the match ranges to highlight.
#[derive(Debug, Clone, PartialEq)]
pub struct Ranked<'a> {
    pub project: &'a Project,
    pub rank: f64,
    /// Matched byte ranges within `project.name`.
    pub name_ranges: Vec<(usize, usize)>,
    /// Matched byte ranges within `project.display_path`.
    pub path_ranges: Vec<(usize, usize)>,
}

/// Rank `projects` for `query`, returning the matches best-first. When `query`
/// is non-empty, candidates matching neither name nor path are dropped.
pub fn rank<'a>(
    projects: &'a [Project],
    query: &str,
    frecency: &FrecencyDb,
    now: u64,
) -> Vec<Ranked<'a>> {
    let query = query.trim();
    let mut ranked: Vec<Ranked<'a>> = Vec::with_capacity(projects.len());

    for project in projects {
        let name_match = fuzzy_match(query, &project.name);
        let path_match = fuzzy_match(query, &project.display_path);

        if !query.is_empty() && name_match.is_none() && path_match.is_none() {
            continue;
        }

        let name_score = name_match.as_ref().map(|m| m.score + NAME_MATCH_BONUS);
        let path_score = path_match.as_ref().map(|m| m.score);
        let fuzzy_score = name_score.into_iter().chain(path_score).max().unwrap_or(0);

        let frec = frecency.score(&project.path.to_string_lossy(), now);
        let rank = fuzzy_score as f64 + FRECENCY_WEIGHT * (1.0 + frec).ln();

        ranked.push(Ranked {
            project,
            rank,
            name_ranges: name_match.map(|m| m.ranges).unwrap_or_default(),
            path_ranges: path_match.map(|m| m.ranges).unwrap_or_default(),
        });
    }

    // Best first; ties broken by name then path for a stable, deterministic order.
    ranked.sort_by(|a, b| {
        b.rank
            .partial_cmp(&a.rank)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.project.name.cmp(&b.project.name))
            .then_with(|| a.project.path.cmp(&b.project.path))
    });
    ranked
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn projects(paths: &[&str]) -> Vec<Project> {
        paths
            .iter()
            .map(|p| Project::from_path(Path::new(p)))
            .collect()
    }

    #[test]
    fn empty_query_keeps_all_ordered_by_frecency() {
        std::env::set_var("HOME", "/h");
        let ps = projects(&["/h/alpha", "/h/beta", "/h/gamma"]);
        let mut db = FrecencyDb::default();
        db.bump("/h/gamma", 1000);
        db.bump("/h/gamma", 1000);
        let out = rank(&ps, "", &db, 1000);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].project.name, "gamma"); // most-used floats to the front
    }

    #[test]
    fn query_filters_non_matches() {
        let ps = projects(&["/x/zellij", "/x/herdr", "/x/notes"]);
        let out = rank(&ps, "zj", &FrecencyDb::default(), 0);
        let names: Vec<&str> = out.iter().map(|r| r.project.name.as_str()).collect();
        assert_eq!(names, vec!["zellij"]);
    }

    #[test]
    fn name_match_beats_path_only_match() {
        std::env::set_var("HOME", "/home");
        // "api" matches the basename of the first, but only the path of the second.
        let ps = projects(&["/home/api", "/home/api/frontend"]);
        let out = rank(&ps, "api", &FrecencyDb::default(), 0);
        assert_eq!(out[0].project.name, "api");
    }

    #[test]
    fn frecency_breaks_fuzzy_ties() {
        let ps = projects(&["/x/app-one", "/x/app-two"]);
        let mut db = FrecencyDb::default();
        db.bump("/x/app-two", 500);
        let out = rank(&ps, "app", &db, 500);
        assert_eq!(out[0].project.name, "app-two");
    }
}
