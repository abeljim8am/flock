//! A small subsequence fuzzy matcher with match-range reporting.
//!
//! The matcher is case-insensitive and greedy left-to-right: a query matches a
//! target when its characters appear in order. The score rewards matches that
//! read as "intentional" — consecutive runs, matches at word boundaries
//! (after `/ - _ . space` or a camelCase hump), and a match at the very start —
//! so `zj` ranks `zellij` above `cozy-jar`. Alongside the score it returns the
//! matched byte ranges so the UI can highlight them.
//!
//! It is deliberately simple (no full Smith–Waterman DP): the candidate lists
//! here are folder names, where greedy subsequence matching with boundary
//! bonuses is more than good enough and easy to reason about / test.

/// A successful fuzzy match: its score (higher is better) and the matched byte
/// ranges into the target, in ascending order, merged where adjacent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FuzzyMatch {
    pub score: i32,
    pub ranges: Vec<(usize, usize)>,
}

// Scoring weights, in the spirit of fzf/skim's bonuses.
const SCORE_MATCH: i32 = 16;
const BONUS_CONSECUTIVE: i32 = 12;
const BONUS_BOUNDARY: i32 = 10;
const BONUS_START: i32 = 16;
const PENALTY_LEADING_GAP: i32 = -2; // per skipped char before the first match
const PENALTY_GAP: i32 = -1; // per skipped char between matches

/// Match `query` against `target`. An empty query matches everything with score
/// 0 and no ranges. Returns `None` when `query` is not a subsequence of `target`.
pub fn fuzzy_match(query: &str, target: &str) -> Option<FuzzyMatch> {
    if query.is_empty() {
        return Some(FuzzyMatch {
            score: 0,
            ranges: Vec::new(),
        });
    }

    // Work over chars with their byte offsets so reported ranges are valid byte
    // slices even with multibyte content.
    let tchars: Vec<(usize, char)> = target.char_indices().collect();
    let qchars: Vec<char> = query.chars().collect();

    let mut score = 0;
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut ti = 0usize; // index into tchars
    let mut qi = 0usize; // index into qchars
    let mut prev_match_ti: Option<usize> = None;
    let mut leading = true;

    while qi < qchars.len() {
        let want = qchars[qi];
        // Advance through the target to the next char matching `want`.
        let start_ti = ti;
        let mut found = None;
        while ti < tchars.len() {
            if eq_ci(tchars[ti].1, want) {
                found = Some(ti);
                break;
            }
            ti += 1;
        }
        let mti = found?; // no match for this query char ⇒ overall miss
        let gap = mti - start_ti;

        score += SCORE_MATCH;
        if leading {
            score += PENALTY_LEADING_GAP * gap as i32;
        } else {
            score += PENALTY_GAP * gap as i32;
        }

        if mti == 0 {
            score += BONUS_START;
        } else if is_boundary(&tchars, mti) {
            score += BONUS_BOUNDARY;
        }
        if prev_match_ti == Some(mti.wrapping_sub(1)) && mti > 0 {
            score += BONUS_CONSECUTIVE;
        }

        // Record the byte range of this single matched char.
        let byte_start = tchars[mti].0;
        let byte_end = tchars.get(mti + 1).map(|(b, _)| *b).unwrap_or(target.len());
        push_range(&mut ranges, byte_start, byte_end);

        prev_match_ti = Some(mti);
        leading = false;
        ti = mti + 1;
        qi += 1;
    }

    Some(FuzzyMatch { score, ranges })
}

/// Case-insensitive char comparison (ASCII fast path covers folder names).
fn eq_ci(a: char, b: char) -> bool {
    a == b || a.to_ascii_lowercase() == b.to_ascii_lowercase()
}

/// Whether the char at `i` starts a "word": preceded by a separator, or the
/// lowercase→uppercase camelCase hump.
fn is_boundary(tchars: &[(usize, char)], i: usize) -> bool {
    if i == 0 {
        return true;
    }
    let prev = tchars[i - 1].1;
    let cur = tchars[i].1;
    matches!(prev, '/' | '-' | '_' | '.' | ' ' | '\\')
        || (prev.is_ascii_lowercase() && cur.is_ascii_uppercase())
}

/// Append a byte range, merging it into the previous range when adjacent so a
/// consecutive run renders as one highlighted span.
fn push_range(ranges: &mut Vec<(usize, usize)>, start: usize, end: usize) {
    if let Some(last) = ranges.last_mut() {
        if last.1 == start {
            last.1 = end;
            return;
        }
    }
    ranges.push((start, end));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_query_matches_with_zero_score() {
        let m = fuzzy_match("", "anything").unwrap();
        assert_eq!(m.score, 0);
        assert!(m.ranges.is_empty());
    }

    #[test]
    fn non_subsequence_misses() {
        assert!(fuzzy_match("xyz", "zellij").is_none());
        assert!(fuzzy_match("zl x", "zellij").is_none());
    }

    #[test]
    fn matches_subsequence_and_reports_ranges() {
        let m = fuzzy_match("zl", "zellij").unwrap();
        // z @0, l @2 ⇒ two separate ranges.
        assert_eq!(m.ranges, vec![(0, 1), (2, 3)]);
    }

    #[test]
    fn consecutive_run_merges_into_one_range() {
        let m = fuzzy_match("zel", "zellij").unwrap();
        assert_eq!(m.ranges, vec![(0, 3)]);
    }

    #[test]
    fn prefix_outranks_scattered_match() {
        let prefix = fuzzy_match("zj", "zellij").unwrap();
        let scattered = fuzzy_match("zj", "cozy-jar").unwrap();
        assert!(
            prefix.score > scattered.score,
            "prefix {} should beat scattered {}",
            prefix.score,
            scattered.score
        );
    }

    #[test]
    fn word_boundary_beats_mid_word() {
        // "ap" at the start of a segment ("/x/api") should beat mid-word ("scrap").
        let boundary = fuzzy_match("ap", "my/api").unwrap();
        let midword = fuzzy_match("ap", "scrap").unwrap();
        assert!(boundary.score > midword.score);
    }

    #[test]
    fn case_insensitive() {
        let m = fuzzy_match("ZJ", "zellij").unwrap();
        assert_eq!(m.ranges.first(), Some(&(0, 1)));
    }
}
