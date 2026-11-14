use serde::{Deserialize, Serialize};

/// How a result was checked. Ordered roughly by the spec's stated
/// priority ("prefer deterministic validation... never make LLM
/// verification the default"): deterministic methods should be tried
/// before specialist/model review.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationMethod {
    DeterministicRule,
    SchemaValidation,
    StaticAnalysis,
    Test,
    EvidenceCheck,
    SpecialistReview,
    ModelReview,
    HumanReview,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationOutcome {
    pub method: VerificationMethod,
    pub passed: bool,
    pub details: String,
    pub verified_at: chrono::DateTime<chrono::Utc>,
}
