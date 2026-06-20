//! Fuzzy filtering + tag filtering + frecency ranking for the host list.
//!
//! The query supports `tag:NAME` tokens (atuin-style filters): every `tag:` token must match
//! one of a host's tags (case-insensitive, exact); the remaining words are fuzzy-matched.
//!
//! - No fuzzy text: tag-filtered hosts ordered by `sort` (frecency or name).
//! - With fuzzy text: only fuzzy matches, ordered by match score, frecency breaking ties.

use std::cmp::Ordering;

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

use crate::config::Sort;
use crate::model::Host;
use crate::state::FrecencyState;

/// A fresh fuzzy matcher.
pub fn matcher() -> Matcher {
    Matcher::new(Config::DEFAULT)
}

/// Split a query into `tag:` filters, an optional single `site:` filter, and the remaining
/// fuzzy text.
pub fn parse_query(query: &str) -> (Vec<String>, Option<String>, String) {
    let mut tags = Vec::new();
    let mut site: Option<String> = None;
    let mut rest: Vec<&str> = Vec::new();
    for tok in query.split_whitespace() {
        if let Some(t) = tok.strip_prefix("tag:").filter(|t| !t.is_empty()) {
            tags.push(t.to_lowercase());
        } else if let Some(s) = tok.strip_prefix("site:").filter(|s| !s.is_empty()) {
            site = Some(s.to_lowercase()); // single-valued: the last one wins
        } else {
            rest.push(tok);
        }
    }
    (tags, site, rest.join(" "))
}

/// Host indices (into `hosts`) in display order.
pub fn rank(
    hosts: &[Host],
    query: &str,
    state: &FrecencyState,
    decay: f64,
    sort: Sort,
) -> Vec<usize> {
    let (tag_filters, site_filter, fuzzy) = parse_query(query);
    let candidates: Vec<usize> = (0..hosts.len())
        .filter(|&i| {
            has_all_tags(&hosts[i], &tag_filters) && matches_site(&hosts[i], site_filter.as_deref())
        })
        .collect();

    let fq = fuzzy.trim();
    if fq.is_empty() {
        let mut idx = candidates;
        match sort {
            Sort::Name => idx.sort_by(|&a, &b| name_asc(&hosts[a], &hosts[b])),
            Sort::Frecency => idx.sort_by(|&a, &b| {
                frecency_desc(state, &hosts[a], &hosts[b], decay)
                    .then_with(|| name_asc(&hosts[a], &hosts[b]))
            }),
        }
        return idx;
    }

    let mut matcher = matcher();
    let pattern = Pattern::parse(fq, CaseMatching::Smart, Normalization::Smart);
    let mut buf = Vec::new();
    let mut scored: Vec<(usize, u32)> = Vec::new();
    for &i in &candidates {
        let hay = hosts[i].search_haystack();
        let hs = Utf32Str::new(&hay, &mut buf);
        if let Some(score) = pattern.score(hs, &mut matcher) {
            scored.push((i, score));
        }
    }
    scored.sort_by(|&(ia, sa), &(ib, sb)| {
        sb.cmp(&sa)
            .then_with(|| frecency_desc(state, &hosts[ia], &hosts[ib], decay))
            .then_with(|| name_asc(&hosts[ia], &hosts[ib]))
    });
    scored.into_iter().map(|(i, _)| i).collect()
}

/// Indices of `labels` that fuzzy-match `query`, ranked best-first. An empty query returns
/// every index in original order. Generic helper (used by the file browser).
pub fn fuzzy_filter(labels: &[String], query: &str) -> Vec<usize> {
    let q = query.trim();
    if q.is_empty() {
        return (0..labels.len()).collect();
    }
    let mut matcher = matcher();
    let pattern = Pattern::parse(q, CaseMatching::Smart, Normalization::Smart);
    let mut buf = Vec::new();
    let mut scored: Vec<(usize, u32)> = Vec::new();
    for (i, label) in labels.iter().enumerate() {
        let hs = Utf32Str::new(label, &mut buf);
        if let Some(score) = pattern.score(hs, &mut matcher) {
            scored.push((i, score));
        }
    }
    scored.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    scored.into_iter().map(|(i, _)| i).collect()
}

/// Matched character positions within `text` for `query` (empty if no match / no query).
pub fn match_indices(text: &str, query: &str, matcher: &mut Matcher) -> Vec<u32> {
    let q = query.trim();
    if q.is_empty() {
        return Vec::new();
    }
    let pattern = Pattern::parse(q, CaseMatching::Smart, Normalization::Smart);
    let mut buf = Vec::new();
    let hs = Utf32Str::new(text, &mut buf);
    let mut indices = Vec::new();
    if pattern.indices(hs, matcher, &mut indices).is_some() {
        indices.sort_unstable();
        indices.dedup();
        indices
    } else {
        Vec::new()
    }
}

fn has_all_tags(h: &Host, tags: &[String]) -> bool {
    tags.iter()
        .all(|t| h.tags.iter().any(|ht| ht.to_lowercase() == *t))
}

