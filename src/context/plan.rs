//! Deterministic ranking, deduplication and budget application for a context plan.

use super::model::ContextItem;
use std::collections::HashSet;

/// Cumulative budgets applied while building a plan. All three must be
/// respected: an item whose cost would exhaust any one of them is skipped,
/// and iteration continues through the remaining candidates so a fitting
/// later item can still make the plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    pub per_call: usize,
    pub per_turn: usize,
    pub per_session: usize,
    /// Tokens already spent this turn by earlier calls.
    pub turn_used: usize,
    /// Tokens already spent this session by earlier calls.
    pub session_used: usize,
}

impl Default for Budget {
    fn default() -> Self {
        // Per-trigger ceiling from spec §5.3.
        Budget {
            per_call: 512,
            per_turn: 1500,
            per_session: 1500,
            turn_used: 0,
            session_used: 0,
        }
    }
}

impl From<usize> for Budget {
    fn from(value: usize) -> Self {
        Budget {
            per_call: value,
            per_turn: value,
            per_session: value,
            turn_used: 0,
            session_used: 0,
        }
    }
}

impl From<(usize, usize, usize)> for Budget {
    fn from((per_call, per_turn, per_session): (usize, usize, usize)) -> Self {
        Budget {
            per_call,
            per_turn,
            per_session,
            turn_used: 0,
            session_used: 0,
        }
    }
}

impl Budget {
    /// Declare tokens already consumed by earlier calls in the same turn and
    /// session, so the per-turn/per-session ceilings enforce cumulative usage
    /// rather than only this call's spend.
    pub fn with_prior_usage(mut self, turn_used: usize, session_used: usize) -> Self {
        self.turn_used = turn_used;
        self.session_used = session_used;
        self
    }
}

/// Estimate the token cost of one item (chars/4 + a small per-item overhead).
pub fn estimate_tokens(item: &ContextItem) -> usize {
    item.content.chars().count() / 4 + 8
}

/// What one offered item did to a [`Selection`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Offer {
    /// Selected whole.
    Taken,
    /// Its id or content was already offered; nothing changed.
    Duplicate,
    /// It would exhaust a budget; skipped whole, never cut.
    OverBudget,
}

/// Items chosen so far against one [`Budget`], deduplicated by id and
/// content. Governing rules and search hits go through the same selection,
/// so they share one budget and one dedup.
#[derive(Debug, Clone)]
pub struct Selection {
    budget: Budget,
    call_tokens: usize,
    turn_tokens: usize,
    session_tokens: usize,
    seen_ids: HashSet<String>,
    seen_content: HashSet<String>,
    items: Vec<ContextItem>,
}

impl Selection {
    pub fn new(budget: impl Into<Budget>) -> Self {
        let budget = budget.into();
        Selection {
            // Per-turn and per-session ceilings are cumulative: they start
            // from the usage reported on the budget, not from zero.
            turn_tokens: budget.turn_used,
            session_tokens: budget.session_used,
            budget,
            call_tokens: 0,
            seen_ids: HashSet::new(),
            seen_content: HashSet::new(),
            items: Vec::new(),
        }
    }

    /// Whether an item with this id was already offered, or its content
    /// was already selected.
    pub fn has_seen(&self, item: &ContextItem) -> bool {
        self.seen_ids.contains(&item.id) || self.seen_content.contains(&item.content)
    }

    /// Offer one item: selected whole when it is new and fits every budget.
    /// An item that does not fit is still marked as seen by id, so a later
    /// copy of it is a duplicate; its content counts as seen only once
    /// selected, so another memory with the same text is offered on its own.
    pub fn offer(&mut self, item: &ContextItem) -> Offer {
        if self.seen_ids.contains(&item.id) || self.seen_content.contains(&item.content) {
            return Offer::Duplicate;
        }
        self.seen_ids.insert(item.id.clone());
        let cost = estimate_tokens(item);
        if self.call_tokens + cost > self.budget.per_call
            || self.turn_tokens + cost > self.budget.per_turn
            || self.session_tokens + cost > self.budget.per_session
        {
            // Skip rather than truncate content, so a plan never silently
            // delivers a half-memory.
            return Offer::OverBudget;
        }
        self.seen_content.insert(item.content.clone());
        self.call_tokens += cost;
        self.turn_tokens += cost;
        self.session_tokens += cost;
        self.items.push(item.clone());
        Offer::Taken
    }

    pub fn count(&self) -> usize {
        self.items.len()
    }

    pub fn into_items(self) -> Vec<ContextItem> {
        self.items
    }
}

/// Deduplicate `items` by id and content, then keep as many as fit in `budget`,
/// respecting the cumulative per-call/turn/session ceilings. The input order is
/// preserved (callers rank first). Production plans go through
/// [`Selection`] directly; this is the one-shot form the tests pin.
#[cfg(test)]
pub fn rank_dedup_budget<I, T>(items: I, budget: impl Into<Budget>) -> Vec<ContextItem>
where
    I: IntoIterator<Item = T>,
    T: std::borrow::Borrow<ContextItem>,
{
    let mut selection = Selection::new(budget);
    for item in items {
        selection.offer(std::borrow::Borrow::borrow(&item));
    }
    selection.into_items()
}

#[cfg(test)]
mod tests {
    use super::super::model::ContextItem;
    use super::*;

    fn item(id: &str, content: &str) -> ContextItem {
        ContextItem {
            id: id.into(),
            memory_type: "rule".into(),
            content: content.into(),
            freshness: None,
        }
    }

    #[test]
    fn dedups_by_id() {
        let items = vec![item("a", "hello"), item("a", "hello2")];
        let out = rank_dedup_budget(items, 10_000usize);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].content, "hello");
    }

    #[test]
    fn dedups_by_content() {
        let items = vec![item("a", "same"), item("b", "same")];
        let out = rank_dedup_budget(items, 10_000usize);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn budget_truncates() {
        let items = vec![item("a", &"x".repeat(1000)), item("b", "short")];
        let out = rank_dedup_budget(items, 10usize);
        assert!(out.len() <= 1);
    }

    #[test]
    fn prior_usage_consumes_turn_and_session_ceiling() {
        let items = vec![item("a", &"x".repeat(400)), item("b", &"y".repeat(400))];
        // Each item costs 108 tokens and the 10_000 ceiling starts with 9_900
        // already spent, so both must be cut even though per_call allows them.
        let budget = Budget::from(10_000usize).with_prior_usage(9_900, 9_900);
        let out = rank_dedup_budget(items, budget);
        assert!(out.is_empty());
    }
}
