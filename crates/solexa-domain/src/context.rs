//! Scoped, explainable context packages.
//!
//! Rule from the spec: "Do NOT give every specialist the entire project
//! context... The specialist should know what context was included and
//! why." A `ContextPackage` is the artifact that answers "why did this
//! specialist receive this context?" after the fact.
//!
//! Phase 1 has no Project Brain yet (that's Phase 4), so the included_*
//! lists are typically empty, but the type exists now, with an explicit
//! `excluded_summary`, so the seam is already in place rather than
//! bolted on later.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ContextPackage {
    pub included_facts: Vec<String>,
    pub included_constraints: Vec<String>,
    pub included_artifacts: Vec<String>,
    pub included_decisions: Vec<String>,
    /// Human-readable explanation of what was deliberately left out and
    /// why, not just "everything else".
    pub excluded_summary: String,
}

/// Marker the Project Brain appends to a decision that has no resolution yet.
/// Shared so the renderer can tell an open question from a ruling without
/// guessing at free text.
pub const UNRESOLVED_SUFFIX: &str = " (unresolved)";

/// Default character budget for the rendered context. A *character* estimate,
/// not a token count, local models have small windows, so it is deliberately
/// modest.
pub const DEFAULT_CONTEXT_CHARS: usize = 4000;

impl ContextPackage {
    /// Total number of included items across all four lists.
    pub fn item_count(&self) -> usize {
        self.included_facts.len()
            + self.included_constraints.len()
            + self.included_artifacts.len()
            + self.included_decisions.len()
    }

    /// Render the package as text for a model's system prompt, or `None` when
    /// it holds nothing. Order: constraints, decisions, open questions, facts,
    /// artifacts, so under a tight budget the binding material survives and
    /// the loosest material is dropped first. When items do not fit, the
    /// rendering stops at the first one that does not and says how many were
    /// omitted (never silent truncation).
    pub fn render_for_prompt(&self, max_chars: usize) -> Option<String> {
        if self.item_count() == 0 {
            return None;
        }
        let (open, ruled): (Vec<&String>, Vec<&String>) =
            self.included_decisions.iter().partition(|d| d.ends_with(UNRESOLVED_SUFFIX));

        // (section heading, text) in priority order.
        let mut entries: Vec<(&'static str, String)> = Vec::new();
        for c in &self.included_constraints {
            entries.push(("Constraints (binding):", c.clone()));
        }
        for d in ruled {
            entries.push(("Decisions (settled):", d.clone()));
        }
        for q in open {
            let q = q.strip_suffix(UNRESOLVED_SUFFIX).unwrap_or(q);
            entries.push(("Open questions (NOT decided, do not treat as settled):", q.to_string()));
        }
        for f in &self.included_facts {
            entries.push(("Facts:", f.clone()));
        }
        for a in &self.included_artifacts {
            entries.push(("Artifacts:", a.clone()));
        }

        let header = "Project context (accepted project knowledge relevant to this task):";
        let mut out = String::from(header);
        let mut current: Option<&str> = None;
        let mut rendered = 0usize;
        // Leave room for the omission note so it can never push us over.
        let note_reserve = 48;
        for (i, (heading, text)) in entries.iter().enumerate() {
            let mut piece = String::new();
            if current != Some(*heading) {
                piece.push('\n');
                piece.push_str(heading);
            }
            piece.push_str("\n- ");
            piece.push_str(text);
            let remaining = entries.len() - i;
            let reserve = if remaining > 1 { note_reserve } else { 0 };
            if out.chars().count() + piece.chars().count() + reserve > max_chars {
                break;
            }
            out.push_str(&piece);
            current = Some(*heading);
            rendered += 1;
        }
        let omitted = entries.len() - rendered;
        if omitted > 0 {
            out.push_str(&format!("\n({omitted} more item(s) omitted)"));
        }
        Some(out)
    }

    pub fn empty(reason: impl Into<String>) -> Self {
        Self {
            included_facts: Vec::new(),
            included_constraints: Vec::new(),
            included_artifacts: Vec::new(),
            included_decisions: Vec::new(),
            excluded_summary: reason.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkg() -> ContextPackage {
        ContextPackage {
            included_facts: vec!["F1".into()],
            included_constraints: vec!["C1".into()],
            included_artifacts: vec!["A1".into()],
            included_decisions: vec![
                format!("Which db? → Postgres"),
                format!("Which cache?{UNRESOLVED_SUFFIX}"),
            ],
            excluded_summary: String::new(),
        }
    }

    #[test]
    fn an_empty_package_renders_nothing() {
        assert!(ContextPackage::empty("none").render_for_prompt(4000).is_none());
    }

    #[test]
    fn every_kind_is_rendered_in_priority_order() {
        let t = pkg().render_for_prompt(4000).unwrap();
        let pos = |s: &str| t.find(s).unwrap_or_else(|| panic!("{s} missing in {t}"));
        assert!(pos("C1") < pos("Postgres"));
        assert!(pos("Postgres") < pos("Which cache?"));
        assert!(pos("Which cache?") < pos("F1"));
        assert!(pos("F1") < pos("A1"));
        assert!(!t.contains("omitted"));
    }

    #[test]
    fn an_open_decision_is_an_open_question_not_a_ruling() {
        let t = pkg().render_for_prompt(4000).unwrap();
        let q = t.find("Which cache?").unwrap();
        let heading = t.find("NOT decided").unwrap();
        let settled = t.find("Decisions (settled)").unwrap();
        assert!(heading < q && settled < heading);
        assert!(!t.contains(UNRESOLVED_SUFFIX.trim()), "marker must not leak: {t}");
    }

    #[test]
    fn a_tight_budget_keeps_constraints_and_says_what_was_omitted() {
        let mut p = pkg();
        p.included_constraints = vec!["MUST-KEEP".into()];
        p.included_facts = vec!["x".repeat(500)];
        let t = p.render_for_prompt(200).unwrap();
        assert!(t.contains("MUST-KEEP"), "{t}");
        assert!(!t.contains("xxxxx"));
        assert!(t.contains("more item(s) omitted"), "{t}");
        assert!(t.chars().count() <= 200, "{} chars", t.chars().count());
    }

    #[test]
    fn a_budget_too_small_for_anything_still_reports_the_omission() {
        let t = pkg().render_for_prompt(10).unwrap();
        assert!(t.contains("5 more item(s) omitted"), "{t}");
    }
}