/// `None` (no filter) matches everything; otherwise the host's site must equal `want`
/// (case-insensitive). `want` is already lowercased by `parse_query`.
fn matches_site(h: &Host, want: Option<&str>) -> bool {
    match want {
        None => true,
        Some(w) => h.site.as_deref().is_some_and(|s| s.eq_ignore_ascii_case(w)),
    }
}

/// Higher frecency first.
fn frecency_desc(state: &FrecencyState, a: &Host, b: &Host, decay: f64) -> Ordering {
    let sa = state.score(&a.id, decay);
    let sb = state.score(&b.id, decay);
    sb.partial_cmp(&sa).unwrap_or(Ordering::Equal)
}

fn name_asc(a: &Host, b: &Host) -> Ordering {
    a.name.to_lowercase().cmp(&b.name.to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Sort;
    use crate::model::Host;
    use crate::state::{FrecencyState, HostStat};

    fn sample() -> Vec<Host> {
        let mut web = Host::new("prod-web", "10.0.0.1");
        web.tags = vec!["prod".into(), "web".into()];
        let mut db = Host::new("prod-db", "10.0.0.2");
        db.tags = vec!["prod".into(), "db".into()];
        let mut stg = Host::new("staging-web", "10.0.1.1");
        stg.tags = vec!["staging".into(), "web".into()];
        let bastion = Host::new("bastion", "bastion.example.com");
        vec![web, db, stg, bastion]
    }

    #[test]
    fn empty_query_frecency_then_name() {
        let hosts = sample();
        let mut state = FrecencyState::default();
        let now = crate::state::now_unix();
        state.stats.insert(
            hosts[1].id.clone(),
            HostStat {
                use_count: 99,
                last_used: now,
            },
        );
        let order = rank(&hosts, "", &state, 0.2, Sort::Frecency);
        assert_eq!(order[0], 1);
    }

    #[test]
    fn empty_query_name_sort() {
        let hosts = sample();
        let state = FrecencyState::default();
        let order = rank(&hosts, "", &state, 0.2, Sort::Name);
        let names: Vec<&str> = order.iter().map(|&i| hosts[i].name.as_str()).collect();
        assert_eq!(names, vec!["bastion", "prod-db", "prod-web", "staging-web"]);
    }

    #[test]
    fn fuzzy_filters_to_matches() {
        let hosts = sample();
        let state = FrecencyState::default();
        let order = rank(&hosts, "prod", &state, 0.2, Sort::Frecency);
        let names: Vec<&str> = order.iter().map(|&i| hosts[i].name.as_str()).collect();
        assert!(names.contains(&"prod-web"));
        assert!(!names.contains(&"bastion"));
    }

    #[test]
    fn tag_token_filters() {
        let hosts = sample();
        let state = FrecencyState::default();
        // tag:web matches prod-web and staging-web only
        let order = rank(&hosts, "tag:web", &state, 0.2, Sort::Name);
        let names: Vec<&str> = order.iter().map(|&i| hosts[i].name.as_str()).collect();
        assert_eq!(names, vec!["prod-web", "staging-web"]);
    }

    #[test]
    fn tag_token_plus_fuzzy() {
        let hosts = sample();
        let state = FrecencyState::default();
        // tag:web narrows to the two -web hosts, then fuzzy "staging"
        let order = rank(&hosts, "tag:web staging", &state, 0.2, Sort::Frecency);
        let names: Vec<&str> = order.iter().map(|&i| hosts[i].name.as_str()).collect();
        assert_eq!(names, vec!["staging-web"]);
    }

    #[test]
    fn multiple_tags_are_anded() {
        let hosts = sample();
        let state = FrecencyState::default();
        let order = rank(&hosts, "tag:prod tag:db", &state, 0.2, Sort::Name);
        let names: Vec<&str> = order.iter().map(|&i| hosts[i].name.as_str()).collect();
        assert_eq!(names, vec!["prod-db"]);
    }

    #[test]
    fn parse_query_splits_tags_site_and_text() {
        let (tags, site, rest) = parse_query("tag:prod web site:dc1 tag:db x");
        assert_eq!(tags, vec!["prod", "db"]);
        assert_eq!(site.as_deref(), Some("dc1"));
        assert_eq!(rest, "web x");
    }

    #[test]
    fn site_token_filters_case_insensitively() {
        let mut a = Host::new("web1", "10.0.0.1");
        a.site = Some("dc1".into());
        let mut b = Host::new("web2", "10.0.0.2");
        b.site = Some("dc2".into());
        let plain = Host::new("plain", "10.0.0.3"); // no site
        let hosts = vec![a, b, plain];
        let state = FrecencyState::default();
        let order = rank(&hosts, "site:DC1", &state, 0.2, Sort::Name);
        let names: Vec<&str> = order.iter().map(|&i| hosts[i].name.as_str()).collect();
        assert_eq!(names, vec!["web1"]);
    }

    #[test]
    fn match_indices_marks_prefix() {
        let mut m = matcher();
        assert_eq!(match_indices("prod-web", "prod", &mut m), vec![0, 1, 2, 3]);
    }
}
