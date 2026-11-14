//! SpecialistBenchmark, measurable, historical performance per
//! specialist (Phase 8 of the roadmap: "Every specialist should
//! eventually have measurable performance... Users should be able to
//! compare specialists objectively").
//!
//! Deliberately narrow for now: just verification pass/fail counts,
//! accumulated from real task attempts (`solexa-coordinator::run_specialist`
//! records one outcome per specialist per attempt, win or lose). This
//! is a genuinely different kind of number from `Confidence`, a
//! specialist's *actual track record* is measured, not self-reported,
//! so a numeric rate is honest here in a way a fabricated confidence
//! score would not be.

use crate::Uuid;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BenchmarkOutcome {
    VerificationPassed,
    VerificationFailed,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct SpecialistBenchmark {
    pub specialist_id: Uuid,
    pub verification_passed_count: u32,
    pub verification_failed_count: u32,
}

impl SpecialistBenchmark {
    pub fn empty(specialist_id: Uuid) -> Self {
        Self { specialist_id, verification_passed_count: 0, verification_failed_count: 0 }
    }

    pub fn attempts(&self) -> u32 {
        self.verification_passed_count + self.verification_failed_count
    }

    /// The registry rule spec section 13 states outright: "must not
    /// rank purely by model size." A specialist with zero history gets
    /// a neutral 0.5, not penalized relative to one with a poor
    /// record, but not automatically outranking one with a genuinely
    /// good one either.
    pub fn success_rate(&self) -> f32 {
        let total = self.attempts();
        if total == 0 {
            0.5
        } else {
            self.verification_passed_count as f32 / total as f32
        }
    }
}
