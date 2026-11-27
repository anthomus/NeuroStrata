//! The shared context read-planning service.
//!
//! `ContextService` is the single entry point for building a [`ContextPlan`]
//! and rendering it. It is stateless: the caller wires whatever backends a
//! production plan needs before invoking it, so there are no capability flags
//! to toggle — a plan is always built the same way.
//!
//! Two construction paths, one honest outcome each:
//!
//! * [`ContextService::plan`] builds the shell of a plan for a request:
//!   scope from the request, no items, never `Supported`. An empty plan is
//!   a degraded outcome (`Partial`) the caller must observe, and a request
//!   whose [`EgressPolicy`] denies egress is refused up front (`Unavailable`
//!   with no items). It is the base both other paths report through, and the
//!   shape a caller uses when it only needs the degradation, policy and
//!   scope semantics probed.
//! * [`ContextService::plan_with_items`] takes the rules governing each
//!   requested file, ranked candidate items and a [`Budget`]. It deals the
//!   rules across files in turns, then fills the rest with candidates, all
//!   whole and deduplicated inside the budget and `max_items` (`plan::Selection`);
//!   a rule that does not fit becomes a pointer in `not_shown`. It reports
//!   `Supported` only when at least one item survived. This is the
//!   production path the server and hosts use.
//!
//! Rendering is deliberately not bundled into planning: production hosts render
//! via the free `render` re-exported from `context`; the test-only `render`
//! shim lives on the service for convenience in unit tests.

use super::model::{ContextPlan, ContextRequest, Degradation};

#[derive(Debug, Clone, Default)]
pub struct ContextService;

impl ContextService {
    pub fn new() -> Self {
        Self
    }

    /// An empty plan degrades to [`Degradation::Partial`]: the model
    /// guarantees an empty plan is never implicitly `Supported` (model.rs),
    /// because "nothing was planned" is a degraded outcome the caller must
    /// be able to observe.
    pub fn plan<R>(&self, request: R) -> ContextPlan
    where
        R: AsRef<ContextRequest>,
    {
        let req = request.as_ref();
        // The egress policy travels with the request: enforce it before any
        // plan is shaped. A denied request produces an empty denial shell —
        // `Degradation::Unavailable`, never implicitly `Supported` — rather
        // than silently planning content that may not leave the process.
        if let Err(reason) = req.policy.enforce() {
            tracing::debug!(reason = %reason, "context plan denied by egress policy");
            let mut plan = ContextPlan::new(req.event_id.clone(), Degradation::Unavailable);
            plan.applied_scope = req.session_id.clone();
            plan.items = Vec::new();
            plan.truncation = false;
            return plan;
        }
        let mut plan = ContextPlan::new(req.event_id.clone(), Degradation::Partial);
        // The request's authorized scope: the caller's session id (production
        // hosts that own a namespace, as in server.rs, overwrite it with the
        // effective namespace after planning).
        plan.applied_scope = req.session_id.clone();
        plan.items = Vec::new();
        plan.truncation = false;
        tracing::trace!(
            epoch = req.context_epoch,
            trigger = %req.trigger,
            repo = %req.source.repo_id,
            "planned context"
        );
        plan
    }

    /// Render `plan` as compact text. Associated function (call as
    /// `ContextService::render(&plan)`).
    /// Render `plan` as compact text. Test-only — production renders via the
    /// free `render` re-exported from `context`.
    #[cfg(test)]
    pub fn render(plan: impl AsRef<ContextPlan>) -> String {
        super::render::render(plan.as_ref())
    }

