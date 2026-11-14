use crate::Uuid;
use serde::{Deserialize, Serialize};

/// What a specialist produced for a task, and how it produced it, the
/// "what tools did it use / which model did it" half of the
/// auditability requirement.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskResult {
    pub content: serde_json::Value,
    pub produced_by_specialist_id: Uuid,
    pub produced_by_model_id: Uuid,
    pub tool_calls: Vec<String>,
}
