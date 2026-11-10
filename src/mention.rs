//! Deterministic, LLM-free mention detector (OxiRS §1.8).
//!
//! For each memory, find the symbols / file paths it references by exact
//! token match against a `KnownIds` index, and return their symbol ids.
//! No regex alternation, no model, no I/O.

use crate::store::ladybug::KnownIds;
use std::collections::HashMap;

const MENTION_CAP: usize = 32;

const STOPWORDS: &[&str] = &[
    "id", "x", "i", "t", "n", "ok", "err", "new", "get", "set",
];

/// Strip surrounding punctuation from a token, returning the inner string.
fn strip_surrounding_punct(s: &str) -> String {
    let mut start = 0usize;
    let mut end = s.len();
    let bytes = s.as_bytes();
    while start < end {
        let c = bytes[start] as char;
        if matches!(c, '`' | '"' | '\'' | '(' | '[' | '{' | '<' | ',' | ';' | ':' | '.' | '!' | '?') {
            start += 1;
        } else {
            break;
        }
    }
    while end > start {
        let c = bytes[end - 1] as char;
        if matches!(c, '`' | '"' | '\'' | ')' | ']' | '}' | '>' | ',' | ';' | ':' | '.' | '!' | '?') {
            end -= 1;
        } else {
            break;
        }
    }
    s[start..end].to_string()
}

fn is_short_alias(s: &str) -> bool {
    !s.contains('/') && !s.contains('#') && !s.contains(':')
}

fn alias_allowed(s: &str) -> bool {
    s.len() >= 3 && !STOPWORDS.contains(&s)
}

fn basename(s: &str) -> &str {
    s.rsplit('/').next().unwrap_or(s)
}

fn strip_hash(s: &str) -> &str {
    s.split('@').next().unwrap_or(s)
}

/// Build the candidate map (candidate string -> symbol id) from a `KnownIds`
/// index. Collisions drop the candidate entirely.
pub fn build_candidates(index: &KnownIds) -> HashMap<String, String> {
    // First pass: collect (candidate, id) pairs.
    let mut pairs: Vec<(String, String)> = Vec::new();
    for id in index.ids() {
        let logical = strip_hash(crate::parser::generation::logical_path_of(id));
        let logical = logical.to_string();
        // Full logical path (without hash).
        pairs.push((logical.clone(), id.to_string()));
        // File path portion before '#'.
        let file_path = logical.split('#').next().unwrap_or("").to_string();
        if !file_path.is_empty() && file_path != logical {
            if !is_short_alias(&file_path) || alias_allowed(&file_path) {
                pairs.push((file_path.clone(), id.to_string()));
            }
        }
        // Symbol name after kind: (e.g. functions:bar -> bar)
        let rest = logical.split('#').nth(1).unwrap_or("");
        let name = if rest.contains(':') {
            rest.split(':').last().unwrap_or(rest)
        } else {
            rest
        };
        let name = name.to_string();
        if !name.is_empty() && name != logical && name != file_path {
            if !is_short_alias(&name) || alias_allowed(&name) {
                pairs.push((name.clone(), id.to_string()));
            }
        }
        // Short aliases (final path components), guard + uniqueness handled later.
        if !file_path.is_empty() {
            let alias = basename(&file_path).to_string();
            if alias != file_path && alias_allowed(&alias) {
                pairs.push((alias, id.to_string()));
            }
        }
        if !name.is_empty() {
            let alias = basename(&name).to_string();
            if alias != name && alias_allowed(&alias) {
                pairs.push((alias, id.to_string()));
            }
        }
    }

    // Count occurrences.
    let mut counts: HashMap<String, usize> = HashMap::new();
    for (cand, _) in &pairs {
        *counts.entry(cand.clone()).or_insert(0) += 1;
    }

    // Keep only unambiguous candidates.
    let mut out: HashMap<String, String> = HashMap::new();
    for (cand, id) in pairs {
        if counts.get(&cand).copied().unwrap_or(0) == 1 {
            // Short aliases already guarded above; guard again for safety.
            if is_short_alias(&cand) && !alias_allowed(&cand) {
                continue;
            }
            out.insert(cand, id);
        }
    }
    out
}

