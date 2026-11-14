//! Task and its explicit state machine.
//!
//! Rule from the spec: "Do not allow arbitrary state transitions. Define
//! valid transitions explicitly." `TaskState::can_transition_to` is the
//! single source of truth for what's legal; the coordinator calls it
//! before every mutation instead of setting `task.state` directly.

use crate::{Confidence, ContextPackage, TaskResult, Uuid, VerificationOutcome};
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Created,
    Planned,
    Queued,
    Assigned,
    Running,
    Waiting,
    Blocked,
    Review,
    Revision,
    Verified,
    Completed,
    Failed,
    Cancelled,
}

impl fmt::Display for TaskState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

#[derive(Debug, thiserror::Error)]
#[error("illegal task state transition: {from} -> {to}")]
pub struct TaskStateError {
    pub from: TaskState,
    pub to: TaskState,
}

impl TaskState {
    /// The explicit transition table. Anything not listed here is
    /// forbidden. `Cancelled` and `Failed` can be reached from most
    /// active states (work can always be aborted), but never re-entered
    /// from a terminal state.
    pub fn can_transition_to(self, next: TaskState) -> bool {
        use TaskState::*;
        match (self, next) {
            (Created, Planned) => true,
            (Planned, Queued) => true,
            (Queued, Assigned) => true,
            (Queued, Blocked) => true,
            (Queued, Running) => true, // a decomposed parent task has no single assigned specialist of its own, it moves straight to Running once its subtasks are queued.
            (Assigned, Running) => true,
            (Running, Waiting) => true,
            (Running, Blocked) => true,
            (Running, Review) => true,
            (Waiting, Running) => true,
            (Blocked, Queued) => true,
            (Review, Verified) => true,
            (Review, Revision) => true,
            (Revision, Running) => true,
            (Verified, Completed) => true,
            // Work can be cancelled from any non-terminal state.
            (Created | Planned | Queued | Assigned | Running | Waiting | Blocked | Review | Revision, Cancelled) => true,
            // Execution can fail from any active state.
            (Assigned | Running | Waiting | Review | Revision, Failed) => true,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: Uuid,
    pub project_id: Uuid,
    pub title: String,
    pub description: String,
    pub state: TaskState,
    /// Member id of whoever owns this task (accountable for its outcome), a
    /// `Member`, never a specialist. Round 64: set at assignment to the live
    /// member whose `specialist_ids` includes the assigned specialist
    /// (smallest member id if several), and `None` when no member runs it.
    /// Before round 64 this held the specialist's own id; the storage
    /// migration clears those.
    pub owner_id: Option<Uuid>,
    pub assigned_specialist_id: Option<Uuid>,
    pub parent_task_id: Option<Uuid>,
    pub required_capabilities: Vec<String>,
    pub priority: u8,
    /// Round 44: optional target completion time. Unlike `priority`
    /// (always compared, defaults to 5), a task with no explicit
    /// deadline simply never wins a deadline tie-break, see
    /// `ResourceManager`'s own doc comment on why `None` has to sort
    /// as *less* urgent than any `Some`, not as "immediate."
    pub deadline: Option<chrono::DateTime<chrono::Utc>>,
    pub context_package: Option<ContextPackage>,
    pub result: Option<TaskResult>,
    pub confidence: Option<Confidence>,
    pub verification: Option<VerificationOutcome>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

impl Task {
    pub fn new(project_id: Uuid, title: impl Into<String>, description: impl Into<String>) -> Self {
        let now = chrono::Utc::now();
        Self {
            id: Uuid::new_v4(),
            project_id,
            title: title.into(),
            description: description.into(),
            state: TaskState::Created,
            owner_id: None,
            assigned_specialist_id: None,
            parent_task_id: None,
            required_capabilities: Vec::new(),
            priority: 5,
            deadline: None,
            context_package: None,
            result: None,
            confidence: None,
            verification: None,
            created_at: now,
            updated_at: now,
        }
    }

    /// The only way callers should move a task's state forward. Returns
    /// an error rather than panicking so the coordinator can turn an
    /// illegal transition into an `Escalated` event instead of crashing.
    pub fn transition_to(&mut self, next: TaskState) -> Result<(), TaskStateError> {
        if !self.state.can_transition_to(next) {
            return Err(TaskStateError { from: self.state, to: next });
        }
        self.state = next;
        self.updated_at = chrono::Utc::now();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legal_happy_path_transitions_succeed() {
        let project_id = Uuid::new_v4();
        let mut task = Task::new(project_id, "t", "d");
        for next in [
            TaskState::Planned,
            TaskState::Queued,
            TaskState::Assigned,
            TaskState::Running,
            TaskState::Review,
            TaskState::Verified,
            TaskState::Completed,
        ] {
            task.transition_to(next).unwrap();
        }
        assert_eq!(task.state, TaskState::Completed);
    }

    #[test]
    fn illegal_transition_is_rejected() {
        let project_id = Uuid::new_v4();
        let mut task = Task::new(project_id, "t", "d");
        // Created -> Running is not a legal jump; must go through
        // Planned/Queued/Assigned first.
        let err = task.transition_to(TaskState::Running).unwrap_err();
        assert_eq!(err.from, TaskState::Created);
        assert_eq!(err.to, TaskState::Running);
    }

    #[test]
    fn terminal_states_cannot_be_left() {
        assert!(!TaskState::Completed.can_transition_to(TaskState::Running));
        assert!(!TaskState::Cancelled.can_transition_to(TaskState::Running));
    }
}