    /// Deals the rules governing the requested files first, then fills the
    /// rest with `candidates`, all inside one `budget` and `max_items`.
    ///
    /// `governing` holds one list per requested file, in the fixed order
    /// (symbol > file > folder, then type and reads). Each list is reordered
    /// by distance from the query, rules without one after, and the lists
    /// are dealt in turns: every file's first rule, then every file's
    /// second, so one file's rules never crowd out another's. A rule is sent
    /// whole or, when it does not fit, listed in `not_shown` as a pointer
    /// with no content. `truncation` is set when there is a pointer or a
    /// candidate was left out.
    pub fn plan_with_items<R>(
        &self,
        request: R,
        governing: Vec<Vec<super::model::GoverningRule>>,
        candidates: Vec<super::model::ContextItem>,
        budget: super::plan::Budget,
        max_items: usize,
    ) -> ContextPlan
    where
        R: AsRef<ContextRequest>,
    {
        use super::model::{NotShown, QueryMatch, FETCH_TOOL};
        use super::plan::{Offer, Selection};

        let mut plan = self.plan(&request);
        if request.as_ref().policy.enforce().is_err() {
            // The egress policy denied this request; keep the denial shell
            // (empty items, `Unavailable`) instead of upgrading it to
            // `Supported` — no item may be delivered against refused egress.
            return plan;
        }
        let mut selection = Selection::new(budget);
        let files: Vec<Vec<super::model::GoverningRule>> = governing
            .into_iter()
            .map(|mut rules| {
                // Stable: rules at the same distance keep the fixed order.
                rules.sort_by(|a, b| match (a.distance, b.distance) {
                    (Some(x), Some(y)) => x.total_cmp(&y),
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (None, None) => std::cmp::Ordering::Equal,
                });
                rules
            })
            .collect();
        let depth = files.iter().map(Vec::len).max().unwrap_or(0);
        // (rule, its 1-based place in its file, the file's rule count)
        let mut left_out: Vec<(&super::model::GoverningRule, usize, usize)> = Vec::new();
        let mut left_out_ids: std::collections::HashSet<&str> = std::collections::HashSet::new();
        // The largest distance among rules sent whole: the weakest match.
        let mut weakest_whole: Option<f32> = None;
        for turn in 0..depth {
            for rules in &files {
                let Some(rule) = rules.get(turn) else {
                    continue;
                };
                // A rule governing two requested files is dealt once.
                if left_out_ids.contains(rule.item.id.as_str()) || selection.has_seen(&rule.item) {
                    continue;
                }
                let taken =
                    selection.count() < max_items && selection.offer(&rule.item) == Offer::Taken;
                if taken {
                    if let Some(d) = rule.distance {
                        weakest_whole = Some(weakest_whole.map_or(d, |w| w.max(d)));
                    }
                } else {
                    left_out_ids.insert(rule.item.id.as_str());
                    left_out.push((rule, turn + 1, rules.len()));
                }
            }
        }
        plan.not_shown = left_out
            .into_iter()
            .map(|(rule, place, of)| NotShown {
                id: rule.item.id.clone(),
                file: rule.file.clone(),
                governs: rule.governs,
                query_rank: format!("{place} of {of}"),
                query_match: match (rule.distance, weakest_whole) {
                    (None, _) => QueryMatch::NoMatch,
                    (Some(d), Some(weakest)) if d > weakest => QueryMatch::Weaker,
                    (Some(_), _) => QueryMatch::Close,
                },
                fetch: FETCH_TOOL.to_string(),
            })
            .collect();
        // A search copy of a rule already dealt (whole or as a pointer) is
        // not a separate hit, so it is not counted as cut.
        let candidates: Vec<_> = candidates
            .into_iter()
            .filter(|it| !left_out_ids.contains(it.id.as_str()) && !selection.has_seen(it))
            .collect();
        let total = candidates.len();
        let mut candidates_cut = false;
        for candidate in &candidates {
            if selection.count() >= max_items || selection.offer(candidate) != Offer::Taken {
                candidates_cut = true;
            }
        }
        plan.items = selection.into_items();
        plan.truncation = candidates_cut || !plan.not_shown.is_empty();
        // A non-empty selection is fully supported even when `truncation` is
        // set: the plan delivered the items it planned, while `truncation`
        // records (and the debug log below surfaces) that the request was cut
        // short by dedup, budget or `max_items`. An empty selection is a
        // degraded plan — nothing survived to deliver.
        plan.degradation = if !plan.items.is_empty() {
            Degradation::Supported
        } else {
            Degradation::Partial
        };
        if plan.truncation {
            tracing::debug!(
                candidates = total,
                delivered = plan.items.len(),
                not_shown = plan.not_shown.len(),
                "context plan truncated: {} items delivered, {} governing rules as pointers",
                plan.items.len(),
                plan.not_shown.len()
            );
        }
        plan
    }
}

