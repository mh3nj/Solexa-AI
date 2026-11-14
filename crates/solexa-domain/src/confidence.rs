//! Structured, multidimensional confidence.
//!
//! Rule from the spec: "Do not rely on a single arbitrary confidence
//! number... Do not treat it as absolute truth. Use confidence as a
//! routing signal." A qualitative `Low/Medium/High` scale (rather than a
//! fabricated float like `0.87`) is a deliberate choice: it resists the
//! temptation to treat these as measured probabilities when they are
//! really a specialist's self-reported signal.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfidenceLevel {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Confidence {
    pub knowledge: ConfidenceLevel,
    pub reasoning: ConfidenceLevel,
    pub evidence: ConfidenceLevel,
    pub requirement_coverage: ConfidenceLevel,
    pub tool_reliability: ConfidenceLevel,
    pub completeness: ConfidenceLevel,
}

impl Confidence {
    /// The coordinator's escalation trigger: is there any dimension weak
    /// enough that this result shouldn't be auto-accepted? Later phases
    /// will make this policy-configurable per project; Phase 1 hardcodes
    /// "any Low dimension warrants a closer look" as a sane default.
    pub fn has_weak_dimension(&self) -> bool {
        [
            self.knowledge,
            self.reasoning,
            self.evidence,
            self.requirement_coverage,
            self.tool_reliability,
            self.completeness,
        ]
        .iter()
        .any(|level| *level == ConfidenceLevel::Low)
    }
}
