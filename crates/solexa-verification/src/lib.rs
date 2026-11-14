//! solexa-verification, deterministic-first result checking.
//!
//! Rule from the spec: "Never use another LLM as the only possible
//! verification mechanism. Where deterministic verification is
//! possible, prefer deterministic verification." This crate implements
//! a small chain of `Verifier`s that runs in priority order and stops at
//! the first applicable, decisive result, mirroring the priority list
//! in the protocol spec (deterministic rule → schema → ... → human
//! review).
//!
//! Phase 1 has two verifiers: a schema check (when the specialist
//! declares an `output_schema`) and a non-empty/well-formed fallback
//! check. Later phases add test execution, static analysis, and
//! specialist/model review without changing this crate's shape.

use serde_json::Value;
use solexa_domain::{Specialist, TaskResult, VerificationMethod, VerificationOutcome};

pub trait Verifier: Send + Sync {
    /// Returns `Some(outcome)` if this verifier had something to say
    /// about the result, or `None` if it doesn't apply (e.g. the schema
    /// verifier has nothing to check when the specialist declares no
    /// `output_schema`).
    fn verify(&self, specialist: &Specialist, result: &TaskResult) -> Option<VerificationOutcome>;
}

/// Checks the result's `content` against the specialist's declared
/// `output_schema`, if any. This is a minimal structural check (are the
/// schema's required top-level keys present?) rather than a full JSON
/// Schema implementation, enough to demonstrate "prefer deterministic
/// verification" without pulling in a JSON Schema crate for Phase 1.
pub struct SchemaVerifier;

impl Verifier for SchemaVerifier {
    fn verify(&self, specialist: &Specialist, result: &TaskResult) -> Option<VerificationOutcome> {
        let schema = specialist.output_schema.as_ref()?;
        let required = schema.get("required")?.as_array()?;

        let missing: Vec<&str> = required
            .iter()
            .filter_map(|k| k.as_str())
            .filter(|key| !has_key(&result.content, key))
            .collect();

        Some(VerificationOutcome {
            method: VerificationMethod::SchemaValidation,
            passed: missing.is_empty(),
            details: if missing.is_empty() {
                "all required output keys present".to_string()
            } else {
                format!("missing required output keys: {}", missing.join(", "))
            },
            verified_at: chrono::Utc::now(),
        })
    }
}

fn has_key(value: &Value, key: &str) -> bool {
    value.get(key).is_some()
}

/// The fallback deterministic check when no schema applies: a result
/// with genuinely empty content is never acceptable, regardless of
/// domain.
pub struct NonEmptyVerifier;

impl Verifier for NonEmptyVerifier {
    fn verify(&self, _specialist: &Specialist, result: &TaskResult) -> Option<VerificationOutcome> {
        let is_empty = match &result.content {
            Value::Null => true,
            Value::String(s) => s.trim().is_empty(),
            Value::Object(o) => o.is_empty(),
            Value::Array(a) => a.is_empty(),
            _ => false,
        };
        Some(VerificationOutcome {
            method: VerificationMethod::DeterministicRule,
            passed: !is_empty,
            details: if is_empty {
                "result content is empty".to_string()
            } else {
                "result content is non-empty".to_string()
            },
            verified_at: chrono::Utc::now(),
        })
    }
}

/// Runs verifiers in priority order and returns the first applicable
/// outcome. A later phase that adds test/static-analysis verifiers just
/// prepends them to this chain.
pub struct CompositeVerifier {
    chain: Vec<Box<dyn Verifier>>,
}

impl CompositeVerifier {
    pub fn deterministic_default() -> Self {
        Self {
            chain: vec![Box::new(SchemaVerifier), Box::new(NonEmptyVerifier)],
        }
    }

    pub fn verify(&self, specialist: &Specialist, result: &TaskResult) -> VerificationOutcome {
        for verifier in &self.chain {
            if let Some(outcome) = verifier.verify(specialist, result) {
                return outcome;
            }
        }
        // Nothing in the chain applied, Phase 1 has no specialist or
        // model review implemented yet, so this is an explicit, visible
        // "unverified" outcome rather than a silent pass.
        VerificationOutcome {
            method: VerificationMethod::DeterministicRule,
            passed: false,
            details: "no applicable verifier produced an outcome".to_string(),
            verified_at: chrono::Utc::now(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solexa_domain::{EscalationPolicy, Uuid};

    fn specialist(output_schema: Option<Value>) -> Specialist {
        Specialist {
            id: Uuid::new_v4(),
            name: "Test Specialist".into(),
            role: "tester".into(),
            domain: "testing".into(),
            capabilities: vec!["general".into()],
            preferred_model_ids: vec![],
            tools: vec![],
            knowledge_scope: vec![],
            permissions: vec![],
            output_schema,
            escalation_policy: EscalationPolicy::default(),
            active: true,
            role_ids: vec![], max_tokens: None, temperature: None,
        }
    }

    fn result(content: Value) -> TaskResult {
        TaskResult {
            content,
            produced_by_specialist_id: Uuid::new_v4(),
            produced_by_model_id: Uuid::new_v4(),
            tool_calls: vec![],
        }
    }

    #[test]
    fn non_empty_verifier_passes_populated_text() {
        let outcome = CompositeVerifier::deterministic_default()
            .verify(&specialist(None), &result(Value::String("hello".into())));
        assert!(outcome.passed);
    }

    #[test]
    fn non_empty_verifier_fails_blank_text() {
        let outcome = CompositeVerifier::deterministic_default()
            .verify(&specialist(None), &result(Value::String("   ".into())));
        assert!(!outcome.passed);
    }

    #[test]
    fn schema_verifier_catches_missing_required_key() {
        let schema = serde_json::json!({"required": ["summary"]});
        let outcome = CompositeVerifier::deterministic_default().verify(
            &specialist(Some(schema)),
            &result(serde_json::json!({"not_summary": "x"})),
        );
        assert!(!outcome.passed);
        assert!(outcome.details.contains("summary"));
    }

    #[test]
    fn schema_verifier_passes_when_required_keys_present() {
        let schema = serde_json::json!({"required": ["summary"]});
        let outcome = CompositeVerifier::deterministic_default().verify(
            &specialist(Some(schema)),
            &result(serde_json::json!({"summary": "ok"})),
        );
        assert!(outcome.passed);
    }
}