#[cfg(test)]
mod tests {
    use super::super::model::{
        ContextItem, ContextRequest, Degradation, EgressPolicy, SourceIdentity,
    };
    use super::super::plan::Budget;
    use super::*;

    fn request() -> ContextRequest {
        ContextRequest {
            session_id: "s".into(),
            context_epoch: 1,
            event_id: "evt".into(),
            trigger: "file_touch".into(),
            source: SourceIdentity::default(),
            policy: EgressPolicy::default(),
        }
    }

    fn item(id: &str, content: &str) -> ContextItem {
        ContextItem {
            id: id.into(),
            memory_type: "rule".into(),
            content: content.into(),
            freshness: None,
        }
    }

    #[test]
    fn plan_degrades_partial_when_empty() {
        let service = ContextService::new();
        let plan = service.plan(&request());
        assert_eq!(plan.degradation, Degradation::Partial);
        assert!(plan.items.is_empty());
        assert!(!plan.truncation);
    }

    /// A request whose egress policy denies never produces a deliverable
    /// plan: `plan` refuses up front (`Unavailable`, no items), and
    /// `plan_with_items` keeps that denial shell instead of upgrading it to
    /// `Supported` even when candidates survived.
    #[test]
    fn denied_policy_never_yields_deliverable_plan() {
        let service = ContextService::new();
        let mut denied = request();
        denied.policy = EgressPolicy::deny("egress refused");
        let plan = service.plan(&denied);
        assert_eq!(plan.degradation, Degradation::Unavailable);
        assert!(plan.items.is_empty());

        let with_items = service.plan_with_items(
            &denied,
            Vec::new(),
            vec![item("i1", "c1")],
            Budget::from(10_000usize),
            10,
        );
        assert_eq!(with_items.degradation, Degradation::Unavailable);
        assert!(with_items.items.is_empty());
    }

    #[test]
    fn plan_with_items_rank_dedups_by_id_and_content() {
        let service = ContextService::new();
        let candidates = vec![
            item("i1", "c1"),
            item("i1", "c1-dup-id"),
            item("i2", "c1"),
            item("i3", "c3"),
        ];
        let plan = service.plan_with_items(
            &request(),
            Vec::new(),
            candidates,
            Budget::from(10_000usize),
            10,
        );
        assert_eq!(plan.items.len(), 2);
        assert_eq!(plan.items[0].id, "i1");
        assert_eq!(plan.items[1].id, "i3");
        // Drops by dedup/budget also count toward `truncation` (total input >
        // output), matching the long-standing contract.
        assert!(plan.truncation);
        assert_eq!(plan.degradation, Degradation::Supported);
    }

    #[test]
    fn plan_with_items_budgets_out_expensive_items() {
        let service = ContextService::new();
        let candidates = vec![item("a", &"x".repeat(1000)), item("b", "tiny")];
        // item "a" costs 1000/4+8 = 258 tokens > the 30-token budget; "b" fits.
        let plan = service.plan_with_items(
            &request(),
            Vec::new(),
            candidates,
            Budget::from(30usize),
            10,
        );
        assert_eq!(plan.items.len(), 1);
        assert_eq!(plan.items[0].id, "b");
    }

    #[test]
    fn plan_with_items_truncates_to_max_items() {
        let service = ContextService::new();
        let candidates: Vec<ContextItem> = (0..5)
            .map(|i| item(&format!("i{}", i), &format!("content-{}", i)))
            .collect();
        let plan = service.plan_with_items(
            &request(),
            Vec::new(),
            candidates,
            Budget::from(10_000usize),
            3,
        );
        assert_eq!(plan.items.len(), 3);
        assert!(plan.truncation);
    }

    fn rule(
        id: &str,
        file: &str,
        content: &str,
        distance: Option<f32>,
    ) -> super::super::model::GoverningRule {
        super::super::model::GoverningRule {
            item: item(id, content),
            file: file.into(),
            governs: super::super::model::Governs::File,
            distance,
        }
    }