/// Whether `needle` occurs in `text` at a position whose immediate left and
/// right neighbors (when present) are neither path/identifier separators
/// (`/`, `\`, `:`, `-`, `_`, `.`) nor alphanumeric characters.
fn has_unattached_occurrence(text: &str, needle: &str) -> bool {
    for (pos, mat) in text.match_indices(needle) {
        let left = text[..pos].chars().next_back();
        let right = text[pos + mat.len()..].chars().next();
        let glued = |c: char| {
            matches!(c, '/' | '\\' | ':' | '-' | '_' | '.') || c.is_alphanumeric()
        };
        if !left.map_or(false, glued) && !right.map_or(false, glued) {
            return true;
        }
    }
    false
}

/// The index surface the mention detector reads. Implementors that memoize
/// the derived candidate map hand it back via [`SymbolIndex::cached_candidates`];
/// the detector falls back to a fresh [`build_candidates`] derivation when
/// the cache is absent.
pub trait SymbolIndex {
    /// Returns the candidate map when the implementor has it cached.
    fn cached_candidates(&self) -> Option<std::collections::HashMap<String, String>> {
        None
    }
}

/// What the detector found: the ids it keeps, and how many distinct matches it
/// had to leave out because of [`MENTION_CAP`].
pub struct Mentions {
    pub ids: Vec<String>,
    pub dropped: usize,
}

/// How specific the text that matched a symbol was. Lower is more specific:
/// a full symbol path (`src/a.rs#functions:f`) over a file path or a
/// `kind:name` form over a bare short alias, which any prose can contain.
fn specificity(candidate: &str) -> u8 {
    if candidate.contains('#') {
        0
    } else if candidate.contains('/') || candidate.contains(':') {
        1
    } else {
        2
    }
}

/// Detect mention symbol ids in `content` and `location`.
pub fn detect_mentions(content: &str, location: &str, index: &KnownIds) -> Vec<String> {
    detect_mentions_report(content, location, index).ids
}

/// Same detection, reporting how many distinct matches the cap left out.
///
/// With [`MENTION_CAP`] or fewer matches the result is the matches in text
/// order, as it always was. With more, the cap keeps the most specific
/// matches (see [`specificity`]; ties keep text order) instead of the first
/// ones in the text, and the kept ids are still returned in text order.
pub fn detect_mentions_report(content: &str, location: &str, index: &KnownIds) -> Mentions {
    if content.len() > 1_048_576 {
        eprintln!("[neurostrata:mention] content exceeds 1 MiB; skipping detection");
        return Mentions { ids: Vec::new(), dropped: 0 };
    }
    let candidates = index
        .cached_candidates()
        .unwrap_or_else(|| build_candidates(index));
    if candidates.is_empty() {
        return Mentions { ids: Vec::new(), dropped: 0 };
    }

    // (id, best specificity seen for it), in order of first appearance.
    let mut found: Vec<(String, u8)> = Vec::new();
    // We need neighbor checks; process content and location with positions.
    let inputs = [content, location];
    for text in inputs {
        for token in text.split_whitespace() {
            let stripped = strip_surrounding_punct(token);
            if stripped.is_empty() {
                continue;
            }
            if let Some(id) = candidates.get(&stripped) {
                // Short alias neighbor guard: only when at least one
                // occurrence of the alias in `text` is NOT glued to a
                // path/identifier separator on either side does it count as a
                // real mention; occurrence inside `src/alias.rs` must not
                // resolve via its short name.
                if is_short_alias(&stripped) && !has_unattached_occurrence(text, &stripped) {
                    continue;
                }
                let class = specificity(&stripped);
                match found.iter_mut().find(|(seen, _)| seen == id) {
                    Some((_, best)) => *best = (*best).min(class),
                    None => found.push((id.clone(), class)),
                }
            }
        }
    }

    if found.len() <= MENTION_CAP {
        return Mentions { ids: found.into_iter().map(|(id, _)| id).collect(), dropped: 0 };
    }
    let dropped = found.len() - MENTION_CAP;
    eprintln!("[neurostrata:mention] mention cap reached: kept {MENTION_CAP}, dropped {dropped}");
    // Stable sort: equally specific matches keep the order they appeared in.
    let mut by_specificity: Vec<usize> = (0..found.len()).collect();
    by_specificity.sort_by_key(|&i| found[i].1);
    let mut keep: Vec<usize> = by_specificity.into_iter().take(MENTION_CAP).collect();
    keep.sort_unstable();
    Mentions { ids: keep.into_iter().map(|i| found[i].0.clone()).collect(), dropped }
}

