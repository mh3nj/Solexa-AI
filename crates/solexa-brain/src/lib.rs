//! solexa-brain, the context packager (Phase 4 of the roadmap).
//!
//! Rule from the spec: "Do NOT give every specialist the entire project
//! context... Only provide: relevant project facts, relevant
//! constraints, relevant decisions, relevant artifacts... The specialist
//! should know what context was included and why."
//!
//! `package_context` takes everything currently in a project's Project
//! Brain (`KnowledgeItem`s and `Decision`s) and a task's required
//! capabilities, and returns a `ContextPackage` containing only the
//! items that matched, with an honest `excluded_summary` explaining
//! what else existed and why it didn't make the cut, never just silence
//! about what was left out.
//!
//! Matching is tag-overlap between an item's `tags` and the task's
//! `required_capabilities`, a keyword heuristic, deliberately in the
//! same spirit as the Planner's capability inference
//! (`solexa-coordinator::planner`): an honest stand-in for real
//! semantic retrieval, not an attempt at one. The spec is explicit that
//! vector search belongs here eventually as "an additional retrieval
//! mechanism, not the project brain itself", this function is exactly
//! the seam a future embedding-based ranking would slot into, without
//! changing anything upstream or downstream of it.

use solexa_domain::{ContextPackage, Decision, KnowledgeItem, KnowledgeKind, UNRESOLVED_SUFFIX};
#[cfg(test)]
use solexa_domain::KnowledgeStatus;

pub fn package_context(
    knowledge: &[KnowledgeItem],
    decisions: &[Decision],
    required_capabilities: &[String],
) -> ContextPackage {
    package_context_scoped(knowledge, decisions, required_capabilities, &[])
}

/// Like `package_context`, narrowed to a specialist's `knowledge_scope`
/// (round 59). A non-empty scope keeps only items that ALSO carry at least one
/// scope tag; it can never add anything the task's capability match, or the
/// status filter, already excluded. Blank entries are ignored, and a scope
/// with no real entries means "unrestricted", so the default (an empty
/// scope) is exactly `package_context`. A scope that matches nothing yields
/// nothing: restricting is its whole point.
pub fn package_context_scoped(
    knowledge: &[KnowledgeItem],
    decisions: &[Decision],
    required_capabilities: &[String],
    knowledge_scope: &[String],
) -> ContextPackage {
    let scope: Vec<&str> = knowledge_scope.iter().map(|t| t.trim()).filter(|t| !t.is_empty()).collect();
    // Round 63: tags and capabilities are compared ignoring case/whitespace.
    let is_relevant = |tags: &[String]| {
        tags.iter().any(|tag| required_capabilities.iter().any(|cap| solexa_domain::tags_match(cap, tag)))
    };
    let in_scope = |tags: &[String]| {
        scope.is_empty() || tags.iter().any(|tag| scope.iter().any(|wanted| solexa_domain::tags_match(wanted, tag)))
    };

    let mut included_facts = Vec::new();
    let mut included_constraints = Vec::new();
    let mut included_artifacts = Vec::new();
    let mut included_count = 0usize;

    // Only authoritative knowledge (Accepted | Verified) may be handed to a
    // model as fact; the rest is counted so the summary stays honest.
    let mut non_authoritative = 0usize;
    let mut out_of_scope = 0usize;
    for item in knowledge {
        if !item.status.is_authoritative() {
            non_authoritative += 1;
            continue;
        }
        if !is_relevant(&item.tags) {
            continue;
        }
        if !in_scope(&item.tags) {
            out_of_scope += 1;
            continue;
        }
        included_count += 1;
        match item.kind {
            KnowledgeKind::Fact => included_facts.push(item.content.clone()),
            KnowledgeKind::Constraint => included_constraints.push(item.content.clone()),
            KnowledgeKind::Artifact => included_artifacts.push(item.content.clone()),
        }
    }

    let mut included_decisions = Vec::new();
    for decision in decisions {
        if !is_relevant(&decision.tags) {
            continue;
        }
        if !in_scope(&decision.tags) {
            out_of_scope += 1;
            continue;
        }
        included_count += 1;
        included_decisions.push(match &decision.resolution {
            Some(resolution) => format!("{} → {}", decision.question, resolution),
            None => format!("{}{}", decision.question, UNRESOLVED_SUFFIX),
        });
    }

    let total = knowledge.len() + decisions.len();
    let mut excluded_summary = summarize(total, included_count, required_capabilities);
    if non_authoritative > 0 {
        excluded_summary.push_str(&format!(
            " {non_authoritative} knowledge item(s) were left out because they are not accepted or verified."
        ));
    }
    if out_of_scope > 0 {
        excluded_summary.push_str(&format!(
            " {out_of_scope} relevant item(s) were left out because they are outside this specialist's knowledge scope."
        ));
    }

    ContextPackage {
        included_facts,
        included_constraints,
        included_artifacts,
        included_decisions,
        excluded_summary,
    }
}