    /// Rules are dealt in turns across files; within a file, distance from
    /// the query comes first and the given order breaks ties; a rule that governs two
    /// files is dealt once.
    #[test]
    fn governing_rules_are_dealt_in_turns_by_search_rank() {
        let service = ContextService::new();
        let governing = vec![
            vec![
                rule("a1", "a.rs", "a one", None),
                rule("a2", "a.rs", "a two", Some(0.4)),
                rule("shared", "a.rs", "shared", Some(0.1)),
            ],
            vec![
                rule("shared", "b.rs", "shared", Some(0.1)),
                rule("b1", "b.rs", "b one", None),
                rule("b2", "b.rs", "b two", None),
            ],
        ];
        let plan = service.plan_with_items(
            &request(),
            governing,
            vec![item("hit", "search hit")],
            Budget::from(10_000usize),
            3,
        );
        let ids: Vec<&str> = plan.items.iter().map(|it| it.id.as_str()).collect();
        // Turn 1: a.rs's best is `shared`; b.rs's `shared` is already dealt.
        // Turn 2: a.rs gives a2, b.rs gives b1. No room is left.
        assert_eq!(ids, vec!["shared", "a2", "b1"]);
        let pointers: Vec<(&str, &str, &str)> = plan
            .not_shown
            .iter()
            .map(|p| (p.id.as_str(), p.file.as_str(), p.query_rank.as_str()))
            .collect();
        assert_eq!(
            pointers,
            vec![("a1", "a.rs", "3 of 3"), ("b2", "b.rs", "3 of 3")]
        );
        assert!(plan.truncation);
    }

    /// `match` compares a pointer's distance from the query with the
    /// weakest rule sent whole: as near or nearer is `close`, farther is
    /// `weaker`, no distance is `none`.
    #[test]
    fn pointers_carry_match_bands() {
        use super::super::model::QueryMatch;
        let service = ContextService::new();
        let long = "x".repeat(400);
        let governing = vec![vec![
            rule("whole", "a.rs", "short", Some(0.2)),
            rule("tie", "a.rs", &format!("tie {long}"), Some(0.2)),
            rule("far", "a.rs", &format!("far {long}"), Some(0.5)),
            rule("unfound", "a.rs", &format!("unfound {long}"), None),
        ]];
        // 50 tokens: room for "short" (9), not for any 400-character rule.
        let plan =
            service.plan_with_items(&request(), governing, Vec::new(), Budget::from(50usize), 10);
        assert_eq!(plan.items.len(), 1);
        let bands: Vec<(&str, QueryMatch)> = plan
            .not_shown
            .iter()
            .map(|p| (p.id.as_str(), p.query_match))
            .collect();
        assert_eq!(
            bands,
            vec![
                ("tie", QueryMatch::Close),
                ("far", QueryMatch::Weaker),
                ("unfound", QueryMatch::NoMatch),
            ]
        );
        let json = serde_json::to_value(&plan.not_shown[0]).expect("pointer json");
        assert_eq!(json["match"], "close");
        assert_eq!(json["governs"], "file");
        assert_eq!(json["fetch"], "neurostrata_get_memory");
        assert!(json.get("content").is_none());
    }

    /// Two rules with the same text that both miss the budget are both
    /// pointers: a rule that was not sent does not hide another.
    #[test]
    fn rules_with_the_same_text_are_both_pointers_when_neither_fits() {
        let service = ContextService::new();
        let text = "y".repeat(400);
        let governing = vec![
            vec![rule("a", "a.rs", &text, None)],
            vec![rule("b", "b.rs", &text, None)],
        ];
        let plan =
            service.plan_with_items(&request(), governing, Vec::new(), Budget::from(50usize), 10);
        assert!(plan.items.is_empty());
        let ids: Vec<&str> = plan.not_shown.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b"]);
    }

    /// Without governing rules nothing is a pointer, and a plan whose
    /// candidates all fit is not truncated.
    #[test]
    fn no_governing_rules_means_no_pointers() {
        let service = ContextService::new();
        let plan = service.plan_with_items(
            &request(),
            Vec::new(),
            vec![item("c", "candidate")],
            Budget::from(10_000usize),
            5,
        );
        assert!(plan.not_shown.is_empty());
        assert!(!plan.truncation);
    }

    #[test]
    fn render_works() {
        let plan = ContextPlan::new("evt", Degradation::Supported);
        let text = ContextService::render(&plan);
        assert!(text.contains("context-plan"));
    }
}