// Expose KnownIds ids via trait-like method added in ladybug.rs.
// This module relies on `index.ids()`.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::ladybug::KnownIds;

    #[test]
    fn basic_detection() {
        let ids = vec![
            "ns::1::src/foo.rs#functions:bar@abc".to_string(),
            "ns::1::src/baz.rs".to_string(),
        ];
        let refs: Vec<&str> = ids.iter().map(|s| s.as_str()).collect();
        let index = KnownIds::new(refs);
        let content = "see `src/foo.rs#functions:bar` and src/baz.rs";
        let got = detect_mentions(content, "", &index);
        assert!(got.iter().any(|g| g.contains("src/foo.rs#functions:bar")));
        assert!(got.iter().any(|g| g.contains("src/baz.rs")));
    }

    #[test]
    fn short_alias_glued_to_separator_not_mention() {
        let ids = vec!["ns::1::src/bar.rs#functions:bar@abc".to_string()];
        let refs: Vec<&str> = ids.iter().map(|s| s.as_str()).collect();
        let index = KnownIds::new(refs);
        // "-bar-": token strips to the short alias "bar", but its only
        // occurrence in text has separators on both sides → no mention.
        assert!(detect_mentions("-bar-", "", &index).is_empty());
        // Standalone short alias mention still resolves.
        assert_eq!(detect_mentions("bar", "", &index).len(), 1);
    }

    #[test]
    fn cap_and_dedup() {
        // Build 40 distinct symbols.
        let mut ids = Vec::new();
        for i in 0..40 {
            ids.push(format!("ns::1::src/f{}.rs#functions:g{}@h{}", i, i, i));
        }
        let refs: Vec<&str> = ids.iter().map(|s| s.as_str()).collect();
        let index = KnownIds::new(refs);
        let content = (0..40)
            .map(|i| format!("src/f{}.rs#functions:g{}", i, i))
            .collect::<Vec<_>>()
            .join(" ");
        let got = detect_mentions(&content, "", &index);
        assert_eq!(got.len(), MENTION_CAP);
    }

    fn ids_of(count: usize) -> Vec<String> {
        (0..count)
            .map(|i| format!("ns::1::src/f{}.rs#functions:sym{}@h{}", i, i, i))
            .collect()
    }

    #[test]
    fn at_or_under_the_cap_nothing_is_dropped_and_text_order_holds() {
        let ids = ids_of(40);
        let index = KnownIds::new(ids.iter().map(|s| s.as_str()));
        let content = (0..MENTION_CAP)
            .rev()
            .map(|i| format!("src/f{}.rs#functions:sym{}", i, i))
            .collect::<Vec<_>>()
            .join(" ");
        let got = detect_mentions_report(&content, "", &index);
        assert_eq!(got.dropped, 0);
        assert_eq!(got.ids.len(), MENTION_CAP);
        assert!(got.ids[0].contains("f31.rs"), "text order, not sorted: {:?}", got.ids[0]);
    }

    #[test]
    fn over_the_cap_the_count_dropped_is_reported() {
        let ids = ids_of(40);
        let index = KnownIds::new(ids.iter().map(|s| s.as_str()));
        let content = (0..40)
            .map(|i| format!("src/f{}.rs#functions:sym{}", i, i))
            .collect::<Vec<_>>()
            .join(" ");
        let got = detect_mentions_report(&content, "", &index);
        assert_eq!(got.ids.len(), MENTION_CAP);
        assert_eq!(got.dropped, 8);
    }

    #[test]
    fn over_the_cap_specific_matches_beat_earlier_short_aliases() {
        // 32 symbols mentioned first by their bare name (short alias), then 8
        // mentioned by full path. The old cut kept the first 32 in text order
        // (all aliases); the cut now keeps the 8 full paths and 24 aliases.
        let ids = ids_of(40);
        let index = KnownIds::new(ids.iter().map(|s| s.as_str()));
        let aliases = (0..32).map(|i| format!("sym{}", i));
        let paths = (32..40).map(|i| format!("src/f{}.rs#functions:sym{}", i, i));
        let content = aliases.chain(paths).collect::<Vec<_>>().join(" ");
        let got = detect_mentions_report(&content, "", &index);
        assert_eq!(got.ids.len(), MENTION_CAP);
        assert_eq!(got.dropped, 8);
        for i in 32..40 {
            assert!(got.ids.contains(&ids[i]), "full-path mention {i} is kept");
        }
        let kept_aliases: Vec<&String> = got.ids.iter().filter(|id| !ids[32..].contains(id)).collect();
        assert_eq!(kept_aliases.len(), 24);
        assert!(kept_aliases.iter().all(|id| ids[..24].contains(id)), "ties keep text order");
    }
}