fn summarize(total: usize, included: usize, required_capabilities: &[String]) -> String {
    if total == 0 {
        return "No Project Brain entries exist yet for this project.".to_string();
    }
    let caps = required_capabilities.join(", ");
    if included == 0 {
        format!(
            "{total} project knowledge item(s)/decision(s) exist, but none are tagged with any \
             of this task's required capabilities ({caps}); nothing was scoped in."
        )
    } else {
        format!(
            "{included} of {total} project knowledge item(s)/decision(s) matched this task's \
             required capabilities ({caps}); the rest were excluded as not relevant."
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solexa_domain::{Provenance, Uuid};

    fn item(kind: KnowledgeKind, content: &str, tags: &[&str]) -> KnowledgeItem {
        KnowledgeItem::new(
            Uuid::new_v4(),
            kind,
            "title",
            content,
            Provenance::human("test"),
            tags.iter().map(|t| t.to_string()).collect(),
        )
    }

    #[test]
    fn only_tag_matching_items_are_included() {
        let backend_constraint = item(
            KnowledgeKind::Constraint,
            "Must use PostgreSQL, not SQLite, in production.",
            &["backend", "sql"],
        );
        let design_fact = item(KnowledgeKind::Fact, "Brand color is #E3A857.", &["design"]);

        let package = package_context(
            &[backend_constraint.clone(), design_fact],
            &[],
            &["backend".to_string()],
        );

        assert_eq!(package.included_constraints, vec![backend_constraint.content]);
        assert!(package.included_facts.is_empty(), "design fact should not leak into a backend task's context");
    }

    #[test]
    fn excluded_summary_explains_what_existed_but_was_left_out() {
        let design_fact = item(KnowledgeKind::Fact, "Brand color is #E3A857.", &["design"]);
        let package = package_context(&[design_fact], &[], &["backend".to_string()]);

        assert!(package.included_facts.is_empty());
        assert!(package.excluded_summary.contains('1'));
        assert!(package.excluded_summary.contains("backend"));
    }

    #[test]
    fn empty_brain_produces_an_honest_empty_summary() {
        let package = package_context(&[], &[], &["backend".to_string()]);
        assert!(package.included_facts.is_empty());
        assert!(package.excluded_summary.contains("No Project Brain entries"));
    }

    fn item_with_status(status: KnowledgeStatus, content: &str) -> KnowledgeItem {
        let mut i = item(KnowledgeKind::Constraint, content, &["backend"]);
        i.status = status;
        i
    }

    #[test]
    fn only_accepted_and_verified_knowledge_is_packaged() {
        let caps = vec!["backend".to_string()];
        for status in [
            KnowledgeStatus::Unverified,
            KnowledgeStatus::Proposed,
            KnowledgeStatus::Rejected,
            KnowledgeStatus::Superseded,
            KnowledgeStatus::Archived,
        ] {
            let package = package_context(&[item_with_status(status, "X")], &[], &caps);
            assert!(
                package.included_constraints.is_empty(),
                "{status:?} knowledge must never reach a model"
            );
        }
        // positive controls: the filter is not simply dropping everything
        for status in [KnowledgeStatus::Accepted, KnowledgeStatus::Verified] {
            let package = package_context(&[item_with_status(status, "X")], &[], &caps);
            assert_eq!(package.included_constraints, vec!["X".to_string()], "{status:?}");
        }
    }

    #[test]
    fn the_summary_says_when_items_were_left_out_for_their_status() {
        let caps = vec!["backend".to_string()];
        let package = package_context(
            &[
                item_with_status(KnowledgeStatus::Accepted, "keep"),
                item_with_status(KnowledgeStatus::Rejected, "drop"),
            ],
            &[],
            &caps,
        );
        assert!(package.excluded_summary.contains("not accepted or verified"), "{}", package.excluded_summary);
    }

    fn tagged(content: &str, tags: &[&str]) -> KnowledgeItem {
        item(KnowledgeKind::Constraint, content, tags)
    }

    #[test]
    fn a_knowledge_scope_narrows_to_items_sharing_a_scope_tag() {
        let caps = vec!["backend".to_string()];
        let auth = tagged("AUTH", &["backend", "auth"]);
        let billing = tagged("BILLING", &["backend", "billing"]);
        let package = package_context_scoped(&[auth, billing], &[], &caps, &["auth".to_string()]);
        assert_eq!(package.included_constraints, vec!["AUTH".to_string()]);
        assert!(package.excluded_summary.contains("outside this specialist's knowledge scope"), "{}", package.excluded_summary);
    }

    #[test]
    fn an_empty_or_blank_scope_is_unrestricted_and_identical_to_the_unscoped_package() {
        let caps = vec!["backend".to_string()];
        let items = [tagged("AUTH", &["backend", "auth"]), tagged("BILLING", &["backend", "billing"])];
        let unscoped = package_context(&items, &[], &caps);
        for scope in [vec![], vec!["".to_string(), "   ".to_string()]] {
            let scoped = package_context_scoped(&items, &[], &caps, &scope);
            assert_eq!(scoped.included_constraints, unscoped.included_constraints, "{scope:?}");
            assert_eq!(scoped.excluded_summary, unscoped.excluded_summary, "{scope:?}");
        }
    }

    #[test]
    fn a_scope_never_widens_what_the_task_already_matched() {
        // tagged for the scope but not for the task's capability -> still out
        let caps = vec!["backend".to_string()];
        let only_scope_tag = tagged("AUTH-ONLY", &["auth"]);
        let package = package_context_scoped(&[only_scope_tag], &[], &caps, &["auth".to_string()]);
        assert!(package.included_constraints.is_empty());
    }

    #[test]
    fn a_scope_that_matches_nothing_gives_nothing() {
        let caps = vec!["backend".to_string()];
        let package = package_context_scoped(&[tagged("AUTH", &["backend", "auth"])], &[], &caps, &["typography".to_string()]);
        assert_eq!(package.item_count(), 0);
    }

    #[test]
    fn a_scope_narrows_decisions_too() {
        let caps = vec!["backend".to_string()];
        let mut keep = Decision::new(Uuid::new_v4(), "Which auth lib?", vec!["a".into(), "b".into()], vec!["backend".into(), "auth".into()]);
        keep.resolve("a", "owner");
        let mut drop = Decision::new(Uuid::new_v4(), "Which billing lib?", vec!["x".into(), "y".into()], vec!["backend".into(), "billing".into()]);
        drop.resolve("x", "owner");
        let package = package_context_scoped(&[], &[keep, drop], &caps, &["auth".to_string()]);
        assert_eq!(package.included_decisions.len(), 1);
        assert!(package.included_decisions[0].contains("auth lib"));
    }

    #[test]
    fn knowledge_tagged_in_a_different_case_still_matches_the_tasks_capability() {
        // the planner emits `backend`; a person typed `Backend` / ` BACKEND `
        for typed in ["Backend", " BACKEND ", "backEnd"] {
            let package = package_context(&[tagged("RULE", &[typed])], &[], &["backend".to_string()]);
            assert_eq!(package.included_constraints, vec!["RULE".to_string()], "tag {typed:?} must match");
        }
        // and the other direction
        let package = package_context(&[tagged("RULE", &["backend"])], &[], &["Backend".to_string()]);
        assert_eq!(package.included_constraints, vec!["RULE".to_string()]);
        // positive control that case-folding didn't turn everything into a match
        let package = package_context(&[tagged("RULE", &["Design"])], &[], &["backend".to_string()]);
        assert!(package.included_constraints.is_empty());
    }

    #[test]
    fn a_decision_tagged_in_a_different_case_still_matches() {
        let mut d = Decision::new(Uuid::new_v4(), "Which lib?", vec!["a".into(), "b".into()], vec!["BACKEND".into()]);
        d.resolve("a", "owner");
        let package = package_context(&[], &[d], &["backend".to_string()]);
        assert_eq!(package.included_decisions.len(), 1);
    }

    #[test]
    fn a_knowledge_scope_ignores_case_and_whitespace_too() {
        let caps = vec!["backend".to_string()];
        let auth = tagged("AUTH", &["backend", "Auth"]);
        let billing = tagged("BILLING", &["backend", "billing"]);
        for scope in ["auth", "AUTH", "  Auth  "] {
            let package = package_context_scoped(&[auth.clone(), billing.clone()], &[], &caps, &[scope.to_string()]);
            assert_eq!(package.included_constraints, vec!["AUTH".to_string()], "scope {scope:?}");
        }
    }

    #[test]
    fn a_resolved_decision_with_a_matching_tag_is_scoped_in() {
        let mut decision = Decision::new(
            Uuid::new_v4(),
            "Which database should be used?",
            vec!["PostgreSQL".into(), "SQLite".into()],
            vec!["backend".into(), "sql".into()],
        );
        decision.resolve("PostgreSQL", "architecture-owner");

        let package = package_context(&[], &[decision], &["sql".to_string()]);
        assert_eq!(package.included_decisions.len(), 1);
        assert!(package.included_decisions[0].contains("PostgreSQL"));
    }
}
