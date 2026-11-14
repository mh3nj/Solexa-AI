//! Specialist, a capability-focused AI configuration (spec: "Team /
//! Member / Specialist Model", section 3).
//!
//! A specialist is a role/capability abstraction, not a model name, the
//! same specialist can be re-pointed at a different model without losing
//! its identity, history, or permissions (spec section 11: "Model
//! Swapping").

use crate::Uuid;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
/// What a specialist or member (directly, or through a `Role`) may do.
///
/// Enforcement status, as of round 58, verified by grep, not assumed:
///
/// * `Execute`, **enforced**: a specialist can only be assigned a task with it.
/// * `Review`, **enforced**: only specialists holding it act as peer reviewers.
/// * `Approve`, **enforced**: git-sync change decisions and, when a project
///   opts in, decision/conflict resolution.
/// * `Read`, **enforced (round 58)**: a specialist without it is given no
///   Project Brain context in its prompt.
/// * `Propose`, `Edit`, **reserved, not enforced anywhere.** There is no
///   capability for them to gate yet (no tool execution, no direct artifact
///   editing). Granting or withholding them changes nothing today; do not
///   rely on them as a security boundary.
pub enum Permission {
    Read,
    Propose,
    Edit,
    Review,
    Approve,
    Execute,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EscalationPolicy {
    /// If true, any weak confidence dimension forces a Review state
    /// instead of auto-completing the task.
    pub escalate_on_weak_confidence: bool,
    /// If true, a failed deterministic verification is escalated to a
    /// human rather than silently marked Failed.
    pub escalate_on_verification_failure: bool,
    /// If true, a result that passes deterministic verification still
    /// gets a second, independent model-review pass before being
    /// accepted (spec's verification priority list, tier 7: "model-
    /// based review"). Defaults to false, deterministic checks remain
    /// sufficient for a specialist unless explicitly opted into the
    /// extra step.
    pub requires_model_review: bool,
    /// If true, a result that passes deterministic verification also
    /// gets reviewed by a second, independent *specialist* not just
    /// a different model, before being accepted (spec's verification
    /// priority list, tier 6: "specialist review", one tier *stronger*
    /// than model-based review, since the reviewer brings its own
    /// domain capabilities, permissions, and track record rather than
    /// being an arbitrary model). Runs before `requires_model_review`
    /// when both are set. Defaults to false, same rationale as
    /// `requires_model_review`.
    pub requires_specialist_review: bool,
}

impl Default for EscalationPolicy {
    fn default() -> Self {
        Self {
            escalate_on_weak_confidence: true,
            escalate_on_verification_failure: true,
            requires_model_review: false,
            requires_specialist_review: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Specialist {
    pub id: Uuid,
    pub name: String,
    pub role: String,
    pub domain: String,
    pub capabilities: Vec<String>,
    /// Preferred model, in priority order. The coordinator tries these
    /// before falling back to any model advertising the required
    /// capability.
    pub preferred_model_ids: Vec<Uuid>,
    /// Descriptive labels for the tools this specialist is meant to use.
    /// **Informational only, nothing executes or checks them** (SOLEXA has no
    /// tool execution). Stored, synced and editable so a team can record
    /// intent; do not treat it as a capability or a security boundary.
    pub tools: Vec<String>,
    /// Tags that NARROW which Project Brain knowledge/decisions this
    /// specialist is given (round 59). Empty (or only blank entries) means
    /// unrestricted. A non-empty scope keeps only items that also carry one of
    /// these tags; it can never add anything the task's capability match, the
    /// `Accepted|Verified` status filter, or the `Permission::Read` gate
    /// already excluded. A scope matching nothing means the specialist is
    /// given no project knowledge.
    pub knowledge_scope: Vec<String>,
    pub permissions: Vec<Permission>,
    /// Optional JSON Schema (as a `serde_json::Value`) the specialist's
    /// output must satisfy. `None` means free-form output.
    pub output_schema: Option<serde_json::Value>,
    pub escalation_policy: EscalationPolicy,
    /// Soft-delete flag (spec section 9's "never silently discard"
    /// philosophy, same shape as `KnowledgeStatus::Archived`): an
    /// inactive specialist is excluded from task routing and from
    /// `rank_specialists_for_capability`'s discovery results, but stays
    /// in `list_specialists` and keeps its full history, its benchmark,
    /// its past assignments, its identity. Archiving is a routing
    /// decision, not deletion.
    pub active: bool,
    /// Round 47: zero or more `Role` ids this specialist ALSO holds,
    /// on top of (never instead of) its own `permissions`, additive
    /// union, the exact same union `Member::role_ids` uses, deliberately
    /// so a specialist never loses a permission it already had from
    /// direct assignment just because a role happens to exist. See
    /// `Coordinator::specialist_has_permission` for where the union is
    /// actually computed, and solexa-domain::role's doc comment for why
    /// this is additive rather than a replacement (round 46 explicitly
    /// deferred this exact design decision to its own round).
    pub role_ids: Vec<Uuid>,
    /// Round 52: optional cap on how many tokens this specialist's main
    /// task call may generate (`GenerationRequest::max_tokens`, honored by
    /// both real providers since round 50). `None` = unbounded, exactly
    /// the pre-round-52 behavior. When set and the provider reports the
    /// output was cut off by it, the result is NOT verified as if it were
    /// complete, see `Coordinator::run_specialist`.
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Round 52: optional sampling temperature for this specialist's main
    /// task call. `None` = the runtime's default.
    #[serde(default)]
    pub temperature: Option<f32>,
}

/// Round 54: the sampling-temperature range accepted for a specialist.
/// Matches what the desktop form has enforced since round 52 (it was the
/// UI alone that did) and what both real runtimes treat as sensible.
pub const MAX_SPECIALIST_TEMPERATURE: f32 = 2.0;

impl Specialist {
    pub fn has_capability(&self, capability: &str) -> bool {
        // Round 63: ignores case and surrounding whitespace (see `tags_match`).
        self.capabilities.iter().any(|c| crate::tag::tags_match(c, capability))
    }

    /// Round 54: rejects limits that could only ever produce a broken
    /// generation request, a `max_tokens` of 0, or a temperature that is
    /// NaN/infinite, negative, or above [`MAX_SPECIALIST_TEMPERATURE`].
    /// Called when a specialist is created or edited, so the backend no
    /// longer depends on the UI having checked.
    pub fn validate_generation_limits(&self) -> Result<(), String> {
        if self.max_tokens == Some(0) {
            return Err("output limit must be at least 1 token (leave it unset for no limit)".into());
        }
        if let Some(t) = self.temperature {
            if !t.is_finite() || !(0.0..=MAX_SPECIALIST_TEMPERATURE).contains(&t) {
                return Err(format!("temperature must be a number from 0 to {MAX_SPECIALIST_TEMPERATURE} (got {t})"));
            }
        }
        Ok(())
    }

    /// Round 54: the limits actually sent to a provider. A specialist can
    /// reach storage without passing `validate_generation_limits`, one
    /// pulled from another device's git snapshot is applied as-is, so an
    /// invalid value is dropped here (treated as unset) rather than sent to
    /// the daemon, where it would fail every task this specialist runs.
    pub fn effective_max_tokens(&self) -> Option<u32> {
        self.max_tokens.filter(|n| *n > 0)
    }

    /// See [`Specialist::effective_max_tokens`].
    pub fn effective_temperature(&self) -> Option<f32> {
        self.temperature.filter(|t| t.is_finite() && (0.0..=MAX_SPECIALIST_TEMPERATURE).contains(t))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn specialist(max_tokens: Option<u32>, temperature: Option<f32>) -> Specialist {
        let mut s: Specialist = serde_json::from_value(serde_json::json!({
            "id": Uuid::new_v4(), "name": "s", "role": "r", "domain": "d", "capabilities": ["general"],
            "preferred_model_ids": [], "tools": [], "knowledge_scope": [], "permissions": ["read"],
            "output_schema": null,
            "escalation_policy": {"escalate_on_weak_confidence": false, "escalate_on_verification_failure": false,
                                  "requires_model_review": false, "requires_specialist_review": false},
            "active": true, "role_ids": []
        }))
        .unwrap();
        s.max_tokens = max_tokens;
        s.temperature = temperature;
        s
    }

    #[test]
    fn valid_generation_limits_are_accepted_including_both_boundaries_and_unset() {
        for (m, t) in [(None, None), (Some(1), Some(0.0)), (Some(u32::MAX), Some(MAX_SPECIALIST_TEMPERATURE)), (Some(256), Some(0.7))] {
            assert!(specialist(m, t).validate_generation_limits().is_ok(), "{m:?} {t:?}");
        }
    }

    #[test]
    fn limits_that_could_only_break_a_generation_request_are_rejected_with_a_reason() {
        assert!(specialist(Some(0), None).validate_generation_limits().unwrap_err().contains("at least 1 token"));
        for bad in [-0.1_f32, 2.01, 9.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let err = specialist(None, Some(bad)).validate_generation_limits().unwrap_err();
            assert!(err.contains("temperature must be a number from 0 to 2"), "{bad}: {err}");
        }
    }

    #[test]
    fn effective_limits_drop_invalid_values_but_keep_valid_ones() {
        // A specialist applied from a git snapshot never passed validation.
        let bad = specialist(Some(0), Some(f32::NAN));
        assert_eq!((bad.effective_max_tokens(), bad.effective_temperature()), (None, None));
        let out_of_range = specialist(None, Some(9.0));
        assert_eq!(out_of_range.effective_temperature(), None);
        let good = specialist(Some(64), Some(0.0));
        assert_eq!((good.effective_max_tokens(), good.effective_temperature()), (Some(64), Some(0.0)), "0.0 is a real temperature, not 'unset'");
    }

    #[test]
    fn has_capability_ignores_case_and_surrounding_whitespace() {
        let mut s: Specialist = serde_json::from_value(serde_json::json!({
            "id": uuid::Uuid::new_v4(), "name": "N", "role": "r", "domain": "d",
            "capabilities": ["Backend ", "SQL"], "preferred_model_ids": [], "tools": [],
            "knowledge_scope": [], "permissions": [], "output_schema": null,
            "escalation_policy": {"escalate_on_weak_confidence": false, "escalate_on_verification_failure": false,
                                  "requires_model_review": false, "requires_specialist_review": false},
            "active": true, "role_ids": [], "max_tokens": null, "temperature": null
        })).unwrap();
        assert!(s.has_capability("backend"), "the planner's lowercase name must match a typed `Backend `");
        assert!(s.has_capability("sql"));
        assert!(s.has_capability("  BACKEND"));
        assert!(!s.has_capability("design"));
        s.capabilities = vec!["".into()];
        assert!(!s.has_capability(""), "a blank capability matches nothing");
    }
}
